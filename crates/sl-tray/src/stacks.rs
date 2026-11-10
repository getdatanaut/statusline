//! Stacks of pull requests the sessions' branches belong to.
//!
//! Two kinds. GitHub's own stacks (what `gh stack submit` makes) are read from
//! the `repos/{owner}/{repo}/stacks` endpoint, one call per repository: each
//! lists its PRs bottom to top with their state. A hand-made stack -- each PR
//! based on the branch of the one below -- is pieced together from the parent
//! PR each session's PR records (`GhInfo::base_pr`).
//!
//! Only a stack with a running or closed session on one of its branches is
//! shown, and only while it is open.
//!
//! Neither source says whether a PR is in the merge queue or needs a rebase,
//! so the PRs not yet merged are asked about in one GraphQL call per
//! repository: `isInMergeQueue`, and `mergeable` / `mergeStateStatus` -- a PR
//! that conflicts with its base (`CONFLICTING`, `DIRTY`) or is behind it where
//! that blocks merging (`BEHIND`) needs a rebase. The same call gives the state
//! of PRs whose state is otherwise unknown: hand-made chains' parents, and
//! closed sessions' PRs.

use std::fmt::Write as _;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::sessions::OpenSession;
use crate::sticky::Remembered;

/// A lookup is abandoned after this long; it runs on the sessions thread.
const TIMEOUT: Duration = Duration::from_secs(15);
/// A repository whose stacks could not be read is asked again this many
/// times less often than one whose could.
const FAILED_SLOWDOWN: i64 = 5;

/// Where a PR in a stack stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Merged,
    Open,
    Draft,
    /// Closed without being merged.
    Closed,
    /// In the merge queue.
    Queued,
    /// Open, and conflicting with or behind its base.
    NeedsRebase,
    /// Not known: a parent PR no session is on, in a hand-made stack.
    Unknown,
}

impl EntryState {
    /// Merged or closed: nothing more will happen to it.
    fn is_final(self) -> bool {
        matches!(self, EntryState::Merged | EntryState::Closed)
    }
}

/// Which session, if any, works on a stack's branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrySession {
    Live(String),
    Closed(String),
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackEntry {
    pub number: i64,
    pub branch: Option<String>,
    pub state: EntryState,
    pub session: EntrySession,
    /// `state` is settled: merged or closed, or GitHub has said whether it is
    /// queued or needs a rebase, now or on an earlier ask. Only a settled
    /// state may count as a change worth a notification.
    pub checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackView {
    /// GitHub's stack number; `None` for a hand-made stack.
    pub number: Option<i64>,
    /// `https://github.com/owner/repo`, for links to its PRs.
    pub repo_url: Option<String>,
    /// Bottom (nearest the base) first.
    pub entries: Vec<StackEntry>,
}

impl StackView {
    /// What identifies a stack across polls: its number and its bottom PR.
    pub fn key(&self) -> String {
        format!("stack:{:?}:{:?}", self.number, self.entries.first().map(|e| e.number))
    }

    pub fn merged(&self) -> usize {
        self.entries.iter().filter(|e| e.state == EntryState::Merged).count()
    }

    /// `Stack #746 · 6/10 merged`, or `PR chain · 3 PRs`.
    pub fn title(&self) -> String {
        match self.number {
            Some(number) => format!("Stack #{number} · {}/{} merged", self.merged(), self.entries.len()),
            None => format!("PR chain · {} PRs", self.entries.len()),
        }
    }

    /// The PR the work is at: the lowest one not merged or closed.
    pub fn next_open(&self) -> Option<&StackEntry> {
        self.entries.iter().find(|e| !e.state.is_final())
    }

    pub fn pr_url(&self, number: i64) -> Option<String> {
        self.repo_url.as_ref().map(|repo| format!("{repo}/pull/{number}"))
    }
}

// -- GitHub's stacks endpoint -----------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ApiStack {
    number: i64,
    open: bool,
    /// `https://api.github.com/repos/owner/repo/stacks/746`.
    url: String,
    pull_requests: Vec<ApiPr>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiPr {
    number: i64,
    state: String,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
    head: ApiHead,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiHead {
    #[serde(rename = "ref")]
    branch: String,
}

fn parse_api(json: &str) -> Option<Vec<ApiStack>> {
    serde_json::from_str(json).ok()
}

/// `https://github.com/owner/repo` from the endpoint's own URL for a stack.
fn repo_url(api_url: &str) -> Option<String> {
    let rest = api_url.split_once("/repos/")?.1;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    let host = api_url.split("://").nth(1)?.split('/').next()?;
    let host = if host == "api.github.com" { "github.com".to_owned() } else { host.to_owned() };
    Some(format!("https://{host}/{owner}/{repo}"))
}

fn api_state(pr: &ApiPr) -> EntryState {
    match (pr.merged_at.is_some(), pr.state.as_str(), pr.draft) {
        (true, _, _) => EntryState::Merged,
        (false, "open", true) => EntryState::Draft,
        (false, "open", false) => EntryState::Open,
        _ => EntryState::Closed,
    }
}

/// When a repository's PR states were asked, which PRs, and the answer.
type StatesRead = (i64, Vec<i64>, Option<HashMap<i64, EntryState>>);

/// Each repository's stacks, read at most once per refresh interval.
#[derive(Default)]
pub struct StackCache {
    /// By the repository's git common dir: when asked, and what came back.
    repos: HashMap<PathBuf, (i64, Option<Vec<ApiStack>>)>,
    /// Each directory's repository, found once: it does not change.
    repo_of: HashMap<String, Option<PathBuf>>,
    /// By repository: when its PRs' states were asked, which were asked
    /// about, and what came back.
    states: HashMap<PathBuf, StatesRead>,
}

impl StackCache {
    /// The git common dir of `cwd`, remembered.
    fn repo(&mut self, cwd: &str) -> Option<PathBuf> {
        self.repo_of.entry(cwd.to_owned()).or_insert_with(|| sl_core::git::common_dir(cwd).map(PathBuf::from)).clone()
    }

    /// The open stacks of `repo`, asked from `cwd`, a directory in it.
    fn open_stacks(&mut self, repo: PathBuf, cwd: &str, now_ms: i64, max_age_ms: i64) -> Vec<ApiStack> {
        if let Some((at, stacks)) = self.repos.get(&repo) {
            let age = if stacks.is_some() { max_age_ms } else { max_age_ms * FAILED_SLOWDOWN };
            if now_ms - at < age {
                return stacks.iter().flatten().filter(|s| s.open).cloned().collect();
            }
        }
        // `gh` fills in `{owner}/{repo}` from the directory's own remote.
        let fetched = crate::actions::run_for("gh", &["api", "repos/{owner}/{repo}/stacks"], cwd, TIMEOUT).ok().and_then(|json| parse_api(&json));
        // A failed read keeps what was known, as a brief outage is not every
        // stack closing.
        let stacks = fetched.or_else(|| self.repos.get(&repo).and_then(|(_, s)| s.clone()));
        let open = stacks.iter().flatten().filter(|s| s.open).cloned().collect();
        self.repos.insert(repo, (now_ms, stacks));
        open
    }

    /// The states of PRs `numbers` in `repo`, asked from `cwd`; asked again
    /// once older than `max_age_ms` or when the PRs asked about change.
    fn pr_states(&mut self, repo: PathBuf, cwd: &str, mut numbers: Vec<i64>, now_ms: i64, max_age_ms: i64) -> HashMap<i64, EntryState> {
        numbers.sort_unstable();
        numbers.dedup();
        if let Some((at, asked, states)) = self.states.get(&repo) {
            let age = if states.is_some() { max_age_ms } else { max_age_ms * FAILED_SLOWDOWN };
            if now_ms - at < age && *asked == numbers {
                return states.clone().unwrap_or_default();
            }
        }
        let previous = self.states.get(&repo).and_then(|(_, _, s)| s.clone());
        let fetched = if numbers.is_empty() { Some(HashMap::new()) } else { fetch_states(cwd, &numbers) };
        // A PR GitHub is still working out leaves its last known state in
        // place: the answer has no news about it, only a gap.
        let states = match (fetched, previous) {
            (Some(fresh), Some(mut known)) => {
                known.retain(|number, _| numbers.contains(number));
                known.extend(fresh);
                Some(known)
            }
            (fresh, known) => fresh.or(known),
        };
        self.states.insert(repo, (now_ms, numbers, states.clone()));
        states.unwrap_or_default()
    }

    /// Forget repositories and directories no session is in any more.
    fn retain(&mut self, repos: &HashSet<PathBuf>, dirs: &HashSet<&str>) {
        self.repos.retain(|repo, _| repos.contains(repo));
        self.states.retain(|repo, _| repos.contains(repo));
        self.repo_of.retain(|dir, _| dirs.contains(dir.as_str()));
    }
}

/// The stacks worth showing: GitHub's own open stacks with a live or closed
/// session on one of their branches, then hand-made chains of the sessions'
/// PRs that no GitHub stack covers.
pub fn build(live: &[OpenSession], closed: &[Remembered], cache: &mut StackCache, now_ms: i64, max_age_ms: i64) -> Vec<StackView> {
    // One directory per repository is enough to ask about its stacks.
    let all: HashSet<&str> = live.iter().map(|s| s.cwd.as_str()).chain(closed.iter().map(|c| c.cwd.as_str())).collect();
    let mut dirs: HashMap<PathBuf, &str> = HashMap::new();
    for cwd in &all {
        if let Some(repo) = cache.repo(cwd) {
            dirs.entry(repo).or_insert(cwd);
        }
    }
    let mut api: Vec<ApiStack> = Vec::new();
    for (repo, cwd) in &dirs {
        api.extend(cache.open_stacks(repo.clone(), cwd, now_ms, max_age_ms));
    }
    let mut views = assemble(live, closed, &api);

    // Each stack's PRs not yet merged, asked about per repository, from the
    // directory of a session on one of its branches.
    let cwd_of = |view: &StackView| -> Option<&str> {
        view.entries.iter().filter_map(|e| e.branch.as_deref()).find_map(|branch| {
            live.iter().find(|s| s.branch.as_deref() == Some(branch)).map(|s| s.cwd.as_str()).or_else(|| closed.iter().find(|c| c.branch.as_deref() == Some(branch)).map(|c| c.cwd.as_str()))
        })
    };
    let mut asks: HashMap<PathBuf, (&str, Vec<i64>)> = HashMap::new();
    let mut view_repo: Vec<Option<PathBuf>> = Vec::new();
    for view in &views {
        let repo = cwd_of(view).and_then(|cwd| Some((cache.repo(cwd)?, cwd)));
        if let Some((repo, cwd)) = &repo {
            let ask = asks.entry(repo.clone()).or_insert((cwd, Vec::new()));
            ask.1.extend(view.entries.iter().filter(|e| !e.state.is_final()).map(|e| e.number));
        }
        view_repo.push(repo.map(|(repo, _)| repo));
    }
    let mut known: HashMap<PathBuf, HashMap<i64, EntryState>> = HashMap::new();
    for (repo, (cwd, numbers)) in asks {
        known.insert(repo.clone(), cache.pr_states(repo, cwd, numbers, now_ms, max_age_ms));
    }
    for (view, repo) in views.iter_mut().zip(view_repo) {
        if let Some(states) = repo.and_then(|repo| known.get(&repo)) {
            apply_states(view, states);
        }
    }
    cache.retain(&dirs.keys().cloned().collect(), &all);
    views
}

/// Each PR's state as GitHub gave it, over what the stack or chain knew.
fn apply_states(view: &mut StackView, states: &HashMap<i64, EntryState>) {
    for entry in &mut view.entries {
        if let Some(state) = states.get(&entry.number) {
            entry.state = *state;
            entry.checked = true;
        }
    }
}

/// The states of `numbers`, PRs in the repository of `cwd`, in one call.
fn fetch_states(cwd: &str, numbers: &[i64]) -> Option<HashMap<i64, EntryState>> {
    let fields: String = numbers.iter().fold(String::new(), |mut fields, n| {
        write!(fields, "p{n}: pullRequest(number: {n}) {{ number state isDraft merged isInMergeQueue mergeable mergeStateStatus }} ").ok();
        fields
    });
    let query = format!("query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields}}} }}");
    // `gh` fills in `{owner}` and `{repo}` in field values too.
    let json = crate::actions::run_for("gh", &["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-f", &format!("query={query}")], cwd, TIMEOUT).ok()?;
    parse_states(&json)
}

/// The PRs' states from the GraphQL answer. A PR it could not find, or whose
/// mergeability it is still working out, is left out; an answer with no
/// repository at all is `None`.
fn parse_states(json: &str) -> Option<HashMap<i64, EntryState>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let repository = value.pointer("/data/repository")?.as_object()?;
    Some(repository.values().filter_map(|pr| Some((pr.get("number")?.as_i64()?, graphql_state(pr)?))).collect())
}

/// One PR's state: merged, closed, queued, needing a rebase, draft or open,
/// the first that holds. `None` for an open PR whose mergeability GitHub is
/// still computing (`UNKNOWN`, as after every push to its base): whether it
/// needs a rebase is not known yet, and "open" would be a guess.
fn graphql_state(pr: &serde_json::Value) -> Option<EntryState> {
    let text = |key: &str| pr.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let flag = |key: &str| pr.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false);
    Some(match text("state") {
        "MERGED" => EntryState::Merged,
        "CLOSED" => EntryState::Closed,
        "OPEN" if flag("isInMergeQueue") => EntryState::Queued,
        "OPEN" if text("mergeable") == "CONFLICTING" || matches!(text("mergeStateStatus"), "DIRTY" | "BEHIND") => EntryState::NeedsRebase,
        "OPEN" if text("mergeable") == "UNKNOWN" || text("mergeStateStatus") == "UNKNOWN" => return None,
        "OPEN" if flag("isDraft") => EntryState::Draft,
        "OPEN" => EntryState::Open,
        _ => return None,
    })
}

/// [`build`]'s pure part: `api` is every open GitHub stack read.
fn assemble(live: &[OpenSession], closed: &[Remembered], api: &[ApiStack]) -> Vec<StackView> {
    let session_on = |branch: &str| -> EntrySession {
        if let Some(s) = live.iter().find(|s| s.branch.as_deref() == Some(branch)) {
            EntrySession::Live(s.name.clone())
        } else if let Some(c) = closed.iter().find(|c| c.branch.as_deref() == Some(branch)) {
            EntrySession::Closed(c.name.clone())
        } else {
            EntrySession::None
        }
    };
    let mut out: Vec<StackView> = Vec::new();
    let mut covered: HashSet<i64> = HashSet::new();
    for stack in api {
        let entries: Vec<StackEntry> = stack
            .pull_requests
            .iter()
            .map(|pr| {
                let state = api_state(pr);
                StackEntry { number: pr.number, branch: Some(pr.head.branch.clone()), state, session: session_on(&pr.head.branch), checked: state.is_final() }
            })
            .collect();
        if entries.iter().all(|e| e.session == EntrySession::None) {
            continue;
        }
        covered.extend(entries.iter().map(|e| e.number));
        out.push(StackView { number: Some(stack.number), repo_url: repo_url(&stack.url), entries });
    }
    out.extend(chains(live, closed, &covered));
    out
}

/// A session, running or closed, as a hand-made chain sees it.
struct Link<'a> {
    branch: Option<&'a str>,
    base_pr: Option<i64>,
    url: Option<&'a str>,
    state: EntryState,
    session: EntrySession,
}

/// Hand-made stacks among the sessions, running and closed: each top PR down
/// through the parents its PR is based on, for PRs no GitHub stack covers. A
/// running session wins over a closed one on the same PR.
fn chains(live: &[OpenSession], closed: &[Remembered], covered: &HashSet<i64>) -> Vec<StackView> {
    let mut by_pr: HashMap<i64, Link> = HashMap::new();
    for c in closed {
        if let Some(pr) = c.pr_number {
            // Its state is not remembered; GitHub's is filled in later.
            by_pr.insert(pr, Link { branch: c.branch.as_deref(), base_pr: c.base_pr, url: None, state: EntryState::Unknown, session: EntrySession::Closed(c.name.clone()) });
        }
    }
    for s in live {
        let Some(gh) = s.gh.as_ref() else { continue };
        let Some(pr) = gh.pr_number else { continue };
        let state = if gh.is_draft == Some(true) { EntryState::Draft } else { EntryState::Open };
        by_pr.insert(pr, Link { branch: s.branch.as_deref(), base_pr: gh.base_pr, url: gh.url.as_deref(), state, session: EntrySession::Live(s.name.clone()) });
    }
    let parents: HashSet<i64> = by_pr.values().filter_map(|link| link.base_pr).collect();
    // Start from each top: a PR on a parent, that is no other PR's parent.
    let mut tops: Vec<i64> = by_pr.iter().filter(|(n, link)| !parents.contains(n) && link.base_pr.is_some()).map(|(n, _)| *n).collect();
    tops.sort_unstable();
    let mut out = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for top in tops {
        if covered.contains(&top) || seen.contains(&top) {
            continue;
        }
        let mut entries = Vec::new();
        let mut repo_url = None;
        let mut at = Some(top);
        while let Some(number) = at.filter(|n| !seen.contains(n) && !covered.contains(n)) {
            seen.insert(number);
            if let Some(link) = by_pr.get(&number) {
                entries.push(StackEntry { number, branch: link.branch.map(str::to_owned), state: link.state, session: link.session.clone(), checked: false });
                repo_url = repo_url.or_else(|| link.url.and_then(|url| url.split("/pull/").next()).map(str::to_owned));
                at = link.base_pr;
            } else {
                // A parent no session is on: its number is all that is known.
                entries.push(StackEntry { number, branch: None, state: EntryState::Unknown, session: EntrySession::None, checked: false });
                at = None;
            }
        }
        if entries.len() > 1 {
            entries.reverse();
            out.push(StackView { number: None, repo_url, entries });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::Activity;
    use sl_core::gh::GhInfo;

    /// Shaped as the endpoint answered for getdatanaut/datanaut on 2026-11-09.
    const API: &str = r#"[
      {"id":1,"number":746,"node_id":"x","url":"https://api.github.com/repos/getdatanaut/datanaut/stacks/746","base":{"ref":"main"},"open":true,"created_at":"2026-11-01T00:00:00Z",
       "pull_requests":[
         {"number":752,"state":"closed","draft":false,"merged_at":"2026-11-08T10:00:00Z","head":{"ref":"jakub/dn-530","sha":"a"}},
         {"number":753,"state":"open","draft":false,"merged_at":null,"head":{"ref":"jakub/dn-531","sha":"b"}},
         {"number":747,"state":"open","draft":true,"merged_at":null,"head":{"ref":"jakub/dn-478-m3","sha":"c"}},
         {"number":740,"state":"closed","draft":false,"merged_at":null,"head":{"ref":"jakub/abandoned","sha":"d"}}]},
      {"id":2,"number":729,"node_id":"y","url":"https://api.github.com/repos/getdatanaut/datanaut/stacks/729","base":{"ref":"main"},"open":false,"created_at":"2026-10-01T00:00:00Z",
       "pull_requests":[{"number":700,"state":"closed","draft":false,"merged_at":"2026-10-02T00:00:00Z","head":{"ref":"jakub/old","sha":"e"}}]},
      {"id":3,"number":763,"node_id":"z","url":"https://api.github.com/repos/getdatanaut/datanaut/stacks/763","base":{"ref":"main"},"open":true,"created_at":"2026-11-03T00:00:00Z",
       "pull_requests":[{"number":768,"state":"open","draft":false,"merged_at":null,"head":{"ref":"marc/dn-563","sha":"f"}}]}
    ]"#;

    fn live(name: &str, branch: &str, pr: Option<i64>, base_pr: Option<i64>) -> OpenSession {
        OpenSession {
            session_id: name.into(),
            name: name.into(),
            project: "datanaut".into(),
            cwd: "/nowhere".into(),
            activity: Activity::Idle,
            started_ms: 0,
            branch: Some(branch.into()),
            gh: pr.map(|n| GhInfo { pr_number: Some(n), base_pr, url: Some(format!("https://github.com/o/r/pull/{n}")), ..GhInfo::default() }),
            cache: None,
            remote_control: None,
            pid: None,
            context: None,
            linear: None,
            provider: crate::view_prefs::Source::Claude,
            last_activity_ms: None,
        }
    }

    fn open_api() -> Vec<ApiStack> {
        parse_api(API).unwrap().into_iter().filter(|s| s.open).collect()
    }

    #[test]
    fn github_stacks_are_read_bottom_first_with_their_states() {
        let stacks = assemble(&[live("M24", "jakub/dn-531", Some(753), Some(752))], &[], &open_api());
        assert_eq!(stacks.len(), 1, "only the open stack a session is on");
        let stack = &stacks[0];
        assert_eq!(stack.number, Some(746));
        assert_eq!(stack.repo_url.as_deref(), Some("https://github.com/getdatanaut/datanaut"));
        let states: Vec<(i64, EntryState)> = stack.entries.iter().map(|e| (e.number, e.state)).collect();
        assert_eq!(states, [(752, EntryState::Merged), (753, EntryState::Open), (747, EntryState::Draft), (740, EntryState::Closed)]);
        assert_eq!(stack.title(), "Stack #746 · 1/4 merged");
        assert_eq!(stack.next_open().map(|e| e.number), Some(753));
        assert_eq!(stack.entries[1].session, EntrySession::Live("M24".into()));
    }

    #[test]
    fn a_closed_sessions_branch_also_brings_its_stack_in() {
        let closed = Remembered {
            session_id: "c".into(),
            name: "M3 engine".into(),
            project: "datanaut".into(),
            cwd: "/nowhere".into(),
            branch: Some("jakub/dn-478-m3".into()),
            pr_number: Some(747),
            base_pr: None,
            last_seen_ms: 0,
            provider: crate::view_prefs::Source::Claude,
        };
        let stacks = assemble(&[], &[closed], &open_api());
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].entries[2].session, EntrySession::Closed("M3 engine".into()));
    }

    #[test]
    fn hand_made_chains_follow_base_prs_and_skip_github_stacks() {
        let sessions = [
            live("top", "t", Some(30), Some(20)),
            live("middle", "m", Some(20), Some(10)),
            live("alone", "a", Some(40), None),
            live("in-github-stack", "jakub/dn-531", Some(753), Some(752)),
        ];
        let stacks = assemble(&sessions, &[], &open_api());
        assert_eq!(stacks.len(), 2, "the GitHub stack, then one chain");
        let chain = &stacks[1];
        assert_eq!(chain.number, None);
        let numbers: Vec<i64> = chain.entries.iter().map(|e| e.number).collect();
        assert_eq!(numbers, [10, 20, 30], "bottom first, down to a parent no session is on");
        assert_eq!(chain.entries[0].state, EntryState::Unknown);
        assert_eq!(chain.title(), "PR chain · 3 PRs");
        assert_eq!(chain.pr_url(20).as_deref(), Some("https://github.com/o/r/pull/20"));
    }

    #[test]
    fn a_hand_made_chain_runs_through_closed_sessions() {
        let closed = |name: &str, branch: &str, pr: i64, base_pr: Option<i64>| Remembered {
            session_id: name.into(),
            name: name.into(),
            project: "p".into(),
            cwd: "/nowhere".into(),
            branch: Some(branch.into()),
            pr_number: Some(pr),
            base_pr,
            last_seen_ms: 0,
            provider: crate::view_prefs::Source::Claude,
        };
        let stacks = assemble(&[live("top", "t", Some(30), Some(20))], &[closed("middle", "m", 20, Some(10)), closed("bottom", "b", 10, Some(5))], &[]);
        assert_eq!(stacks.len(), 1);
        let entries: Vec<(i64, &EntrySession)> = stacks[0].entries.iter().map(|e| (e.number, &e.session)).collect();
        assert_eq!(
            entries,
            [(5, &EntrySession::None), (10, &EntrySession::Closed("bottom".into())), (20, &EntrySession::Closed("middle".into())), (30, &EntrySession::Live("top".into()))]
        );
        assert_eq!(stacks[0].entries[1].state, EntryState::Unknown, "a closed session's PR state is not remembered");
        assert_eq!(stacks[0].entries[1].branch.as_deref(), Some("b"));
        assert_eq!(stacks[0].repo_url.as_deref(), Some("https://github.com/o/r"));
    }

    /// As GitHub answered for stack #746's open PRs on 2026-11-09, trimmed.
    const GRAPHQL: &str = r#"{"data":{"repository":{
      "p753":{"number":753,"state":"OPEN","isDraft":false,"merged":false,"isInMergeQueue":false,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"},
      "p747":{"number":747,"state":"OPEN","isDraft":true,"merged":false,"isInMergeQueue":false,"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY"},
      "p769":{"number":769,"state":"OPEN","isDraft":true,"merged":false,"isInMergeQueue":false,"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"},
      "p700":{"number":700,"state":"OPEN","isDraft":false,"merged":false,"isInMergeQueue":true,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"},
      "p701":{"number":701,"state":"OPEN","isDraft":false,"merged":false,"isInMergeQueue":false,"mergeable":"MERGEABLE","mergeStateStatus":"BEHIND"},
      "p702":{"number":702,"state":"MERGED","isDraft":false,"merged":true,"isInMergeQueue":false,"mergeable":"UNKNOWN","mergeStateStatus":"UNKNOWN"},
      "p703":{"number":703,"state":"CLOSED","isDraft":false,"merged":false,"isInMergeQueue":false,"mergeable":"UNKNOWN","mergeStateStatus":"UNKNOWN"},
      "p704":null,
      "p705":{"number":705,"state":"OPEN","isDraft":false,"merged":false,"isInMergeQueue":false,"mergeable":"UNKNOWN","mergeStateStatus":"UNKNOWN"}}}}"#;

    #[test]
    fn graphql_states_map_to_queued_rebase_draft_and_the_rest() {
        let states = parse_states(GRAPHQL).expect("parses");
        assert_eq!(states.get(&753), Some(&EntryState::Open), "clean");
        assert_eq!(states.get(&747), Some(&EntryState::NeedsRebase), "a draft that conflicts needs a rebase first");
        assert_eq!(states.get(&769), Some(&EntryState::Draft), "blocked is a draft's or a review's gate, not a rebase");
        assert_eq!(states.get(&700), Some(&EntryState::Queued));
        assert_eq!(states.get(&701), Some(&EntryState::NeedsRebase), "behind where that blocks merging");
        assert_eq!(states.get(&702), Some(&EntryState::Merged));
        assert_eq!(states.get(&703), Some(&EntryState::Closed));
        assert!(!states.contains_key(&704), "a PR GitHub could not find is left as it was");
        assert!(!states.contains_key(&705), "nor one GitHub is still working out");
        assert_eq!(parse_states(r#"{"errors":[{"message":"x"}]}"#), None);
    }

    #[test]
    fn github_states_override_what_the_stack_knew() {
        let mut stack = assemble(&[live("M24", "jakub/dn-531", Some(753), Some(752))], &[], &open_api()).remove(0);
        apply_states(&mut stack, &parse_states(GRAPHQL).unwrap());
        let states: Vec<(i64, EntryState)> = stack.entries.iter().map(|e| (e.number, e.state)).collect();
        assert_eq!(states, [(752, EntryState::Merged), (753, EntryState::Open), (747, EntryState::NeedsRebase), (740, EntryState::Closed)]);
        assert_eq!(stack.next_open().map(|e| e.number), Some(753));
    }

    #[test]
    fn github_states_mark_entries_checked_and_api_finals_already_are() {
        let mut stack = assemble(&[live("M24", "jakub/dn-531", Some(753), Some(752))], &[], &open_api()).remove(0);
        let checked: Vec<(i64, bool)> = stack.entries.iter().map(|e| (e.number, e.checked)).collect();
        assert_eq!(checked, [(752, true), (753, false), (747, false), (740, true)], "merged and closed are settled; open ones wait for GitHub");
        apply_states(&mut stack, &parse_states(GRAPHQL).unwrap());
        assert!(stack.entries.iter().all(|e| e.checked));
    }

    #[test]
    fn the_repo_url_comes_from_the_endpoints_own() {
        assert_eq!(repo_url("https://api.github.com/repos/a/b/stacks/1").as_deref(), Some("https://github.com/a/b"));
        assert_eq!(repo_url("https://ghe.example.com/api/v3/repos/a/b/stacks/1").as_deref(), Some("https://ghe.example.com/a/b"));
        assert_eq!(repo_url("nonsense"), None);
    }
}


