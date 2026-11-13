//! When to ask GitHub, and asking. The tray asks, with its own token, and
//! the status line reads what it stored. Only while no tray is asking (none
//! running, or none holding a token) does the status line ask by itself, with
//! whatever token its program supplies ([`load_and_refresh`]).
//!
//! A branch is due when it was never asked about, when its answer's time is
//! up ([`Policy`]), when its answer predates [`SCHEMA`], when `origin/<branch>`
//! moved since (a push: new commits, new checks), or, when someone is looking
//! (the tray's menu just opened), when its answer is older than they would
//! want. Everything else is read from the [`Store`] without asking.
//!
//! The times follow what is likely to change: a branch is *active* for a
//! while after a push, and an open PR while its checks run soon after one;
//! active branches are asked about often, the rest seldom.

use std::collections::{BTreeMap, HashMap};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::api::{self, Failure, Head, Post};
use super::store::{Asked, Claim, Row, Store};
use super::{CiState, GhInfo, PrView, SCHEMA, is_non_default_base, worth_fetching};
use crate::git;

/// How long an ask's claim on a repository lasts, should it never finish.
const CLAIM_MS: i64 = 30_000;

/// How long each kind of answer is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// A branch pushed within `active_for_ms`: its PR is about to be opened,
    /// or its checks are running.
    pub active_ms: i64,
    pub active_for_ms: i64,
    /// An open PR otherwise: a review may still come at any time.
    pub open_ms: i64,
    /// No PR yet, and no push lately.
    pub miss_ms: i64,
    /// A merged or closed PR, which changes only if the branch is reused.
    pub final_ms: i64,
    /// After a failed ask, before the repository is asked again.
    pub failed_ms: i64,
}

/// Hard stop for the status line's refresh process.
const REFRESH_DEADLINE: Duration = Duration::from_secs(30);
/// The tray says it is asking GitHub every minute; one not heard from for
/// this long is gone, and the status line asks by itself.
pub const TRAY_GONE_MS: i64 = 3 * 60_000;

/// Checks still running count as activity for this many times
/// `active_for_ms` after the push: long enough for a slow CI, but a check
/// that never reports (forever "expected") does not keep a PR asked about
/// every half minute.
const PENDING_FOR: i64 = 4;

impl Policy {
    /// The defaults; the tray sets `active_ms` from its Settings.
    pub const BASE: Policy = Policy {
        active_ms: 30_000,
        active_for_ms: 15 * 60_000,
        open_ms: 10 * 60_000,
        miss_ms: 10 * 60_000,
        final_ms: 6 * 60 * 60_000,
        failed_ms: 2 * 60_000,
    };

    /// [`BASE`](Self::BASE) with active branches asked about
    /// every `active_ms`.
    pub fn active_every(active_ms: i64) -> Self {
        Self {
            active_ms,
            ..Self::BASE
        }
    }

    /// How long `found` is trusted, for a branch last pushed at `pushed_ms`.
    fn trust(&self, found: Option<&GhInfo>, pushed_ms: Option<i64>, now_ms: i64) -> i64 {
        let since_push = pushed_ms.map(|at| now_ms - at);
        let active = since_push.is_some_and(|age| age < self.active_for_ms);
        let checking = since_push.is_some_and(|age| age < self.active_for_ms * PENDING_FOR)
            && found
                .and_then(|info| info.ci.as_ref())
                .is_some_and(|ci| ci.state == CiState::Pending);
        match found {
            Some(info) if info.is_final() => self.final_ms,
            _ if active || checking => self.active_ms,
            None => self.miss_ms,
            Some(_) => self.open_ms,
        }
    }
}

/// Whether `row` should be asked about again, `remote_sha` being where
/// `origin/<branch>` is now. With `fresh_ms` (someone is looking), also an
/// answer older than that, unless the PR is merged or closed.
pub fn due(
    row: Option<&Row>,
    remote_sha: Option<&str>,
    now_ms: i64,
    fresh_ms: Option<i64>,
) -> bool {
    let Some(row) = row else {
        return true;
    };
    row.info.schema != SCHEMA
        || now_ms >= row.info.fresh_until_ms
        || (remote_sha.is_some() && remote_sha != row.remote_sha.as_deref())
        || fresh_ms
            .is_some_and(|fresh| !row.info.is_final() && now_ms - row.info.fetched_ms >= fresh)
}

/// What is stored for `branch`, checked out in `dir`; never asks GitHub.
/// A branch in a repository not on GitHub reads as having no PR, as
/// [`sync`] answers for it.
pub fn load(store: &Store, dir: &str, branch: &str) -> Option<GhInfo> {
    stored(store, git::github_repo(dir).as_deref(), branch)
}

/// What is stored for `branch` of `repo` (`None`: not on GitHub).
fn stored(store: &Store, repo: Option<&str>, branch: &str) -> Option<GhInfo> {
    if !worth_fetching(branch) {
        return None;
    }
    match repo {
        Some(repo) => store.get(repo, branch).map(|row| row.info),
        None => Some(GhInfo {
            schema: SCHEMA,
            ..GhInfo::default()
        }),
    }
}

/// A branch to ask about: where its `origin/<branch>` is, and whether that
/// moved since it was last asked about.
struct Due {
    branch: String,
    /// A directory it is checked out in.
    dir: String,
    /// Where it is pushed, when that is a fork: see [`git::GitHubRemote`].
    head_owner: Option<String>,
    remote_sha: Option<String>,
    pushed_now: bool,
    /// When it was last pushed: now, or as stored, or, for a branch never
    /// asked about, when its `origin/<branch>` was written.
    pushed_ms: Option<i64>,
}

impl Due {
    fn new(
        dir: &str,
        branch: &str,
        head_owner: Option<String>,
        row: Option<&Row>,
        now_ms: i64,
    ) -> Self {
        let remote_sha = git::remote_sha(dir, branch);
        let pushed_now = row.is_some_and(|row| {
            remote_sha.is_some() && remote_sha.as_deref() != row.remote_sha.as_deref()
        });
        let pushed_ms = if pushed_now {
            Some(now_ms)
        } else {
            match row {
                Some(row) => row.pushed_ms,
                None => git::remote_moved_ms(dir, branch).filter(|at| *at <= now_ms),
            }
        };
        Self {
            branch: branch.to_owned(),
            dir: dir.to_owned(),
            head_owner,
            pushed_ms,
            remote_sha,
            pushed_now,
        }
    }

    fn head(&self) -> Head<'_> {
        Head::new(&self.branch, self.head_owner.as_deref())
    }
}

/// Ask GitHub about whatever is due among `asks` (directory, branch), with
/// one request per repository, all at once, and return each directory's
/// stored answer afterwards. A repository another process is asking about
/// is left to it. A branch in a repository not on GitHub reads as having no
/// PR, as there is nothing to wait for. `fresh_ms`: see [`due`].
pub fn sync(
    store: &Store,
    post: &Post<'_>,
    asks: &[(String, String)],
    now_ms: i64,
    policy: &Policy,
    fresh_ms: Option<i64>,
) -> HashMap<String, Option<GhInfo>> {
    let resolved: Vec<(&str, &str, Option<git::GitHubRemote>)> = asks
        .iter()
        .map(|(dir, branch)| {
            let remote = worth_fetching(branch)
                .then(|| git::github_remote(dir))
                .flatten();
            (dir.as_str(), branch.as_str(), remote)
        })
        .collect();
    let mut due_by_repo: BTreeMap<&str, Vec<Due>> = BTreeMap::new();
    for (dir, branch, remote) in &resolved {
        let Some(remote) = remote else {
            continue;
        };
        let wanted = due_by_repo.entry(&remote.repo).or_default();
        let row = store.get(&remote.repo, branch);
        if !wanted.iter().any(|d| d.branch == *branch)
            && due(
                row.as_ref(),
                git::remote_sha(dir, branch).as_deref(),
                now_ms,
                fresh_ms,
            )
        {
            wanted.push(Due::new(
                dir,
                branch,
                remote.head_owner.clone(),
                row.as_ref(),
                now_ms,
            ));
        }
    }
    let claimed: Vec<(&str, Vec<Due>, Claim)> = due_by_repo
        .into_iter()
        .filter(|(_, wanted)| !wanted.is_empty())
        .filter_map(|(repo, wanted)| {
            let claim = store.claim(repo, now_ms, now_ms + CLAIM_MS)?;
            Some((repo, wanted, claim))
        })
        .collect();
    let answers: Vec<_> = std::thread::scope(|scope| {
        let running: Vec<_> = claimed
            .iter()
            .map(|(repo, wanted, _)| {
                scope.spawn(move || {
                    let heads: Vec<Head<'_>> = wanted.iter().map(Due::head).collect();
                    api::ask_branches(post, repo, &heads)
                })
            })
            .collect();
        running
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|_| Err(Failure::Request("panicked".to_owned())))
            })
            .collect()
    });
    for ((repo, wanted, claim), answer) in claimed.iter().zip(answers) {
        settle(store, post, repo, *claim, wanted, answer, now_ms, policy);
    }
    resolved
        .into_iter()
        .map(|(dir, branch, remote)| {
            let repo = remote.as_ref().map(|remote| remote.repo.as_str());
            (dir.to_owned(), stored(store, repo, branch))
        })
        .collect()
}

/// Store what GitHub said about `wanted` in `repo` (with each PR's parent:
/// the PR of the branch it targets), then give `claim` back; or, if it said
/// nothing, store nothing and hold the claim for `failed_ms`.
#[expect(
    clippy::too_many_arguments,
    reason = "one ask's whole context, used once each"
)]
fn settle(
    store: &Store,
    post: &Post<'_>,
    repo: &str,
    claim: Claim,
    wanted: &[Due],
    answer: Result<Vec<Option<PrView>>, Failure>,
    now_ms: i64,
    policy: &Policy,
) {
    let prs = match answer {
        Ok(prs) => prs,
        Err(failure) => {
            store.hold(
                repo,
                claim,
                now_ms,
                now_ms + policy.failed_ms,
                &format!("{failure:?}"),
            );
            return;
        }
    };
    let mut infos: Vec<Option<GhInfo>> = prs
        .into_iter()
        .map(|pr| pr.map(|pr| info_of(pr, now_ms)))
        .collect();
    let parents = parents(store, post, repo, wanted, &infos, now_ms, policy);
    for info in infos.iter_mut().flatten() {
        info.base_pr = info
            .base
            .as_ref()
            .and_then(|base| parents.get(base).copied().flatten());
    }
    for (due, info) in wanted.iter().zip(&infos) {
        store
            .record(
                repo,
                &due.branch,
                info.as_ref(),
                Asked {
                    remote_sha: due.remote_sha.as_deref(),
                    pushed_ms: due.pushed_now.then_some(now_ms),
                    now_ms,
                    next_ms: now_ms + policy.trust(info.as_ref(), due.pushed_ms, now_ms),
                },
            )
            .ok();
    }
    store.release(repo, claim);
}

/// The PR number of each branch an open PR among `infos` targets, other than
/// the default branch: from this same answer, else stored, else asked (and
/// then stored, as any branch's answer is, with where its `origin/<branch>`
/// is, so it does not later read as pushed). An ask that fails leaves them
/// unknown.
fn parents(
    store: &Store,
    post: &Post<'_>,
    repo: &str,
    wanted: &[Due],
    infos: &[Option<GhInfo>],
    now_ms: i64,
    policy: &Policy,
) -> HashMap<String, Option<i64>> {
    let mut known: HashMap<String, Option<i64>> = HashMap::new();
    let mut ask: Vec<&str> = Vec::new();
    for base in infos
        .iter()
        .flatten()
        .filter(|info| !info.is_final())
        .filter_map(|info| info.base.as_deref())
        .filter(|base| is_non_default_base(base) && !base.starts_with('-'))
    {
        if known.contains_key(base) || ask.contains(&base) {
            continue;
        }
        if let Some(i) = wanted.iter().position(|d| d.branch == base) {
            known.insert(
                base.to_owned(),
                infos[i].as_ref().and_then(|info| info.pr_number),
            );
        } else if let Some(row) = store
            .get(repo, base)
            .filter(|row| !due(Some(row), None, now_ms, None))
        {
            known.insert(base.to_owned(), row.info.pr_number);
        } else {
            ask.push(base);
        }
    }
    // A base is in the same repository as the branches asked about, and so
    // pushed to the same place.
    let (dir, head_owner) = wanted
        .first()
        .map(|due| (due.dir.as_str(), due.head_owner.as_deref()))
        .unwrap_or_default();
    let heads: Vec<Head<'_>> = ask.iter().map(|base| Head::new(base, head_owner)).collect();
    if let Ok(prs) = api::ask_branches(post, repo, &heads) {
        for (base, pr) in ask.iter().zip(prs) {
            let info = pr.map(|pr| info_of(pr, now_ms));
            let remote_sha = git::remote_sha(dir, base);
            store
                .record(
                    repo,
                    base,
                    info.as_ref(),
                    Asked {
                        remote_sha: remote_sha.as_deref(),
                        pushed_ms: None,
                        now_ms,
                        next_ms: now_ms + policy.trust(info.as_ref(), None, now_ms),
                    },
                )
                .ok();
            known.insert((*base).to_owned(), info.and_then(|info| info.pr_number));
        }
    }
    known
}

fn info_of(pr: PrView, now_ms: i64) -> GhInfo {
    GhInfo {
        pr_number: Some(pr.number),
        base: Some(pr.base),
        base_sha: pr.base_sha,
        base_pr: None,
        is_draft: pr.is_draft,
        ci: pr.ci,
        fetched_ms: now_ms,
        url: pr.url,
        review_decision: pr.review_decision,
        reviews: pr.reviews,
        latest_reviews: pr.latest_reviews,
        requested: pr.requested,
        assessment: pr.assessment,
        schema: SCHEMA,
        state: pr.state,
        fresh_until_ms: 0,
    }
}

/// Whether a tray holding a token said, within [`TRAY_GONE_MS`] of
/// `now_ms`, that it is asking GitHub.
pub fn tray_is_asking(store: &Store, now_ms: i64) -> bool {
    store
        .tray_seen_ms()
        .is_some_and(|seen| now_ms - seen < TRAY_GONE_MS)
}

/// How long after the status line last showed a branch the tray still asks
/// about it.
pub const SEEN_FOR_MS: i64 = 10 * 60_000;

/// The status line's entry: whatever is stored for `branch` in `dir`. While
/// a tray is asking GitHub, the branch is noted for it to ask about too
/// (the tray otherwise knows only its sessions' own directories). When none
/// is and the branch is due, a detached `statusline --gh-refresh` asks, for
/// the next render. Never waits on the network, and a failure anywhere only
/// means nothing new is shown.
pub fn load_and_refresh(dir: &str, branch: &str, now_ms: i64) -> Option<GhInfo> {
    if !worth_fetching(branch) {
        return None;
    }
    let repo = git::github_repo(dir)?;
    let store = Store::open_for_render()?;
    let row = store.get(&repo, branch);
    if tray_is_asking(&store, now_ms) {
        store.saw(&repo, branch, dir, now_ms);
    } else if due(
        row.as_ref(),
        git::remote_sha(dir, branch).as_deref(),
        now_ms,
        None,
    ) && let Some(claim) = store.claim(&repo, now_ms, now_ms + CLAIM_MS)
    {
        let spawned = std::env::current_exe().ok().and_then(|exe| {
            Command::new(exe)
                .arg("--gh-refresh")
                .arg(dir)
                .arg(branch)
                .arg(claim.0.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        });
        if spawned.is_none() {
            store.release(&repo, claim);
        }
    }
    row.map(|row| row.info)
}

/// `statusline --gh-refresh <dir> <branch> <claim>`: ask about `branch`
/// under the claim the render took, and store the answer.
pub fn run_refresh(dir: &str, branch: &str, claim: Claim, now_ms: i64) {
    std::thread::spawn(|| {
        std::thread::sleep(REFRESH_DEADLINE);
        #[expect(
            clippy::exit,
            reason = "a watchdog: a request that hangs must not keep this detached process alive"
        )]
        std::process::exit(1);
    });
    let (Some(store), Some(remote)) = (Store::open_default(), git::github_remote(dir)) else {
        return;
    };
    let row = store.get(&remote.repo, branch);
    let wanted = [Due::new(
        dir,
        branch,
        remote.head_owner,
        row.as_ref(),
        now_ms,
    )];
    let answer = api::ask_branches(&api::post, &remote.repo, &[wanted[0].head()]);
    settle(
        &store,
        &api::post,
        &remote.repo,
        claim,
        &wanted,
        answer,
        now_ms,
        &Policy::BASE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Mutex;

    /// A repository at `dir` whose origin is `github.com/o/r`, on `branch`.
    fn repo(dir: &std::path::Path, branch: &str) -> String {
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q", "-b", branch]);
        run(&["remote", "add", "origin", "git@github.com:o/r.git"]);
        dir.to_str().unwrap().to_owned()
    }

    fn answer(prs: &[Option<&str>]) -> String {
        let fields: Vec<String> = prs
            .iter()
            .enumerate()
            .map(|(i, pr)| format!(r#""b{i}":{{"nodes":[{}]}}"#, pr.unwrap_or("")))
            .collect();
        format!(r#"{{"data":{{"repository":{{{}}}}}}}"#, fields.join(","))
    }

    /// As after `git push`: `origin/<branch>` at `sha`.
    fn push(work: &std::path::Path, branch: &str, sha: &str) {
        let refs = work.join(".git/refs/remotes/origin");
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join(branch), format!("{sha}\n")).unwrap();
    }

    fn scratch() -> (tempfile::TempDir, Store, std::path::PathBuf, String) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("gh.sqlite")).unwrap();
        let work = tmp.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let dir = repo(&work, "feat");
        (tmp, store, work, dir)
    }

    const OPEN_PR: &str = r#"{"number":5,"state":"OPEN","baseRefName":"main"}"#;

    #[test]
    fn a_found_pr_is_not_asked_about_again_until_due_and_a_failure_keeps_it() {
        let (_tmp, store, _work, dir) = scratch();
        let asks = [(dir.clone(), "feat".to_owned())];
        let posts = Mutex::new(0);
        let ok = |_: &str| {
            *posts.lock().unwrap() += 1;
            Ok(answer(&[Some(OPEN_PR)]))
        };
        let policy = Policy::active_every(30_000);
        let open = policy.open_ms;
        let got = sync(&store, &ok, &asks, 0, &policy, None);
        assert_eq!(got[&dir].as_ref().and_then(|i| i.pr_number), Some(5));
        sync(&store, &ok, &asks, open - 1, &policy, None);
        assert_eq!(*posts.lock().unwrap(), 1, "not due: GitHub is not asked");
        let failing = |_: &str| Err(Failure::Request("down".into()));
        let got = sync(&store, &failing, &asks, open, &policy, None);
        assert_eq!(
            got[&dir].as_ref().and_then(|i| i.pr_number),
            Some(5),
            "a failed ask changes nothing"
        );
        assert_eq!(
            store.last_error("o/r").as_deref(),
            Some(r#"Request("down")"#)
        );
        sync(&store, &ok, &asks, open + 1_000, &policy, None);
        assert_eq!(
            *posts.lock().unwrap(),
            1,
            "held for failed_ms after a failure"
        );
        sync(&store, &ok, &asks, open + policy.failed_ms, &policy, None);
        assert_eq!(*posts.lock().unwrap(), 2);
    }

    #[test]
    fn a_pr_opened_just_after_the_push_is_found_at_the_next_active_ask() {
        let (_tmp, store, work, dir) = scratch();
        let opened = Mutex::new(false);
        let post = |_: &str| {
            Ok(if *opened.lock().unwrap() {
                answer(&[Some(OPEN_PR)])
            } else {
                answer(&[None])
            })
        };
        let policy = Policy::active_every(30_000);
        let asks = [(dir.clone(), "feat".to_owned())];
        sync(&store, &post, &asks, 0, &policy, None);
        // `git push`, then a render asks before `gh pr create` has run.
        push(&work, "feat", "abc");
        let got = sync(&store, &post, &asks, 1_000, &policy, None);
        assert_eq!(
            got[&dir].as_ref().map(|i| i.pr_number),
            Some(None),
            "asked before the PR"
        );
        *opened.lock().unwrap() = true;
        let got = sync(&store, &post, &asks, 31_000, &policy, None);
        assert_eq!(
            got[&dir].as_ref().and_then(|i| i.pr_number),
            Some(5),
            "found half a minute later, not ten"
        );
    }

    #[test]
    fn an_open_pr_is_asked_about_often_only_while_its_push_is_recent() {
        let (_tmp, store, work, dir) = scratch();
        let posts = Mutex::new(Vec::new());
        let post = |_: &str| Ok(answer(&[Some(OPEN_PR)]));
        let counting = |body: &str| {
            posts.lock().unwrap().push(body.len());
            post(body)
        };
        let policy = Policy::active_every(30_000);
        let asks = [(dir.clone(), "feat".to_owned())];
        let next = |store: &Store| store.get("o/r", "feat").unwrap().info.fresh_until_ms;
        sync(&store, &counting, &asks, 0, &policy, None);
        assert_eq!(next(&store), policy.open_ms, "no push seen: seldom");
        push(&work, "feat", "abc");
        sync(&store, &counting, &asks, 1_000, &policy, None);
        assert_eq!(next(&store), 1_000 + 30_000, "just pushed: often");
        let later = 1_000 + policy.active_for_ms;
        sync(&store, &counting, &asks, later, &policy, None);
        assert_eq!(next(&store), later + policy.open_ms, "the burst is over");
        assert_eq!(posts.lock().unwrap().len(), 3);
    }

    #[test]
    fn running_checks_keep_a_pr_active_but_not_forever() {
        let policy = Policy::BASE;
        let pending = GhInfo {
            pr_number: Some(1),
            ci: Some(super::super::Ci {
                state: CiState::Pending,
                failed: vec![],
                pending: 1,
            }),
            ..GhInfo::default()
        };
        let pushed = Some(0);
        let after = |minutes: i64| minutes * 60_000;
        assert_eq!(
            policy.trust(Some(&pending), pushed, after(30)),
            policy.active_ms
        );
        assert_eq!(
            policy.trust(Some(&pending), pushed, after(61)),
            policy.open_ms,
            "a check stuck pending does not keep it busy"
        );
        assert_eq!(
            policy.trust(Some(&pending), None, after(1)),
            policy.open_ms,
            "nor one on a PR not pushed since it was first seen"
        );
        let merged = GhInfo {
            pr_number: Some(1),
            state: Some("MERGED".into()),
            ..GhInfo::default()
        };
        assert_eq!(policy.trust(Some(&merged), pushed, 0), policy.final_ms);
        assert_eq!(policy.trust(None, None, 0), policy.miss_ms);
    }

    #[test]
    fn a_push_or_someone_looking_makes_a_branch_due() {
        let row = |next: i64, sha: Option<&str>| Row {
            info: GhInfo {
                schema: SCHEMA,
                fetched_ms: 0,
                fresh_until_ms: next,
                ..GhInfo::default()
            },
            remote_sha: sha.map(String::from),
            pushed_ms: None,
        };
        assert!(due(None, None, 0, None), "never asked");
        assert!(!due(Some(&row(100, Some("a"))), Some("a"), 50, None));
        assert!(
            due(Some(&row(100, Some("a"))), Some("b"), 50, None),
            "pushed since"
        );
        assert!(
            due(Some(&row(100, None)), Some("b"), 50, None),
            "first push"
        );
        assert!(
            !due(Some(&row(100, Some("a"))), None, 50, None),
            "no remote branch here"
        );
        assert!(due(Some(&row(100, None)), None, 100, None));
        let mut old = row(100, None);
        old.info.schema = SCHEMA - 1;
        assert!(due(Some(&old), None, 0, None), "an older schema");
        assert!(
            due(Some(&row(1_000, None)), None, 40, Some(30)),
            "the menu opened: older than 30"
        );
        assert!(
            !due(Some(&row(1_000, None)), None, 20, Some(30)),
            "fresh enough"
        );
        let mut merged = row(1_000, None);
        merged.info.pr_number = Some(1);
        merged.info.state = Some("MERGED".into());
        assert!(
            !due(Some(&merged), None, 40, Some(30)),
            "a merged PR is not asked about for a look"
        );
    }

    #[test]
    fn a_stacked_pr_learns_its_parent_from_the_branch_it_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("gh.sqlite")).unwrap();
        let work = tmp.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let dir = repo(&work, "top");
        let post = |body: &str| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            Ok(match v["variables"]["b0"].as_str().unwrap() {
                "top" => answer(&[Some(r#"{"number":8,"state":"OPEN","baseRefName":"below"}"#)]),
                _ => answer(&[Some(r#"{"number":6,"state":"OPEN","baseRefName":"main"}"#)]),
            })
        };
        let got = sync(
            &store,
            &post,
            &[(dir.clone(), "top".to_owned())],
            0,
            &Policy::BASE,
            None,
        );
        assert_eq!(got[&dir].as_ref().and_then(|i| i.base_pr), Some(6));
        assert_eq!(
            store.get("o/r", "below").and_then(|row| row.info.pr_number),
            Some(6),
            "the parent's answer is kept too"
        );
    }

    #[test]
    fn a_branch_of_a_repository_not_on_github_reads_as_no_pr_everywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("gh.sqlite")).unwrap();
        let work = tmp.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let dir = repo(&work, "feat");
        let set_url = Command::new("git")
            .args(["remote", "set-url", "origin", "git@gitlab.com:o/r.git"])
            .current_dir(&work)
            .status()
            .unwrap();
        assert!(set_url.success());
        let never = |_: &str| -> Result<String, Failure> { panic!("GitHub asked") };
        let got = sync(
            &store,
            &never,
            &[(dir.clone(), "feat".to_owned())],
            0,
            &Policy::BASE,
            None,
        );
        let miss = got[&dir].clone();
        assert_eq!(miss.as_ref().map(|i| i.pr_number), Some(None));
        assert_eq!(
            load(&store, &dir, "feat"),
            miss,
            "what a pass reads is what a round found, so the two never disagree"
        );
    }

    #[test]
    fn a_branch_first_asked_about_just_after_its_push_is_active() {
        let (_tmp, store, work, dir) = scratch();
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        push(&work, "feat", "abc");
        let policy = Policy::active_every(30_000);
        let post = |_: &str| Ok(answer(&[None]));
        sync(
            &store,
            &post,
            &[(dir, "feat".to_owned())],
            now + 1_000,
            &policy,
            None,
        );
        let row = store.get("o/r", "feat").unwrap();
        assert_eq!(
            row.info.fresh_until_ms,
            now + 1_000 + 30_000,
            "no PR yet, a moment after a push: asked again soon, not in ten minutes"
        );
    }

    #[test]
    fn a_parent_asked_about_keeps_where_its_remote_branch_is() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("gh.sqlite")).unwrap();
        let work = tmp.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let dir = repo(&work, "top");
        push(&work, "below", "b1");
        let post = |body: &str| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            Ok(match v["variables"]["b0"].as_str().unwrap() {
                "top" => answer(&[Some(r#"{"number":8,"state":"OPEN","baseRefName":"below"}"#)]),
                _ => answer(&[Some(r#"{"number":6,"state":"OPEN","baseRefName":"main"}"#)]),
            })
        };
        sync(
            &store,
            &post,
            &[(dir.clone(), "top".to_owned())],
            0,
            &Policy::BASE,
            None,
        );
        let below = store.get("o/r", "below").unwrap();
        assert_eq!(below.remote_sha.as_deref(), Some("b1"));
        assert!(
            !due(Some(&below), Some("b1"), 1, None),
            "a session on it later is not taken for a push"
        );
    }

    #[test]
    fn the_status_line_leaves_github_to_a_tray_that_said_it_is_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("gh.sqlite")).unwrap();
        let now = 1_000_000_000;
        assert!(!tray_is_asking(&store, now), "no tray ever");
        store.mark_tray(now);
        assert!(tray_is_asking(&store, now + TRAY_GONE_MS - 1));
        assert!(
            !tray_is_asking(&store, now + TRAY_GONE_MS),
            "a tray not heard from is gone"
        );
        store.clear_tray();
        assert!(!tray_is_asking(&store, now), "a tray without a token");
    }
}
