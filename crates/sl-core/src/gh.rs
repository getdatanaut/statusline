//! GitHub PR context (target branch, CI status) for the status line.
//!
//! `gh` takes around a second per call, far too slow to run inside a render.
//! So a render only ever reads a per-branch cache file; when that file is
//! stale it spawns a detached `statusline --gh-refresh` that runs `gh` and
//! rewrites the cache for the next render. The first render on a branch
//! therefore shows nothing, and data lags by at most one refresh.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Bumped when `GhInfo` gains data a consumer needs. An entry written by an
/// older build (whose status line shares this cache) lacks it, so it is
/// treated as stale whatever its age. 2: reviews, review requests, PR url.
/// 3: reviews fold the full history (a later comment no longer hides an
/// approval).
const SCHEMA: u32 = 4;
/// A cache entry older than this triggers a background refresh.
const STALE_AFTER_MS: i64 = 30_000;
/// Minimum gap between refresh spawns for one branch, so a slow `gh` is not
/// stacked up by every render in between.
const LOCK_FOR: Duration = Duration::from_secs(20);
/// Hard stop for a refresh process, in case `gh` hangs on the network.
const REFRESH_DEADLINE: Duration = Duration::from_secs(30);
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
    /// Set by a reader, never stored: this "no PR" answer is old enough that
    /// the PR may exist by now, and has not been asked about again yet.
    #[serde(skip)]
    pub unconfirmed: bool,
}

impl GhInfo {
    /// A merged or closed PR does not change again.
    pub fn is_final(&self) -> bool {
        self.pr_number.is_some() && matches!(self.state.as_deref(), Some("MERGED" | "CLOSED"))
    }
}

/// How long a merged or closed PR is trusted before the branch is asked about
/// once more (a branch name can be reused for a new PR).
const FINAL_RECHECK_MS: i64 = 60 * 60 * 1000;

fn is_default_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master")
}

/// Branches that never have a PR worth a `gh` call: the default branch and
/// a detached HEAD (`@<sha>`).
pub fn worth_fetching(branch: &str) -> bool {
    !is_default_branch(branch) && !branch.starts_with('@')
}

/// Stable per-(directory, branch) file stem. FNV-1a, not `DefaultHasher`,
/// whose output the standard library does not promise to keep.
fn cache_key(dir: &str, branch: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.bytes().chain([0]).chain(branch.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn default_cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("gh")
}

fn cache_path(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir.join(format!("{key}.json"))
}

fn lock_path(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir.join(format!("{key}.lock"))
}

/// Render-side entry: return whatever is cached for this branch and, if it is
/// missing or stale, kick off a background refresh. Never blocks on `gh`.
pub fn load_and_refresh(cache_dir: &Path, dir: &str, branch: &str, now_ms: i64) -> Option<GhInfo> {
    if !worth_fetching(branch) {
        return None;
    }
    let key = cache_key(dir, branch);
    let cached: Option<GhInfo> = crate::paths::read_json(&cache_path(cache_dir, &key));
    let stale = cached
        .as_ref()
        .is_none_or(|c| c.schema != SCHEMA || now_ms - c.fetched_ms >= STALE_AFTER_MS);
    if stale {
        spawn_refresh(cache_dir, &key, dir, branch);
    }
    cached
}

fn spawn_refresh(cache_dir: &Path, key: &str, dir: &str, branch: &str) {
    let lock = lock_path(cache_dir, key);
    let recent = fs::metadata(&lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < LOCK_FOR);
    if recent {
        return;
    }
    fs::create_dir_all(cache_dir).ok();
    if fs::write(&lock, b"").is_err() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Best effort: a refresh that cannot start leaves the cache as it was.
    Command::new(exe)
        .arg("--gh-refresh")
        .arg(dir)
        .arg(branch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}

/// `statusline --gh-refresh <dir> <branch>`: run `gh`, rewrite the cache.
pub fn run_refresh(cache_dir: &Path, dir: &str, branch: &str, now_ms: i64) {
    std::thread::spawn(|| {
        std::thread::sleep(REFRESH_DEADLINE);
        #[expect(
            clippy::exit,
            reason = "a watchdog: a `gh` that hangs must not keep this detached process alive"
        )]
        std::process::exit(1);
    });
    store(cache_dir, dir, branch, &fetch(dir, now_ms));
}

/// Whatever is cached for `branch` in `dir`, however old, without asking `gh`:
/// for a caller that must not touch the network.
pub fn load_cached(cache_dir: &Path, dir: &str, branch: &str) -> Option<GhInfo> {
    if !worth_fetching(branch) {
        return None;
    }
    let cached: GhInfo = crate::paths::read_json(&cache_path(cache_dir, &cache_key(dir, branch)))?;
    (cached.schema == SCHEMA).then_some(cached)
}

/// Cached info for `branch` if it is younger than `max_age_ms(cached)`, else a
/// synchronous `gh` round trip that rewrites the cache. For callers already on
/// a background thread (the tray); a render must use [`load_and_refresh`].
pub fn load_or_fetch(
    cache_dir: &Path,
    dir: &str,
    branch: &str,
    now_ms: i64,
    max_age_ms: impl Fn(&GhInfo) -> i64,
) -> Option<GhInfo> {
    if !worth_fetching(branch) {
        return None;
    }
    let key = cache_key(dir, branch);
    let cached: Option<GhInfo> = crate::paths::read_json(&cache_path(cache_dir, &key));
    if let Some(cached) = cached
        && cached.schema == SCHEMA
        && now_ms - cached.fetched_ms
            < if cached.is_final() {
                FINAL_RECHECK_MS
            } else {
                max_age_ms(&cached)
            }
    {
        return Some(cached);
    }
    let info = fetch(dir, now_ms);
    store(cache_dir, dir, branch, &info);
    Some(info)
}

/// Run `gh` in `dir` for the current branch's PR, its base and its checks.
pub fn fetch(dir: &str, now_ms: i64) -> GhInfo {
    fetch_with(Path::new("gh"), dir, now_ms)
}

/// [`fetch`], running `program` as `gh`.
fn fetch_with(program: &Path, dir: &str, now_ms: i64) -> GhInfo {
    let mut info = GhInfo {
        fetched_ms: now_ms,
        schema: SCHEMA,
        ..GhInfo::default()
    };
    std::thread::scope(|scope| {
        // Asked alongside `pr view`, not after it: two round trips at once.
        // With no PR its answer is thrown away. `gh pr checks` exits non-zero
        // for failing or pending checks but still prints the JSON, so only
        // the output matters.
        let checks = scope.spawn(|| {
            gh(
                program,
                dir,
                &["pr", "checks", "--json", "name,bucket,link"],
            )
        });
        let view = gh(
            program,
            dir,
            &[
                "pr",
                "view",
                "--json",
                "number,state,baseRefName,baseRefOid,isDraft,url,reviewDecision,reviews,author,reviewRequests,comments",
            ],
        );
        let Some(pr) = view.as_deref().and_then(parse_pr_view) else {
            return;
        };
        info.pr_number = Some(pr.number);
        info.is_draft = pr.is_draft;
        info.state = pr.state;
        info.base_sha = pr.base_sha;
        info.url = pr.url;
        info.review_decision = pr.review_decision;
        info.reviews = pr.reviews;
        info.latest_reviews = pr.latest_reviews;
        info.requested = pr.requested;
        info.assessment = pr.assessment;
        if is_non_default_base(&pr.base) && !pr.base.starts_with('-') {
            info.base_pr = gh(program, dir, &["pr", "view", &pr.base, "--json", "number"])
                .and_then(|out| serde_json::from_str::<Value>(&out).ok())
                .and_then(|v| v.get("number")?.as_i64());
        }
        info.base = Some(pr.base);
        info.ci = checks
            .join()
            .ok()
            .flatten()
            .as_deref()
            .and_then(summarize_checks);
    });
    info
}

fn store(cache_dir: &Path, dir: &str, branch: &str, info: &GhInfo) {
    crate::paths::write_json_atomic(&cache_path(cache_dir, &cache_key(dir, branch)), info);
}

fn gh(program: &Path, dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

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
}

/// Length of the commit hashes shown.
const SHA_LEN: usize = 10;

pub fn parse_pr_view(json: &str) -> Option<PrView> {
    let v: Value = serde_json::from_str(json).ok()?;
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
            v.get("reviews")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            v.pointer("/author/login").and_then(Value::as_str),
        ),
        latest_reviews: latest_per_reviewer(
            v.get("reviews")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            v.pointer("/author/login").and_then(Value::as_str),
        ),
        requested: v
            .get("reviewRequests")
            .and_then(Value::as_array)
            .map(|requests| {
                requests
                    .iter()
                    .filter_map(|r| {
                        // A user has `login`; a team has `slug` (and `name`).
                        r.get("login")
                            .or_else(|| r.get("slug"))
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        assessment: v
            .get("comments")
            .and_then(Value::as_array)
            .and_then(|comments| latest_assessment(comments)),
    })
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

/// Fold `gh pr checks --json name,bucket,link` into one verdict. Buckets are
/// `pass`, `fail`, `pending`, `skipping` and `cancel`; only `fail` is red and
/// only `pending` is in flight. `None` when there are no checks at all.
pub fn summarize_checks(json: &str) -> Option<Ci> {
    let v: Value = serde_json::from_str(json).ok()?;
    let checks = v.as_array().filter(|a| !a.is_empty())?;
    let mut failed = Vec::new();
    let mut pending = 0;
    for check in checks {
        match check.get("bucket").and_then(Value::as_str) {
            Some("fail") => failed.push(FailedJob {
                name: check
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                link: check
                    .get("link")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty())
                    .map(String::from),
            }),
            Some("pending") => pending += 1,
            _ => {}
        }
    }
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
    fn a_merged_pr_is_final_and_kept_while_a_miss_or_open_one_is_not() {
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
        let dir = std::env::temp_dir().join(format!("sl-gh-final-{}", std::process::id()));
        store(
            &dir,
            "/d",
            "b",
            &GhInfo {
                fetched_ms: 0,
                schema: SCHEMA,
                ..info(Some(7), Some("MERGED"))
            },
        );
        let kept = load_or_fetch(&dir, "/d", "b", 30 * 60 * 1000, |_| 1_000).expect("cached");
        assert_eq!(
            kept.pr_number,
            Some(7),
            "half an hour later it is still not asked again"
        );
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn entries_from_an_older_schema_deserialize_but_are_not_current() {
        // What the previous build wrote: no reviews, no url, no schema.
        let old: GhInfo =
            serde_json::from_str(r#"{"pr_number":734,"base":"main","ci":null,"fetched_ms":1}"#)
                .unwrap();
        assert_eq!(old.schema, 0);
        assert!(old.reviews.is_empty());
        assert_ne!(old.schema, SCHEMA);
        assert_eq!(fetch_stub_schema(), SCHEMA);
    }

    fn fetch_stub_schema() -> u32 {
        GhInfo {
            schema: SCHEMA,
            ..GhInfo::default()
        }
        .schema
    }

    /// A stub `gh` that takes `delay` per call: `pr view` of the current
    /// branch, of its base, and `pr checks`. Each call's start and end are
    /// logged to `gh.log` beside it.
    fn stub_gh(delay: &str, view: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let program = dir.join("gh");
        let script = format!(
            "#!/bin/sh\necho \"start $2 $3\" >> \"$0.log\"\nsleep {delay}\necho \"end $2 $3\" >> \"$0.log\"\ncase \"$2 $3\" in\n  \"view --json\") echo '{view}' ;;\n  \"view base\") echo '{{\"number\":6}}' ;;\n  \"checks --json\") echo '[{{\"name\":\"a\",\"bucket\":\"fail\"}}]' ;;\nesac\n"
        );
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        (dir_guard, program)
    }

    #[test]
    fn a_fetch_asks_for_the_checks_alongside_the_pr() {
        let (dir, program) = stub_gh(
            "0.3",
            r#"{"number":7,"baseRefName":"base","isDraft":false}"#,
        );
        let info = fetch_with(&program, dir.path().to_str().unwrap(), 1);
        assert_eq!(
            (info.pr_number, info.base.as_deref(), info.base_pr),
            (Some(7), Some("base"), Some(6))
        );
        assert_eq!(info.ci.map(|ci| ci.state), Some(CiState::Failing));
        let log = fs::read_to_string(dir.path().join("gh.log")).unwrap();
        let at = |line: &str| {
            log.lines()
                .position(|l| l == line)
                .unwrap_or_else(|| panic!("{line:?} not in {log}"))
        };
        assert!(
            at("start checks --json") < at("end view --json"),
            "the checks did not wait for the PR:\n{log}"
        );
    }

    #[test]
    fn with_no_pr_the_checks_are_dropped() {
        let (dir, program) = stub_gh("0", "");
        let info = fetch_with(&program, dir.path().to_str().unwrap(), 1);
        assert_eq!((info.pr_number, info.ci), (None, None));
        assert_eq!(info.fetched_ms, 1);
    }

    #[test]
    fn cache_key_separates_directories_and_branches() {
        assert_eq!(cache_key("/a", "x"), cache_key("/a", "x"));
        assert_ne!(cache_key("/a", "x"), cache_key("/b", "x"));
        assert_ne!(cache_key("/a", "x"), cache_key("/a", "y"));
        assert_ne!(cache_key("/ab", "c"), cache_key("/a", "bc"));
    }
}
