//! GitHub PR context (state, reviews, CI, the PR below it) for the status
//! line and the tray.
//!
//! What GitHub said is kept in one SQLite database ([`store`]), keyed by
//! repository and branch, and shared by every worktree and both programs:
//! the tray asks GitHub and stores the answers, and the status line reads
//! them, asking by itself only while no tray is (see [`sync`]). A
//! branch's PR, once found, is never looked for again: GitHub is asked only
//! when an answer is due ([`sync`]): an open PR every so often or after a push,
//! a branch with no PR yet less often, a merged one rarely. All due branches
//! of a repository are asked about in one GraphQL request ([`api`]). A request
//! that fails changes nothing that is stored.

pub mod api;
pub mod jobs;
pub mod store;
pub mod sync;

pub use store::Store;
pub use sync::{Policy, load, load_and_refresh, run_refresh, sync, tray_is_asking};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when `GhInfo` gains data a consumer needs; kept in each stored
/// answer, and an answer under another schema is due again. 2: reviews, review
/// requests, PR url. 3: reviews fold the full history. 4: the PR's state.
/// 5: asked over GraphQL, stored in SQLite. 6: checks of the same name in
/// different workflows kept apart, and checks past the first 100 counted.
pub const SCHEMA: u32 = 6;
/// An answer this long past when it was due again is shown as old: nothing
/// is asking GitHub about it (no token, GitHub failing, no one running).
pub const OLD_AFTER_MS: i64 = 10 * 60_000;
/// Failed jobs listed individually; the rest collapse into `+N more`.
pub const MAX_FAILED_ROWS: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailedJob {
    pub name: String,
    pub link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CiState {
    Passing,
    Pending,
    Failing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ci {
    pub state: CiState,
    pub failed: Vec<FailedJob>,
    pub pending: usize,
}

/// One reviewer's latest review (`gh`'s `latestReviews`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub author: String,
    pub state: ReviewState,
}

/// One reviewer's most recent submitted review as it was given: a comment
/// after an approval is a comment here, where [`Review`] keeps the approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatestReview {
    pub author: String,
    pub state: ReviewState,
    /// GitHub's `submittedAt`: each new review has its own.
    pub submitted_at: String,
}

/// A submitted review's state, as GitHub names it. Unsubmitted (`PENDING`)
/// reviews are never kept, so they have no variant; a state GitHub adds later
/// reads as [`Other`](ReviewState::Other).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    #[serde(other)]
    Other,
}

impl ReviewState {
    /// The state `gh` reports; `None` for a pending (unsubmitted) review.
    pub fn from_api(state: &str) -> Option<Self> {
        match state {
            "PENDING" => None,
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "COMMENTED" => Some(Self::Commented),
            "DISMISSED" => Some(Self::Dismissed),
            _ => Some(Self::Other),
        }
    }

    /// A state that stands until the reviewer changes their mind, which a
    /// later plain comment does not.
    fn is_opinion(self) -> bool {
        matches!(
            self,
            Self::Approved | Self::ChangesRequested | Self::Dismissed
        )
    }
}

/// Where a PR's review stands overall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    ChangesRequested,
}

impl GhInfo {
    /// The overall review outcome: GitHub's own `reviewDecision` when the
    /// repository has a review rule (`REVIEW_REQUIRED` is no outcome yet, however
    /// many approvals fall short of the rule), else what the latest reviews say
    /// (any outstanding change request wins over approvals).
    pub fn verdict(&self) -> Option<Verdict> {
        match self.review_decision.as_deref() {
            Some("APPROVED") => return Some(Verdict::Approved),
            Some("CHANGES_REQUESTED") => return Some(Verdict::ChangesRequested),
            Some(_) => return None,
            None => {}
        }
        let has = |state: ReviewState| self.reviews.iter().any(|review| review.state == state);
        if has(ReviewState::ChangesRequested) {
            Some(Verdict::ChangesRequested)
        } else if has(ReviewState::Approved) {
            Some(Verdict::Approved)
        } else {
            None
        }
    }
}

/// A `4/5` style score.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub value: u8,
    pub max: u8,
}

/// The risk and quality a reviewer posted as a PR comment holding a
/// `Risk | 4/5 | ...` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    pub author: String,
    pub risk: Option<Score>,
    pub quality: Option<Score>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GhInfo {
    pub pr_number: Option<i64>,
    pub base: Option<String>,
    /// Short tip commit of the base branch; absent in older cache files.
    #[serde(default)]
    pub base_sha: Option<String>,
    /// The base branch's own open PR (the parent in a stack), if it has one.
    #[serde(default)]
    pub base_pr: Option<i64>,
    /// Absent in cache files written before drafts were tracked.
    #[serde(default)]
    pub is_draft: Option<bool>,
    pub ci: Option<Ci>,
    pub fetched_ms: i64,
    #[serde(default)]
    pub url: Option<String>,
    /// `APPROVED`, `CHANGES_REQUESTED` or `REVIEW_REQUIRED`; `None` when the
    /// repository has no review rule.
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub reviews: Vec<Review>,
    /// Each reviewer's most recent review, to tell a new one from one seen
    /// before; absent in entries written before it was kept.
    #[serde(default)]
    pub latest_reviews: Vec<LatestReview>,
    /// Users and teams whose review is requested and not yet given.
    #[serde(default)]
    pub requested: Vec<String>,
    /// The latest comment carrying a risk/quality table, if any.
    #[serde(default)]
    pub assessment: Option<Assessment>,
    /// The [`SCHEMA`] this entry was written under; 0 in entries that predate it.
    #[serde(default)]
    pub schema: u32,
    /// `OPEN`, `MERGED` or `CLOSED`; absent in entries written before it was kept.
    #[serde(default)]
    pub state: Option<String>,
    /// When this answer is due to be asked about again.
    #[serde(default)]
    pub fresh_until_ms: i64,
}

impl GhInfo {
    /// A merged or closed PR does not change again.
    pub fn is_final(&self) -> bool {
        self.pr_number.is_some() && matches!(self.state.as_deref(), Some("MERGED" | "CLOSED"))
    }

    /// How old this answer is, once it is [`OLD_AFTER_MS`] past when it was
    /// due again; `None` while it is as fresh as it is meant to be. Each kind
    /// of answer ages on its own clock: a merged PR is due again only after
    /// hours, an open one after minutes.
    pub fn old_for_ms(&self, now_ms: i64) -> Option<i64> {
        (self.fresh_until_ms > 0 && now_ms - self.fresh_until_ms >= OLD_AFTER_MS)
            .then(|| now_ms - self.fetched_ms)
    }
}

fn is_default_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master")
}

/// Branches that never have a PR worth a `gh` call: the default branch and
/// a detached HEAD (`@<sha>`).
pub fn worth_fetching(branch: &str) -> bool {
    !is_default_branch(branch) && !branch.starts_with('@')
}

/// One PR as GitHub describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct PrView {
    pub number: i64,
    pub state: Option<String>,
    pub base: String,
    pub base_sha: Option<String>,
    pub is_draft: Option<bool>,
    pub url: Option<String>,
    pub review_decision: Option<String>,
    pub reviews: Vec<Review>,
    pub latest_reviews: Vec<LatestReview>,
    pub requested: Vec<String>,
    pub assessment: Option<Assessment>,
    /// From the head commit's check rollup; `None` with no checks, or when
    /// the answer did not include them.
    pub ci: Option<Ci>,
    /// Opened from a fork: not the PR of this repository's branch.
    pub cross_repository: bool,
    /// The owner of the repository the PR's branch is in.
    pub head_owner: Option<String>,
}

/// Length of the commit hashes shown.
const SHA_LEN: usize = 10;

/// [`pr_view`] of a JSON text.
pub fn parse_pr_view(json: &str) -> Option<PrView> {
    pr_view(&serde_json::from_str(json).ok()?)
}

/// The elements of `v[key]`: a GraphQL connection's `nodes`, or a plain list.
fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    let field = v.get(key);
    field
        .and_then(|f| f.get("nodes"))
        .or(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// A GraphQL `PullRequest` node (or `gh pr view --json`'s like of it).
pub fn pr_view(v: &Value) -> Option<PrView> {
    Some(PrView {
        number: v.get("number")?.as_i64()?,
        state: v.get("state").and_then(Value::as_str).map(String::from),
        base: v.get("baseRefName")?.as_str()?.to_string(),
        base_sha: v
            .get("baseRefOid")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(SHA_LEN).collect()),
        is_draft: v.get("isDraft").and_then(Value::as_bool),
        url: v.get("url").and_then(Value::as_str).map(String::from),
        review_decision: v
            .get("reviewDecision")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(String::from),
        reviews: per_reviewer(
            list(v, "reviews"),
            v.pointer("/author/login").and_then(Value::as_str),
        ),
        latest_reviews: latest_per_reviewer(
            list(v, "reviews"),
            v.pointer("/author/login").and_then(Value::as_str),
        ),
        requested: list(v, "reviewRequests")
            .iter()
            .filter_map(|r| {
                // GraphQL nests the reviewer; a user (or bot) has `login`, a
                // team has `slug` (and `name`).
                let r = r.get("requestedReviewer").unwrap_or(r);
                r.get("login")
                    .or_else(|| r.get("slug"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect(),
        assessment: latest_assessment(list(v, "comments")),
        ci: v
            .pointer("/commits/nodes/0/commit/statusCheckRollup")
            .and_then(ci_of_rollup),
        cross_repository: v
            .get("isCrossRepository")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        head_owner: v
            .pointer("/headRepositoryOwner/login")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// The head commit's checks folded by [`summarize`]. With more checks than
/// the one page read, those read are not all of them: the rollup's own state
/// (GitHub's verdict over every check) then decides whether CI passes.
fn ci_of_rollup(rollup: &Value) -> Option<Ci> {
    let contexts = rollup.pointer("/contexts/nodes")?.as_array()?;
    let mut ci = summarize(&checks_of_rollup(contexts))?;
    let more = rollup
        .pointer("/contexts/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if more {
        match rollup.get("state").and_then(Value::as_str) {
            Some("FAILURE" | "ERROR") => ci.state = CiState::Failing,
            Some("PENDING" | "EXPECTED") if ci.state == CiState::Passing => {
                ci.state = CiState::Pending;
            }
            _ => {}
        }
    }
    Some(ci)
}

/// A check as `gh pr checks` reports it: `name`, `bucket` and `link`.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub bucket: &'static str,
    pub link: Option<String>,
}

/// The rollup's check runs and commit statuses as [`Check`]s, sorted into
/// `gh`'s buckets, with only the latest run of each check kept (a re-run
/// leaves the old one in the rollup). A check run is told apart by its name,
/// workflow and event (one workflow on `push` and `pull_request` runs each job
/// twice), a commit status by its context. A node that is neither (one GitHub
/// could not resolve comes as `null`) is left out.
fn checks_of_rollup(contexts: &[Value]) -> Vec<Check> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let mut latest: Vec<((String, String, String), String, Check)> = Vec::new();
    for context in contexts {
        let is_status = context.get("context").is_some();
        if !is_status && context.get("name").and_then(Value::as_str).is_none() {
            continue;
        }
        let run = context.pointer("/checkSuite/workflowRun");
        let key = (
            text(context, if is_status { "context" } else { "name" }),
            run.and_then(|r| r.pointer("/workflow/name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            run.map(|r| text(r, "event")).unwrap_or_default(),
        );
        let (name, bucket, link, at) = if is_status {
            // A commit status: SUCCESS, FAILURE, ERROR, PENDING, EXPECTED.
            let bucket = match text(context, "state").as_str() {
                "SUCCESS" => "pass",
                "FAILURE" | "ERROR" => "fail",
                _ => "pending",
            };
            (
                text(context, "context"),
                bucket,
                text(context, "targetUrl"),
                text(context, "createdAt"),
            )
        } else {
            let bucket = if text(context, "status") == "COMPLETED" {
                match text(context, "conclusion").as_str() {
                    "SUCCESS" | "NEUTRAL" => "pass",
                    "SKIPPED" => "skipping",
                    "CANCELLED" => "cancel",
                    _ => "fail",
                }
            } else {
                "pending"
            };
            (
                text(context, "name"),
                bucket,
                text(context, "detailsUrl"),
                text(context, "startedAt"),
            )
        };
        let check = Check {
            name: name.clone(),
            bucket,
            link: (!link.is_empty()).then_some(link),
        };
        match latest.iter_mut().find(|(k, _, _)| *k == key) {
            Some(slot) if slot.1 <= at => (slot.1, slot.2) = (at, check),
            Some(_) => {}
            None => latest.push((key, at, check)),
        }
    }
    latest.into_iter().map(|(_, _, check)| check).collect()
}

/// The newest comment whose body holds a risk and/or quality score table.
fn latest_assessment(comments: &[Value]) -> Option<Assessment> {
    comments.iter().rev().find_map(|c| {
        let author = c.pointer("/author/login").and_then(Value::as_str)?;
        let (risk, quality) = parse_scores(c.get("body").and_then(Value::as_str)?);
        (risk.is_some() || quality.is_some()).then(|| Assessment {
            author: author.to_owned(),
            risk,
            quality,
        })
    })
}

/// `(risk, quality)` from the rows of a markdown table: `| Risk | 4/5 | note |`.
pub fn parse_scores(body: &str) -> (Option<Score>, Option<Score>) {
    let (mut risk, mut quality) = (None, None);
    for line in body.lines().map(str::trim).filter(|l| l.starts_with('|')) {
        let mut cells = line
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().trim_matches(['*', '`']).trim());
        let (Some(label), Some(score)) = (cells.next(), cells.next()) else {
            continue;
        };
        let Some((value, max)) = score.split_once('/') else {
            continue;
        };
        let (Ok(value), Ok(max)) = (value.trim().parse::<u8>(), max.trim().parse::<u8>()) else {
            continue;
        };
        let score = Some(Score { value, max });
        match label.to_ascii_lowercase().as_str() {
            "risk" => risk = score,
            "quality" => quality = score,
            _ => {}
        }
    }
    (risk, quality)
}

/// Each reviewer's standing, from the full review history in the order GitHub
/// shows it: their latest *approval, change request or dismissal* wins over any
/// later plain comment (`latestReviews` would show the comment and hide the
/// approval). A reviewer who only commented shows as commented. The PR's own
/// author and unsubmitted (pending) reviews are left out, as in GitHub's
/// reviewer list.
fn per_reviewer(reviews: &[Value], pr_author: Option<&str>) -> Vec<Review> {
    let mut ordered: Vec<&Value> = reviews.iter().collect();
    ordered.sort_by_key(|r| r.get("submittedAt").and_then(Value::as_str).unwrap_or(""));
    // First-seen order, so the list does not shuffle between refreshes.
    let mut standing: Vec<(String, Option<ReviewState>, ReviewState)> = Vec::new();
    for review in ordered {
        let (Some(author), Some(state)) = (
            review.pointer("/author/login").and_then(Value::as_str),
            review
                .get("state")
                .and_then(Value::as_str)
                .and_then(ReviewState::from_api),
        ) else {
            continue;
        };
        if Some(author) == pr_author {
            continue;
        }
        let idx = if let Some(idx) = standing.iter().position(|(a, _, _)| a == author) {
            idx
        } else {
            standing.push((author.to_owned(), None, state));
            standing.len() - 1
        };
        standing[idx].2 = state;
        if state.is_opinion() {
            standing[idx].1 = Some(state);
        }
    }
    standing
        .into_iter()
        .map(|(author, opinion, last)| Review {
            author,
            state: opinion.unwrap_or(last),
        })
        .collect()
}

/// Each reviewer's most recent submitted review, in the order they first
/// reviewed. The PR's own author and unsubmitted reviews are left out, as in
/// [`per_reviewer`].
fn latest_per_reviewer(reviews: &[Value], pr_author: Option<&str>) -> Vec<LatestReview> {
    let mut ordered: Vec<&Value> = reviews.iter().collect();
    ordered.sort_by_key(|r| r.get("submittedAt").and_then(Value::as_str).unwrap_or(""));
    let mut latest: Vec<LatestReview> = Vec::new();
    for review in ordered {
        let (Some(author), Some(state), Some(at)) = (
            review.pointer("/author/login").and_then(Value::as_str),
            review
                .get("state")
                .and_then(Value::as_str)
                .and_then(ReviewState::from_api),
            review.get("submittedAt").and_then(Value::as_str),
        ) else {
            continue;
        };
        if Some(author) == pr_author {
            continue;
        }
        let review = LatestReview {
            author: author.to_owned(),
            state,
            submitted_at: at.to_owned(),
        };
        match latest.iter_mut().find(|r| r.author == author) {
            Some(slot) => *slot = review,
            None => latest.push(review),
        }
    }
    latest
}

/// The workflow run and job ids in a check's link, e.g.
/// `https://github.com/o/r/actions/runs/40729184900/job/122960538826`. Checks
/// from other apps link elsewhere (`.../runs/<id>`) and have neither.
pub fn run_and_job_ids(link: &str) -> Option<(u64, u64)> {
    let rest = link.split("/actions/runs/").nth(1)?;
    let (run, job) = rest.split_once("/job/")?;
    let job: String = job.chars().take_while(char::is_ascii_digit).collect();
    Some((run.parse().ok()?, job.parse().ok()?))
}

/// `gh pr checks --json name,bucket,link` as [`Check`]s, folded by
/// [`summarize`]. `None` for no checks, or for text that is not that list.
pub fn summarize_checks(json: &str) -> Option<Ci> {
    let v: Value = serde_json::from_str(json).ok()?;
    let checks: Vec<Check> = v
        .as_array()?
        .iter()
        .map(|check| {
            let text = |key| check.get(key).and_then(Value::as_str);
            Check {
                name: text("name").unwrap_or("?").to_owned(),
                bucket: match text("bucket") {
                    Some("fail") => "fail",
                    Some("pending") => "pending",
                    _ => "pass",
                },
                link: text("link").filter(|l| !l.is_empty()).map(String::from),
            }
        })
        .collect();
    summarize(&checks)
}

/// Fold checks into one verdict. Buckets are `pass`, `fail`, `pending`,
/// `skipping` and `cancel`; only `fail` is red and only `pending` is in
/// flight. `None` when there are no checks at all.
pub fn summarize(checks: &[Check]) -> Option<Ci> {
    if checks.is_empty() {
        return None;
    }
    let failed: Vec<FailedJob> = checks
        .iter()
        .filter(|c| c.bucket == "fail")
        .map(|c| FailedJob {
            name: c.name.clone(),
            link: c.link.clone(),
        })
        .collect();
    let pending = checks.iter().filter(|c| c.bucket == "pending").count();
    let state = if !failed.is_empty() {
        CiState::Failing
    } else if pending > 0 {
        CiState::Pending
    } else {
        CiState::Passing
    };
    Some(Ci {
        state,
        failed,
        pending,
    })
}

/// Whether a PR targets something other than the default branch -- the only
/// case where the target is worth a segment.
pub fn is_non_default_base(base: &str) -> bool {
    !is_default_branch(base)
}

#[cfg(test)]
mod tests {
    #[test]
    fn checks_are_kept_apart_by_workflow_and_event_and_only_reruns_collapse() {
        let run = |name: &str, workflow: &str, event: &str, at: &str, conclusion: &str| {
            serde_json::json!({"name": name, "status": "COMPLETED", "conclusion": conclusion,
                "startedAt": at, "checkSuite": {"workflowRun": {"event": event, "workflow": {"name": workflow}}}})
        };
        let contexts = [
            run("test", "ci", "push", "2026-01-01T10:00:00Z", "FAILURE"),
            run(
                "test",
                "ci",
                "pull_request",
                "2026-01-01T10:00:05Z",
                "SUCCESS",
            ),
            run("lint", "ci", "push", "2026-01-01T10:00:00Z", "FAILURE"),
            run("lint", "ci", "push", "2026-01-01T10:05:00Z", "SUCCESS"),
            serde_json::Value::Null,
            serde_json::json!({}),
        ];
        let ci = super::summarize(&super::checks_of_rollup(&contexts)).unwrap();
        assert_eq!(ci.state, super::CiState::Failing, "{ci:?}");
        assert_eq!(
            ci.failed.len(),
            1,
            "the push run of `test`; `lint` was re-run"
        );
        assert_eq!(ci.pending, 0, "a null node is not a check");
    }

    #[test]
    fn past_one_page_of_checks_the_rollup_state_decides() {
        let rollup = |state: &str, more: bool| {
            serde_json::json!({"state": state, "contexts": {"pageInfo": {"hasNextPage": more},
                "nodes": [{"name": "a", "status": "COMPLETED", "conclusion": "SUCCESS"}]}})
        };
        let state = |v| super::ci_of_rollup(&v).unwrap().state;
        assert_eq!(state(rollup("FAILURE", true)), super::CiState::Failing);
        assert_eq!(state(rollup("PENDING", true)), super::CiState::Pending);
        assert_eq!(
            state(rollup("FAILURE", false)),
            super::CiState::Passing,
            "all read"
        );
    }

    #[test]
    fn an_answer_is_old_once_well_past_when_it_was_due_again() {
        let info = super::GhInfo {
            fetched_ms: 1_000,
            fresh_until_ms: 2_000,
            ..super::GhInfo::default()
        };
        assert_eq!(info.old_for_ms(2_000 + super::OLD_AFTER_MS - 1), None);
        assert_eq!(
            info.old_for_ms(2_000 + super::OLD_AFTER_MS),
            Some(1_000 + super::OLD_AFTER_MS)
        );
        assert_eq!(
            super::GhInfo::default().old_for_ms(i64::MAX),
            None,
            "never stored"
        );
    }

    #[test]
    fn a_merged_or_closed_pr_is_final_and_a_miss_or_open_one_is_not() {
        let info = |pr: Option<i64>, state: Option<&str>| GhInfo {
            pr_number: pr,
            state: state.map(String::from),
            ..GhInfo::default()
        };
        assert!(info(Some(1), Some("MERGED")).is_final());
        assert!(info(Some(1), Some("CLOSED")).is_final());
        assert!(!info(Some(1), Some("OPEN")).is_final());
        assert!(
            !info(Some(1), None).is_final(),
            "an entry from before the state was kept"
        );
        assert!(!info(None, Some("MERGED")).is_final());
    }

    #[test]
    fn the_state_is_read_from_the_pr() {
        let pr = parse_pr_view(r#"{"number":3,"state":"MERGED","baseRefName":"main"}"#).unwrap();
        assert_eq!(pr.state.as_deref(), Some("MERGED"));
    }

    use super::*;
    #[test]
    fn review_states_round_trip_as_github_names_them_and_pending_is_none() {
        assert_eq!(ReviewState::from_api("PENDING"), None);
        assert_eq!(
            ReviewState::from_api("DISMISSED"),
            Some(ReviewState::Dismissed)
        );
        assert_eq!(
            ReviewState::from_api("SOMETHING_NEW"),
            Some(ReviewState::Other)
        );
        let review = Review {
            author: "a".into(),
            state: ReviewState::ChangesRequested,
        };
        let json = serde_json::to_string(&review).unwrap();
        assert_eq!(
            json, r#"{"author":"a","state":"CHANGES_REQUESTED"}"#,
            "the cache keeps GitHub's names"
        );
        let later: Review =
            serde_json::from_str(r#"{"author":"a","state":"SOMETHING_NEW"}"#).unwrap();
        assert_eq!(
            later.state,
            ReviewState::Other,
            "an unknown state still reads"
        );
    }

    #[test]
    fn the_verdict_follows_the_review_rule_else_the_reviews() {
        let review = |state| Review {
            author: "a".into(),
            state,
        };
        let info = |decision: Option<&str>, reviews: Vec<Review>| GhInfo {
            review_decision: decision.map(String::from),
            reviews,
            ..GhInfo::default()
        };
        assert_eq!(
            info(
                Some("APPROVED"),
                vec![review(ReviewState::ChangesRequested)]
            )
            .verdict(),
            Some(Verdict::Approved),
            "the rule wins"
        );
        assert_eq!(
            info(Some("CHANGES_REQUESTED"), vec![]).verdict(),
            Some(Verdict::ChangesRequested)
        );
        assert_eq!(
            info(Some("REVIEW_REQUIRED"), vec![review(ReviewState::Approved)]).verdict(),
            None,
            "one approval of two required is not approved"
        );
        assert_eq!(
            info(None, vec![review(ReviewState::Approved)]).verdict(),
            Some(Verdict::Approved),
            "no review rule: the reviews say"
        );
        assert_eq!(
            info(
                None,
                vec![
                    review(ReviewState::Approved),
                    review(ReviewState::ChangesRequested)
                ]
            )
            .verdict(),
            Some(Verdict::ChangesRequested),
            "a change request wins"
        );
        assert_eq!(
            info(None, vec![review(ReviewState::Commented)]).verdict(),
            None
        );
    }

    #[test]
    fn scores_are_read_from_a_markdown_table_and_the_latest_comment_wins() {
        use super::*;

        let table = "|  | Score | Note |\n| --- | --- | --- |\n| Risk | 4/5 | a, b |\n| **Quality** | 3/5 | c |\n";
        assert_eq!(
            parse_scores(table),
            (
                Some(Score { value: 4, max: 5 }),
                Some(Score { value: 3, max: 5 })
            )
        );
        assert_eq!(parse_scores("just prose, 4/5 stars"), (None, None));
        let comments: Vec<Value> = serde_json::from_str(
            r#"[{"author":{"login":"claude"},"body":"| Risk | 2/5 | old |"},
                {"author":{"login":"bot"},"body":"| Quality | 5/5 | new |"},
                {"author":{"login":"ann"},"body":"lgtm"}]"#,
        )
        .unwrap();
        let found = latest_assessment(&comments).unwrap();
        assert_eq!(
            (found.author.as_str(), found.risk, found.quality),
            ("bot", None, Some(Score { value: 5, max: 5 }))
        );
    }

    #[test]
    fn pr_view_yields_number_base_short_sha_and_draft() {
        let pr = parse_pr_view(
            r#"{"number":42,"baseRefName":"develop","baseRefOid":"1adc44e1c5aabbccddeeff00112233445566","isDraft":true}"#,
        )
        .unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.base, "develop");
        assert_eq!(pr.base_sha.as_deref(), Some("1adc44e1c5"));
        assert_eq!(pr.is_draft, Some(true));
        let bare = parse_pr_view(r#"{"number":42,"baseRefName":"develop"}"#).unwrap();
        assert_eq!((bare.base_sha, bare.is_draft), (None, None));
        assert_eq!(parse_pr_view("not json"), None);
        assert_eq!(parse_pr_view(r#"{"number":42}"#), None);
    }

    #[test]
    fn pr_view_reads_reviews_requests_and_url() {
        let pr = parse_pr_view(
            r#"{"number":7,"baseRefName":"main","url":"https://github.com/o/r/pull/7","reviewDecision":"CHANGES_REQUESTED",
                "author":{"login":"zed"},
                "reviews":[{"author":{"login":"ann"},"state":"APPROVED","submittedAt":"2026-01-01T10:00:00Z","body":"x"},
                           {"author":{"login":"bob"},"state":"CHANGES_REQUESTED","submittedAt":"2026-01-01T11:00:00Z"},
                           {"state":"COMMENTED"}],
                "reviewRequests":[{"__typename":"User","login":"cy"},{"__typename":"Team","name":"Core","slug":"core"}]}"#,
        )
        .unwrap();
        assert_eq!(pr.url.as_deref(), Some("https://github.com/o/r/pull/7"));
        assert_eq!(pr.review_decision.as_deref(), Some("CHANGES_REQUESTED"));
        assert_eq!(
            pr.reviews,
            vec![
                Review {
                    author: "ann".into(),
                    state: ReviewState::Approved
                },
                Review {
                    author: "bob".into(),
                    state: ReviewState::ChangesRequested
                },
            ]
        );
        assert_eq!(pr.requested, ["cy", "core"]);
        let none =
            parse_pr_view(r#"{"number":7,"baseRefName":"main","reviewDecision":""}"#).unwrap();
        assert_eq!(
            (none.review_decision, none.reviews, none.requested),
            (None, vec![], vec![])
        );
    }

    #[test]
    fn a_later_comment_does_not_hide_an_approval_or_a_change_request() {
        // PR #735: marbemac approved, then left a comment review 27 seconds later.
        let pr = parse_pr_view(
            r#"{"number":735,"baseRefName":"main","author":{"login":"P0lip"},
                "reviews":[
                  {"author":{"login":"claude"},"state":"COMMENTED","submittedAt":"2026-10-28T14:36:55Z"},
                  {"author":{"login":"claude"},"state":"COMMENTED","submittedAt":"2026-10-28T14:36:57Z"},
                  {"author":{"login":"P0lip"},"state":"COMMENTED","submittedAt":"2026-10-28T14:42:34Z"},
                  {"author":{"login":"marbemac"},"state":"APPROVED","submittedAt":"2026-10-28T19:58:50Z"},
                  {"author":{"login":"marbemac"},"state":"COMMENTED","submittedAt":"2026-10-28T19:59:17Z"},
                  {"author":{"login":"dan"},"state":"PENDING","submittedAt":"2026-10-28T20:00:00Z"}]}"#,
        )
        .unwrap();
        let standing: Vec<(&str, ReviewState)> = pr
            .reviews
            .iter()
            .map(|r| (r.author.as_str(), r.state))
            .collect();
        assert_eq!(
            standing,
            [
                ("claude", ReviewState::Commented),
                ("marbemac", ReviewState::Approved)
            ],
            "the author and a pending draft are not reviewers"
        );
        // A change request outlives a later comment too, and a later approval replaces it.
        let reviews = |json: &str| {
            parse_pr_view(&format!(
                r#"{{"number":1,"baseRefName":"main","reviews":{json}}}"#
            ))
            .unwrap()
            .reviews
        };
        let r = reviews(
            r#"[{"author":{"login":"a"},"state":"CHANGES_REQUESTED","submittedAt":"1"},{"author":{"login":"a"},"state":"COMMENTED","submittedAt":"2"}]"#,
        );
        assert_eq!(
            r[0].state,
            ReviewState::ChangesRequested,
            "a change request outlives a later comment"
        );
        let r = reviews(
            r#"[{"author":{"login":"a"},"state":"CHANGES_REQUESTED","submittedAt":"1"},{"author":{"login":"a"},"state":"APPROVED","submittedAt":"2"}]"#,
        );
        assert_eq!(
            r[0].state,
            ReviewState::Approved,
            "a later approval replaces a change request"
        );
    }

    #[test]
    fn each_reviewers_latest_review_is_kept_as_given_with_its_time() {
        let pr = parse_pr_view(
            r#"{"number":735,"baseRefName":"main","author":{"login":"P0lip"},
                "reviews":[
                  {"author":{"login":"marbemac"},"state":"APPROVED","submittedAt":"2026-10-28T19:58:50Z"},
                  {"author":{"login":"P0lip"},"state":"COMMENTED","submittedAt":"2026-10-28T19:59:00Z"},
                  {"author":{"login":"marbemac"},"state":"COMMENTED","submittedAt":"2026-10-28T19:59:17Z"},
                  {"author":{"login":"dan"},"state":"PENDING","submittedAt":"2026-10-28T20:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(
            pr.latest_reviews,
            [LatestReview {
                author: "marbemac".into(),
                state: ReviewState::Commented,
                submitted_at: "2026-10-28T19:59:17Z".into(),
            }],
            "the comment after the approval; not the PR's author, not a pending draft"
        );
    }

    #[test]
    fn run_and_job_ids_come_from_actions_links_only() {
        assert_eq!(
            run_and_job_ids("https://github.com/o/r/actions/runs/40729184900/job/122960538826"),
            Some((40_729_184_900, 122_960_538_826))
        );
        assert_eq!(
            run_and_job_ids("https://github.com/o/r/actions/runs/1/job/2?pr=3"),
            Some((1, 2))
        );
        assert_eq!(
            run_and_job_ids("https://github.com/o/r/runs/122961035283"),
            None
        );
        assert_eq!(
            run_and_job_ids("https://github.com/o/r/actions/runs/5"),
            None
        );
    }

    #[test]
    fn failing_checks_list_the_failed_jobs_with_links() {
        let ci = summarize_checks(
            r#"[{"name":"lint","bucket":"fail","link":"https://x/1"},
                {"name":"build","bucket":"pass","link":"https://x/2"},
                {"name":"e2e","bucket":"fail","link":""},
                {"name":"deploy","bucket":"pending","link":"https://x/4"}]"#,
        )
        .unwrap();
        assert_eq!(ci.state, CiState::Failing);
        assert_eq!(ci.pending, 1);
        assert_eq!(
            ci.failed,
            vec![
                FailedJob {
                    name: "lint".into(),
                    link: Some("https://x/1".into())
                },
                FailedJob {
                    name: "e2e".into(),
                    link: None
                },
            ]
        );
    }

    #[test]
    fn pending_beats_passing_and_skipped_or_cancelled_are_neutral() {
        let pending =
            summarize_checks(r#"[{"name":"a","bucket":"pass"},{"name":"b","bucket":"pending"}]"#)
                .unwrap();
        assert_eq!(pending.state, CiState::Pending);
        let green = summarize_checks(r#"[{"name":"a","bucket":"pass"},{"name":"b","bucket":"skipping"},{"name":"c","bucket":"cancel"}]"#).unwrap();
        assert_eq!(green.state, CiState::Passing);
    }

    #[test]
    fn no_checks_means_no_verdict() {
        assert_eq!(summarize_checks("[]"), None);
        assert_eq!(summarize_checks("garbage"), None);
    }

    #[test]
    fn default_and_detached_branches_are_not_fetched() {
        assert!(!worth_fetching("main"));
        assert!(!worth_fetching("master"));
        assert!(!worth_fetching("@abc1234"));
        assert!(worth_fetching("feat/x"));
    }
}
