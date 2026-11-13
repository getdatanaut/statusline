//! GitHub's API, sent with `/usr/bin/curl` (at a fixed path, so a bare `PATH`
//! does not matter, and without a TLS stack compiled into the status line).
//!
//! PRs are asked about with GraphQL: one request per repository, however
//! many branches. A few things only the REST API has (stacks, Actions jobs)
//! go through [`rest`]. The token comes from the [`TokenSource`] the program
//! installed (the tray's Keychain item; the status line installs `gh`'s), is
//! read once per process (by one caller at a time, so a Keychain prompt is
//! shown once), and is handed to curl on stdin, never on its command line.
//! Finding none is remembered until a token is stored; failing to read one
//! (the Keychain locked, or the prompt denied), for [`RETRY_UNREADABLE`].

use std::fmt::Write as _;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{PrView, pr_view};

/// Why GitHub gave no answer. Never stored: a failed ask changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No token stored, or it could not be read.
    NoToken(String),
    /// GitHub refused the token.
    Unauthorized,
    /// curl failed or timed out, or GitHub answered with an error.
    Request(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoToken(why) | Failure::Request(why) => write!(f, "{why}"),
            Failure::Unauthorized => write!(
                f,
                "GitHub refused the token. Replace it in Settings → Integrations."
            ),
        }
    }
}

/// Sends a GraphQL request body, returning the response body.
pub type Post<'a> = dyn Fn(&str) -> Result<String, Failure> + Sync + 'a;

const API: &str = "https://api.github.com";
const CURL: &str = "/usr/bin/curl";
/// curl's own limit for a PR lookup.
const POST_SECS: u64 = 15;
/// Branches asked about in one request; more are split over several.
const BRANCHES_PER_REQUEST: usize = 20;
/// PRs read per branch name, newest first: enough that forks' PRs from
/// branches of the same name do not crowd out the one sought.
const PRS_PER_BRANCH: usize = 10;
/// How long a token that could not be read is not tried again.
pub const RETRY_UNREADABLE: Duration = Duration::from_mins(10);
/// What [`Failure::NoToken`] says when no token is stored.
pub const NO_TOKEN: &str = "No GitHub token. Add one in Settings → Integrations.";

/// What is read of each PR. `reviews` is the full history (the latest opinion
/// per reviewer is worked out from it); `comments` is enough of the newest to
/// find a risk/quality table; the rollup is the head commit's checks, each
/// check run with the workflow and event it ran for (one job name can run in
/// several), and the rollup's own state for when there are more than a page.
const PR_FIELDS: &str = "fragment Pr on PullRequest { \
    number state isDraft url baseRefName baseRefOid reviewDecision isCrossRepository \
    author { login } headRepositoryOwner { login } \
    reviews(last: 100) { nodes { author { login } state submittedAt } } \
    reviewRequests(first: 20) { nodes { requestedReviewer { \
        ... on User { login } ... on Bot { login } ... on Mannequin { login } ... on Team { slug } } } } \
    comments(last: 30) { nodes { author { login } body } } \
    commits(last: 1) { nodes { commit { statusCheckRollup { state \
        contexts(first: 100) { pageInfo { hasNextPage } nodes { \
        ... on CheckRun { name status conclusion detailsUrl startedAt \
            checkSuite { workflowRun { event workflow { name } } } } \
        ... on StatusContext { context state targetUrl createdAt } } } } } } } }";

/// Reads the stored token: `None` when none is stored.
pub type TokenSource = fn() -> Result<Option<String>, String>;

static SOURCE: OnceLock<TokenSource> = OnceLock::new();
static TOKEN: Mutex<Read> = Mutex::new(Read::NotYet);

/// What the last look at the token found.
#[derive(Debug, Clone)]
enum Read {
    NotYet,
    Token(String),
    /// None stored: not looked for again until one is.
    Missing,
    /// It could not be read: not tried again until the instant passes.
    Unreadable(String, Instant),
}
static OFFLINE: AtomicBool = AtomicBool::new(false);
/// The Keychain had no token the last time it was read.
static MISSING: AtomicBool = AtomicBool::new(false);
/// GitHub refused the stored token at the last request: expired or revoked.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// Where the token comes from, for the rest of the process.
pub fn use_token_source(source: TokenSource) {
    SOURCE.set(source).ok();
}

/// Never read the token or touch the network again in this process: for the
/// tray's self-test, whose stubbed `PATH` can catch neither `/usr/bin/curl`
/// nor the Keychain.
pub fn go_offline() {
    OFFLINE.store(true, Ordering::SeqCst);
    forget_token();
}

/// Whether the last look in the Keychain found no token: GitHub has not
/// been asked about anything since.
pub fn token_missing() -> bool {
    MISSING.load(Ordering::SeqCst)
}

/// Whether GitHub refused the stored token at the last request made with it
/// (it expired, or was revoked); cleared by the next it accepts, or by
/// [`token_replaced`].
pub fn token_refused() -> bool {
    REFUSED.load(Ordering::SeqCst)
}

/// A new token was stored, or the old one removed: read it again, and
/// forget that the old one was refused.
pub fn token_replaced() {
    forget_token();
    REFUSED.store(false, Ordering::SeqCst);
}

/// Whether [`go_offline`] was called.
pub fn is_offline() -> bool {
    OFFLINE.load(Ordering::SeqCst)
}

fn cached_token() -> MutexGuard<'static, Read> {
    TOKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read the token from the Keychain again at the next request: after it is
/// replaced or removed, or refused.
pub fn forget_token() {
    *cached_token() = Read::NotYet;
}

/// Forget `used` if it is still the token in memory: a request answered
/// after the token was replaced says nothing about the new one.
fn forget_refused(used: &str) -> bool {
    let mut cached = cached_token();
    let current = matches!(&*cached, Read::Token(token) if token == used);
    if current {
        *cached = Read::NotYet;
    }
    current
}

/// The stored token, kept in memory once read; never written anywhere else.
/// The lock is held while it is read, so callers at once wait for one read
/// instead of each reading (and each prompting).
fn token() -> Result<String, Failure> {
    if is_offline() {
        return Err(Failure::NoToken("offline".to_owned()));
    }
    let mut cached = cached_token();
    match &*cached {
        Read::Token(token) => return Ok(token.clone()),
        Read::Missing => return Err(Failure::NoToken(NO_TOKEN.to_owned())),
        Read::Unreadable(why, until) if Instant::now() < *until => {
            return Err(Failure::NoToken(why.clone()));
        }
        Read::NotYet | Read::Unreadable(..) => {}
    }
    let read = SOURCE
        .get()
        .ok_or_else(|| Failure::NoToken(NO_TOKEN.to_owned()))?;
    let (read, found) = match read() {
        Ok(Some(token)) => (Read::Token(token.clone()), Ok(token)),
        Ok(None) => (Read::Missing, Err(Failure::NoToken(NO_TOKEN.to_owned()))),
        Err(why) => (
            Read::Unreadable(why.clone(), Instant::now() + RETRY_UNREADABLE),
            Err(Failure::NoToken(why)),
        ),
    };
    MISSING.store(matches!(read, Read::Missing), Ordering::SeqCst);
    *cached = read;
    found
}

/// Whether a token can be read, reading it if it was not yet.
pub fn has_token() -> bool {
    token().is_ok()
}

/// Whether GitHub can be asked: a token can be read, and GitHub did not
/// refuse it at the last request.
pub fn can_ask() -> bool {
    !token_refused() && has_token()
}

/// What came back from one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
    /// Where a redirect points, which is not followed.
    pub location: Option<String>,
}

impl Response {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body of a 2xx answer, else a [`Failure::Request`] quoting it.
    ///
    /// # Errors
    ///
    /// Any status but a 2xx.
    pub fn ok(self) -> Result<String, Failure> {
        if self.is_success() {
            Ok(self.body)
        } else {
            let message = serde_json::from_str::<Value>(&self.body)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| self.body.chars().take(200).collect());
            Err(Failure::Request(format!("HTTP {}: {message}", self.status)))
        }
    }
}

/// One request with curl: `method` to `url`, with `body` as JSON, the token
/// (from stdin) when `token` is given, never following a redirect.
fn send(
    method: &str,
    url: &str,
    body: Option<&str>,
    token: Option<&str>,
    secs: u64,
) -> Result<Response, Failure> {
    if is_offline() {
        return Err(Failure::Request("offline".to_owned()));
    }
    let mut command = Command::new(CURL);
    command.args([
        "--silent",
        "--show-error",
        "--max-time",
        &secs.to_string(),
        "--request",
        method,
        // The authorization header, if any, read from stdin.
        "--header",
        "@-",
        "--header",
        "Accept: application/vnd.github+json",
        "--header",
        "User-Agent: statusline",
        "--write-out",
        "\n%{http_code}\n%{redirect_url}",
    ]);
    if let Some(body) = body {
        command.args([
            "--header",
            "Content-Type: application/json",
            "--data-raw",
            body,
        ]);
    }
    command.arg(url);
    let header = token.map_or_else(String::new, |token| {
        format!("Authorization: bearer {token}\n")
    });
    let ran =
        crate::process::run_with_input(command, header.as_bytes(), Duration::from_secs(secs + 5))
            .map_err(|error| Failure::Request(format!("curl: {error:?}")))?;
    if !ran.success {
        return Err(Failure::Request(ran.stderr.trim().to_owned()));
    }
    // The body, then the two lines `--write-out` added.
    let mut parts = ran.stdout.rsplitn(3, '\n');
    let (Some(location), Some(code), Some(body)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(Failure::Request("no status from curl".to_owned()));
    };
    let status = code
        .trim()
        .parse()
        .map_err(|_not_a_number| Failure::Request(format!("no status from curl: {code:?}")))?;
    Ok(Response {
        status,
        body: body.to_owned(),
        location: Some(location.trim())
            .filter(|l| !l.is_empty())
            .map(str::to_owned),
    })
}

/// [`send`] with the stored token; a refused token is forgotten, so the
/// next request reads the Keychain again.
fn send_signed(
    method: &str,
    url: &str,
    body: Option<&str>,
    secs: u64,
) -> Result<Response, Failure> {
    let token = token()?;
    let response = send(method, url, body, Some(&token), secs)?;
    let refused = response.status == 401;
    // About the token in memory only: one replaced while this ran is not
    // the one GitHub answered about.
    if !refused || forget_refused(&token) {
        REFUSED.store(refused, Ordering::SeqCst);
    }
    if refused {
        return Err(Failure::Unauthorized);
    }
    Ok(response)
}

/// POST `body` to GitHub's GraphQL endpoint, giving up after `secs`.
///
/// # Errors
///
/// A [`Failure`] for no token, a refused one, or anything but an HTTP 200.
pub fn graphql(body: &str, secs: u64) -> Result<String, Failure> {
    send_signed("POST", &format!("{API}/graphql"), Some(body), secs)?.ok()
}

/// The time a round of requests may take all together: the tray gives each
/// pass over the sessions its PR & CI interval, so a GitHub that hangs never
/// holds a pass up for longer than one interval, however many requests the
/// pass makes. Each request gets at most what is left.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    until: Instant,
}

impl Budget {
    #[must_use]
    pub fn new(total: Duration) -> Self {
        Self {
            until: Instant::now() + total,
        }
    }

    /// `wanted` seconds, or fewer if that is all that is left.
    ///
    /// # Errors
    ///
    /// Less than a second left: the request is not made.
    pub fn secs(&self, wanted: u64) -> Result<u64, Failure> {
        let left = self
            .until
            .saturating_duration_since(Instant::now())
            .as_secs();
        match left.min(wanted) {
            0 => Err(Failure::Request(
                "no time left for GitHub this round".to_owned(),
            )),
            secs => Ok(secs),
        }
    }
}

/// [`post`] within `budget`.
///
/// # Errors
///
/// As [`graphql`], or `budget` spent.
pub fn post_within(budget: &Budget, body: &str) -> Result<String, Failure> {
    graphql(body, budget.secs(POST_SECS)?)
}

/// [`graphql`] with the PR lookup's time limit: the status line's [`Post`].
///
/// # Errors
///
/// As [`graphql`].
pub fn post(body: &str) -> Result<String, Failure> {
    graphql(body, POST_SECS)
}

/// `method` on the REST API's `path` (`repos/owner/name/...`), giving up
/// after `secs`. Any status but 401 is the caller's to read.
///
/// # Errors
///
/// A [`Failure`] for no token, a refused one, or curl failing.
pub fn rest(method: &str, path: &str, secs: u64) -> Result<Response, Failure> {
    send_signed(method, &format!("{API}/{path}"), None, secs)
}

/// GET `url` without the token: a link GitHub redirected to, which carries
/// its own short-lived signature and is not GitHub's to be sent the token.
///
/// # Errors
///
/// curl failing, or anything but a 2xx.
pub fn download(url: &str, secs: u64) -> Result<String, Failure> {
    send("GET", url, None, None, secs)?.ok()
}

/// Whose `token` is, asked before it is stored.
///
/// # Errors
///
/// A refused token, or GitHub not answering.
pub fn login_of(token: &str) -> Result<String, Failure> {
    let body = json!({ "query": "query { viewer { login } }" }).to_string();
    let response = send(
        "POST",
        &format!("{API}/graphql"),
        Some(&body),
        Some(token),
        POST_SECS,
    )?;
    if response.status == 401 {
        return Err(Failure::Unauthorized);
    }
    let value: Value = serde_json::from_str(&response.ok()?)
        .map_err(|error| Failure::Request(error.to_string()))?;
    value
        .pointer("/data/viewer/login")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Failure::Request("no login in GitHub's answer".to_owned()))
}

/// Whose the stored token is.
///
/// # Errors
///
/// As [`login_of`], or no token.
pub fn login() -> Result<String, Failure> {
    let token = token()?;
    let login = login_of(&token);
    let refused = login == Err(Failure::Unauthorized);
    if (refused && forget_refused(&token)) || login.is_ok() {
        REFUSED.store(refused, Ordering::SeqCst);
    }
    login
}

/// The request body asking `repo` (`owner/name`) for the newest PR of each
/// of `branches`, whatever its state: a merged PR is still the branch's PR.
pub fn branches_query(repo: &str, branches: &[&str]) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    let mut params = String::from("$owner: String!, $name: String!");
    let mut fields = String::new();
    let mut variables = serde_json::Map::new();
    variables.insert("owner".into(), json!(owner));
    variables.insert("name".into(), json!(name));
    for (i, branch) in branches.iter().enumerate() {
        write!(params, ", $b{i}: String!").ok();
        write!(
            fields,
            "b{i}: pullRequests(headRefName: $b{i}, states: [OPEN, MERGED, CLOSED], \
             first: {PRS_PER_BRANCH}, orderBy: {{field: CREATED_AT, direction: DESC}}) \
             {{ nodes {{ ...Pr }} }} "
        )
        .ok();
        variables.insert(format!("b{i}"), json!(branch));
    }
    let query = format!(
        "query({params}) {{ repository(owner: $owner, name: $name) {{ {fields}}} }} {PR_FIELDS}"
    );
    Some(json!({ "query": query, "variables": variables }).to_string())
}

/// A branch to ask about, and the owner of the repository it is pushed to
/// (`origin`): the PR's head. `None` for the repository asked, as when
/// `origin` is not a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head<'a> {
    pub branch: &'a str,
    pub owner: Option<&'a str>,
}

impl<'a> Head<'a> {
    pub fn new(branch: &'a str, owner: Option<&'a str>) -> Self {
        Self { branch, owner }
    }
}

/// Each of `heads`' PR from the answer to [`branches_query`]: the newest one
/// opened from where the branch is pushed (from this repository, or from the
/// fork `origin` is; another fork's PR from a branch of the same name is not
/// it), or `None` for a branch with no PR.
///
/// # Errors
///
/// When the answer is not one: no repository in it (not found, no access,
/// or GitHub reporting errors), or a branch missing from it.
pub fn parse_branches(json: &str, heads: &[Head<'_>]) -> Result<Vec<Option<PrView>>, Failure> {
    let value: Value =
        serde_json::from_str(json).map_err(|error| Failure::Request(error.to_string()))?;
    let Some(repository) = value.pointer("/data/repository").filter(|r| r.is_object()) else {
        let errors = value
            .get("errors")
            .map_or_else(|| "no repository".to_owned(), Value::to_string);
        return Err(Failure::Request(errors.chars().take(300).collect()));
    };
    heads
        .iter()
        .enumerate()
        .map(|(i, head)| {
            let nodes = repository
                .pointer(&format!("/b{i}/nodes"))
                .and_then(Value::as_array)
                .ok_or_else(|| Failure::Request(format!("branch {i} missing from the answer")))?;
            Ok(nodes.iter().filter_map(pr_view).find(|pr| {
                match (head.owner, pr.head_owner.as_deref()) {
                    (Some(wanted), Some(owner)) => owner.eq_ignore_ascii_case(wanted),
                    _ => !pr.cross_repository,
                }
            }))
        })
        .collect()
}

/// Each of `heads`' PR in `repo`, asked with `post`, in as few requests as
/// [`BRANCHES_PER_REQUEST`] allows.
///
/// # Errors
///
/// The first request's [`Failure`]: then nothing is known of any branch.
pub fn ask_branches(
    post: &Post<'_>,
    repo: &str,
    heads: &[Head<'_>],
) -> Result<Vec<Option<PrView>>, Failure> {
    let mut found = Vec::with_capacity(heads.len());
    for chunk in heads.chunks(BRANCHES_PER_REQUEST) {
        let branches: Vec<&str> = chunk.iter().map(|head| head.branch).collect();
        let body = branches_query(repo, &branches)
            .ok_or_else(|| Failure::Request(format!("not owner/name: {repo}")))?;
        found.extend(parse_branches(&post(&body)?, chunk)?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real query against GitHub, read-only: `SL_GH_LIVE=owner/name:branch,...
    /// cargo test -p sl-core live -- --ignored --nocapture`.
    #[test]
    #[ignore = "asks GitHub"]
    fn live() {
        let spec = std::env::var("SL_GH_LIVE").expect("SL_GH_LIVE=owner/name:branch,branch");
        let (repo, branches) = spec.split_once(':').expect("owner/name:branches");
        let branches: Vec<&str> = branches.split(',').collect();
        let heads: Vec<Head<'_>> = branches.iter().map(|b| Head::new(b, None)).collect();
        let found = ask_branches(&post, repo, &heads).expect("an answer");
        for (branch, pr) in branches.iter().zip(&found) {
            println!("{branch}: {pr:?}");
        }
        assert_eq!(found.len(), branches.len());
    }

    #[test]
    fn the_query_asks_every_branch_in_any_state_with_variables() {
        let body = branches_query("o/r", &["a", "we\"ird"]).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        let query = v["query"].as_str().unwrap();
        assert!(
            query.contains("b0: pullRequests(headRefName: $b0, states: [OPEN, MERGED, CLOSED]")
        );
        assert!(query.contains("b1: pullRequests(headRefName: $b1"));
        assert_eq!(v["variables"]["owner"], "o");
        assert_eq!(v["variables"]["name"], "r");
        assert_eq!(
            v["variables"]["b1"], "we\"ird",
            "a branch name travels as data"
        );
        assert_eq!(branches_query("no-slash", &["a"]), None);
    }

    #[test]
    fn each_branch_gets_its_newest_own_pr_or_none() {
        let answer = r#"{"data":{"repository":{
            "b0":{"nodes":[{"number":9,"baseRefName":"main","isCrossRepository":true},
                           {"number":7,"state":"MERGED","baseRefName":"main","isCrossRepository":false}]},
            "b1":{"nodes":[]}}}}"#;
        let found = parse_branches(answer, &[Head::new("a", None), Head::new("b", None)]).unwrap();
        assert_eq!(
            found[0].as_ref().map(|pr| pr.number),
            Some(7),
            "a fork's PR is skipped"
        );
        assert_eq!(found[1], None, "no PR is an answer");
    }

    #[test]
    fn a_branch_pushed_to_a_fork_gets_the_pr_from_that_fork_only() {
        let answer = r#"{"data":{"repository":{"b0":{"nodes":[
            {"number":12,"baseRefName":"main","isCrossRepository":true,"headRepositoryOwner":{"login":"other"}},
            {"number":11,"baseRefName":"main","isCrossRepository":true,"headRepositoryOwner":{"login":"Me"}},
            {"number":10,"baseRefName":"main","isCrossRepository":false,"headRepositoryOwner":{"login":"org"}}]}}}}"#;
        let found = parse_branches(answer, &[Head::new("feat", Some("me"))]).unwrap();
        assert_eq!(found[0].as_ref().map(|pr| pr.number), Some(11));
    }

    #[test]
    fn an_answer_without_the_repository_or_a_branch_is_a_failure() {
        let one = [Head::new("a", None)];
        assert!(matches!(
            parse_branches(r#"{"data":{"repository":null},"errors":[{"type":"NOT_FOUND"}]}"#, &one),
            Err(Failure::Request(text)) if text.contains("NOT_FOUND")
        ));
        assert!(
            parse_branches(
                r#"{"data":{"repository":{"b0":{"nodes":[]}}}}"#,
                &[Head::new("a", None), Head::new("b", None)]
            )
            .is_err()
        );
        assert!(parse_branches("<html>", &one).is_err());
    }

    #[test]
    fn many_branches_are_split_over_requests_and_one_failure_fails_all() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let post = |body: &str| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let v: Value = serde_json::from_str(body).unwrap();
            let n = v["variables"].as_object().unwrap().len() - 2;
            let fields: Vec<String> = (0..n)
                .map(|i| format!(r#""b{i}":{{"nodes":[]}}"#))
                .collect();
            Ok(format!(
                r#"{{"data":{{"repository":{{{}}}}}}}"#,
                fields.join(",")
            ))
        };
        let branches: Vec<String> = (0..45).map(|i| format!("b{i}")).collect();
        let names: Vec<Head<'_>> = branches.iter().map(|b| Head::new(b, None)).collect();
        let found = ask_branches(&post, "o/r", &names).unwrap();
        assert_eq!(found.len(), 45);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let failing = |_: &str| Err(Failure::Unauthorized);
        assert_eq!(
            ask_branches(&failing, "o/r", &names),
            Err(Failure::Unauthorized)
        );
    }
}
