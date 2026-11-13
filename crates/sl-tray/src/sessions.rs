//! Open Claude Code sessions, and the git/GitHub state of the branch each one
//! is on.
//!
//! The session list comes from the `~/.claude/sessions/<pid>.json` record
//! Claude Code keeps for every running process (`cwd`, `name`, `status`), so
//! the tray needs nothing from the status line to know what is open. The
//! records are checked against `claude agents --json`, which lists the same
//! sessions at the cost of a Node process, and it stands in whenever they
//! cannot be read or disagree (see [`ClaudeLister`]). Each distinct
//! directory is then resolved to its branch with `git` and to its PR and CI
//! state with `sl_core::gh`, whose per-branch cache the status line shares.
//!
//! Everything here runs on its own thread: `gh` takes about a second per call,
//! which must neither delay the usage snapshot nor block AppKit.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sl_core::gh::{self, Assessment, CiState, GhInfo, Review, ReviewState, Score, Verdict};
use sl_core::types::ContextCapture;

use crate::view_prefs::{SortOrder, Source, Tuning};

use crate::cache_ttl::{CacheInfo, CacheReader};
use crate::notify::{self, Notifier, PrWatch, PrWatcher, Watched};

/// Whether any session was open at the last poll; the usage thread backs off
/// while none is. True until the first poll says otherwise.
pub static ANY_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// How often the remembered sessions are saved when only their times moved.
const STICKY_SAVE_EVERY_MS: i64 = 10 * 60_000;

/// The longest the session list waits between passes while nothing is open; a
/// new session's record rings [`SESSIONS_BELL`] long before.
const IDLE_SESSIONS_CAP: u64 = 120;

/// Bumped, and its waiters woken, by [`wake`].
static WAKE: (std::sync::Mutex<u64>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// Tell every [`nap`] to ask again how long it is: a setting changed, or
/// whether any session is open did.
pub fn wake() {
    let (count, woken) = &WAKE;
    *count
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
    woken.notify_all();
}

/// Record whether any session is open; the polling intervals stretch or
/// shrink with it, so a change wakes every [`nap`]. Whether it did.
fn note_any_open(any: bool) -> bool {
    let changed = ANY_OPEN.swap(any, Ordering::Relaxed) != any;
    if changed {
        wake();
    }
    changed
}

/// Wait for the interval `wanted` reports, in seconds, asking again whenever
/// [`wake`] is called -- so a change of setting, or a session opening, ends a
/// long pause at once -- and otherwise sleeping the whole of it.
pub fn nap(mut wanted: impl FnMut() -> u64) {
    let start = Instant::now();
    let (count, woken) = &WAKE;
    loop {
        // Read before asking, so a wake between the two is not missed.
        let seen = *count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(left) = Duration::from_secs(wanted())
            .checked_sub(start.elapsed())
            .filter(|left| !left.is_zero())
        else {
            return;
        };
        let guard = count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Returns on a wake, at the end of the interval, or spuriously; each
        // goes round to ask again.
        drop(woken.wait_timeout_while(guard, left, |now| *now == seen));
    }
}

/// Rung by a file-system watcher when what a thread reads has changed, so the
/// thread looks again at once rather than on a timer.
#[derive(Debug)]
pub struct Bell(std::sync::atomic::AtomicBool);

impl Bell {
    pub const fn new() -> Self {
        Self(std::sync::atomic::AtomicBool::new(false))
    }

    /// Something changed. Wakes the [`nap`]s only on the first ring since the
    /// last [`answer`](Self::answer), however often a busy transcript rings.
    pub fn ring(&self) {
        if !self.0.swap(true, Ordering::Relaxed) {
            wake();
        }
    }

    /// About to look: cleared before, so a ring while looking is kept.
    pub fn answer(&self) {
        self.0.store(false, Ordering::Relaxed);
    }

    /// The seconds to [`nap`] for: `gap` -- the least time between two looks
    /// -- once rung, else `backstop`, the most.
    pub fn nap_secs(&self, gap: u64, backstop: u64) -> u64 {
        if self.0.load(Ordering::Relaxed) {
            gap
        } else {
            backstop
        }
    }
}

/// Rung when a transcript changes: the usage numbers are refreshed.
pub static USAGE_BELL: Bell = Bell::new();

/// Set when the menu or the Compact popover opens, until the next pass asks
/// GitHub with [`LOOK_FRESH_MS`].
static LOOKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// When someone looks, PR answers older than this are asked about again.
const LOOK_FRESH_MS: i64 = 30_000;

/// The menu or the Compact popover opened: have the sessions looked at now,
/// and their PRs asked about if what is known of them is not fresh.
pub fn looked() {
    LOOKED.store(true, Ordering::Relaxed);
    SESSIONS_BELL.ring();
}
/// Rung when Claude Code's session records or Codex's rollouts change: the
/// open sessions are read again.
pub static SESSIONS_BELL: Bell = Bell::new();

/// The least time between two passes over the sessions, however often they
/// change.
const SESSIONS_GAP: u64 = 3;
/// The most time between two passes while a session is open: what has no
/// event to say it changed (a branch, CI, the status line's context capture)
/// is picked up within this. PR and CI results are reused for as long as
/// Settings says, so this is also the finest that setting can be.
const SESSIONS_BACKSTOP: u64 = 15;
/// `claude agents --json` is a local call; anything slower is stuck.
const AGENTS_TIMEOUT: Duration = Duration::from_secs(10);
/// Rows reserved in the menu: more than are ever open at once in practice.
pub const MAX_SESSIONS: usize = 16;
/// Reviewer rows reserved in a session's submenu.
pub const MAX_REVIEWERS: usize = 6;
/// Failed-job rows reserved in a session's submenu.
pub const MAX_JOBS: usize = 5;
/// Branch names are cut to this many characters in a row.
const BRANCH_WIDTH: usize = 34;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Busy,
    Idle,
    /// Any other status string `claude agents` reports, shown as idle.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenSession {
    pub session_id: String,
    pub name: String,
    /// The repository the directory belongs to, shared by all its worktrees.
    pub project: String,
    pub cwd: String,
    pub activity: Activity,
    pub started_ms: i64,
    pub branch: Option<String>,
    pub gh: Option<GhInfo>,
    /// When the session last talked to the API, and the lifetime of the prompt
    /// cache it left behind; from the transcript.
    pub cache: Option<CacheInfo>,
    /// `/rc` is on, with its remote-control session id: the session is
    /// reachable from another device, so the Mac stays awake for it whether
    /// it is busy or idle (unless that is switched off in Settings).
    pub remote_control: Option<String>,
    /// The `claude` process, whose `~/.claude/sessions/<pid>.json` records `/rc`.
    pub pid: Option<u32>,
    /// How full its context is, as the status line last showed it; `None`
    /// until the status line has rendered for it.
    pub context: Option<ContextCapture>,
    /// The Linear ticket the branch is named for, when there is one and
    /// the `linear` tool could say.
    pub linear: Option<crate::linear::LinearInfo>,
    /// Claude Code's, or a Codex thread (see [`crate::codex_sessions`]).
    pub provider: Source,
    /// When it last finished or started a turn, for a session without a
    /// prompt cache to time its idleness by (Codex's).
    pub last_activity_ms: Option<i64>,
}

/// One running session, as `claude agents --json` lists it and as Claude
/// Code's own `~/.claude/sessions/<pid>.json` records it: the two agree field
/// for field, the file adding `/rc` and where the name came from.
#[derive(Debug, Deserialize)]
struct AgentEntry {
    pid: Option<u32>,
    cwd: String,
    name: Option<String>,
    status: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<i64>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "bridgeSessionId")]
    bridge_session_id: Option<String>,
    #[serde(rename = "nameSource")]
    name_source: Option<String>,
}

impl AgentEntry {
    fn into_session(self) -> OpenSession {
        OpenSession {
            session_id: self.session_id.unwrap_or_default(),
            cache: None,
            remote_control: self.bridge_session_id.filter(|b| !b.is_empty()),
            pid: self.pid,
            context: None,
            linear: None,
            provider: Source::Claude,
            last_activity_ms: None,
            name: self
                .name
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| dir_name(&self.cwd)),
            activity: match self.status.as_deref() {
                Some("busy") => Activity::Busy,
                Some("idle") => Activity::Idle,
                _ => Activity::Other,
            },
            started_ms: self.started_at.unwrap_or(0),
            project: String::new(),
            cwd: self.cwd,
            branch: None,
            gh: None,
        }
    }

    /// The user named it (`/rename`), rather than Claude Code deriving the
    /// name from the directory.
    fn named_by_user(&self) -> bool {
        self.name_source.as_deref().is_some_and(|s| s != "derived")
    }
}

/// Parse `claude agents --json`. Busy sessions sort first, then the most
/// recently started. `None` if the output is not a JSON array.
pub fn parse_agents(json: &str) -> Option<Vec<OpenSession>> {
    let entries: Vec<AgentEntry> = serde_json::from_str(json).ok()?;
    let mut sessions: Vec<OpenSession> =
        entries.into_iter().map(AgentEntry::into_session).collect();
    sort_sessions(&mut sessions);
    Some(sessions)
}

/// Claude Code's sessions, read off the `<pid>.json` records it keeps in
/// `dir` instead of asking `claude agents` (a whole Node process every poll).
/// A record outlives a process that crashed, and its pid can be reused, so
/// one counts only while `start_of` its pid -- when that process started, in
/// seconds -- is no later than the session's own `startedAt`.
///
/// `None` -- ask `claude agents` instead -- when `dir` cannot be listed, or a
/// live process's record cannot be read as one: half-written, or a format
/// this does not know. Otherwise the sessions, sorted as [`parse_agents`]
/// sorts them, with the pids of those the user named.
fn sessions_from_files(
    dir: &std::path::Path,
    start_of: impl Fn(u32) -> Option<u64>,
) -> Option<(Vec<OpenSession>, Vec<u32>)> {
    let mut sessions = Vec::new();
    let mut named = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // No records yet (Claude Code never ran here): no sessions, which is
        // an answer, not a failure to read one.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some((sessions, named));
        }
        Err(_) => return None,
    };
    for entry in entries {
        let entry = entry.ok()?;
        let file_name = entry.file_name();
        let Some(pid) = file_name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(started) = start_of(pid) else {
            continue;
        };
        let parsed = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| serde_json::from_str::<AgentEntry>(&text).ok());
        let record = parsed.filter(|r| {
            r.pid == Some(pid) && r.session_id.as_deref().is_some_and(|id| !id.is_empty())
        })?;
        // A process that started after the session did is another one on a
        // reused pid.
        if record
            .started_at
            .is_some_and(|ms| u64::try_from(ms / 1000).is_ok_and(|secs| started > secs))
        {
            continue;
        }
        if record.named_by_user() {
            named.push(pid);
        }
        sessions.push(record.into_session());
    }
    sort_sessions(&mut sessions);
    Some((sessions, named))
}

/// The remote-control session id, while Claude Code's own record of the
/// process says `/rc` is on: the file holds a `bridgeSessionId` while it is,
/// and drops it when it is turned off.
pub fn remote_control_id(sessions_dir: &std::path::Path, pid: u32) -> Option<String> {
    process_file(sessions_dir, pid)?
        .get("bridgeSessionId")?
        .as_str()
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
}

/// Whether the session's name is one the user gave it (`/rename`) rather than
/// one Claude Code derived from the directory: the file's `nameSource` says.
/// A name the user chose is kept over the session's title.
pub fn named_by_user(sessions_dir: &std::path::Path, pid: u32) -> bool {
    process_file(sessions_dir, pid).is_some_and(|v| {
        v.get("nameSource")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s != "derived")
    })
}

/// Claude Code's own record of a running `claude` process.
fn process_file(sessions_dir: &std::path::Path, pid: u32) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(sessions_dir.join(format!("{pid}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// Grouped by project (alphabetical, so the menu does not reshuffle), busy
/// sessions first within one, then the most recently started.
fn sort_sessions(sessions: &mut [OpenSession]) {
    sessions.sort_by(|a, b| {
        (
            &a.project,
            a.activity != Activity::Busy,
            std::cmp::Reverse(a.started_ms),
        )
            .cmp(&(
                &b.project,
                b.activity != Activity::Busy,
                std::cmp::Reverse(b.started_ms),
            ))
    });
}

/// A project's name from its git common directory: `/x/datanaut/main/.git`
/// is `datanaut` (a checkout named after its branch says nothing about the
/// project), `/x/repo/.git` is `repo`, a bare `/x/repo.git` is `repo`.
pub fn project_name(common_dir: &str) -> String {
    let path = common_dir.trim_end_matches('/');
    let path = path.strip_suffix("/.git").unwrap_or(path);
    let mut parts = path.rsplit('/').filter(|p| !p.is_empty());
    let last = parts.next().unwrap_or(path);
    let name = if matches!(last, "main" | "master" | "trunk") {
        parts.next().unwrap_or(last)
    } else {
        last
    };
    name.strip_suffix(".git").unwrap_or(name).to_owned()
}

fn dir_name(cwd: &str) -> String {
    cwd.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(cwd)
        .to_owned()
}

/// Run `claude agents --json`. A tray launched outside a shell may not have
/// `~/.local/bin` on its `PATH`, so the usual install locations are tried too.
fn run_agents() -> Option<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [
        "claude".to_owned(),
        home.join(".local/bin/claude").display().to_string(),
        "/opt/homebrew/bin/claude".to_owned(),
        "/usr/local/bin/claude".to_owned(),
    ];
    for bin in candidates {
        let mut command = Command::new(&bin);
        command.args(["agents", "--json"]);
        match sl_core::process::run(command, AGENTS_TIMEOUT) {
            // Not at this location: try the next.
            Err(sl_core::process::RunError::Spawn(_)) => {}
            Ok(ran) if ran.success => return Some(ran.stdout),
            Ok(_) | Err(sl_core::process::RunError::TimedOut) => return None,
        }
    }
    None
}

/// `claude agents --json`, with what each listed process's record adds: `/rc`
/// and whether the user named it.
fn from_agents(sessions_dir: &std::path::Path) -> Option<(Vec<OpenSession>, Vec<u32>)> {
    let mut sessions = run_agents().and_then(|out| parse_agents(&out))?;
    let mut named = Vec::new();
    for session in &mut sessions {
        let Some(pid) = session.pid else { continue };
        session.remote_control = remote_control_id(sessions_dir, pid);
        if named_by_user(sessions_dir, pid) {
            named.push(pid);
        }
    }
    Some((sessions, named))
}

/// How long the session records are believed before `claude agents` is
/// asked again whether they list what it does.
const CROSS_CHECK_EVERY: Duration = Duration::from_mins(5);
/// While the records and `claude agents` disagree, how long one answer from
/// `claude agents` stands before it is run again: passes come every few
/// seconds, and each run is a Node process.
const AGENTS_EVERY: Duration = Duration::from_secs(15);

/// The directory Claude Code keeps its `<pid>.json` session records in:
/// `sessions/` in the first of its config directories (see
/// [`sl_core::paths::claude_config_dirs`]) that has one.
pub fn records_dir() -> std::path::PathBuf {
    let dirs: Vec<std::path::PathBuf> =
        sl_core::paths::claude_config_dirs(&dirs::home_dir().unwrap_or_default())
            .into_iter()
            .map(|dir| dir.join("sessions"))
            .collect();
    dirs.iter()
        .find(|dir| dir.is_dir())
        .or(dirs.first())
        .cloned()
        .unwrap_or_default()
}

/// Where Claude Code's open sessions come from. Its `<pid>.json` records
/// ([`sessions_from_files`]) cost a few file reads; `claude agents` costs a
/// Node process (about 0.1 s of CPU and 150 MB). The records are not a
/// documented interface, so they are used only once `claude agents` has
/// listed the same sessions, checked again every [`CROSS_CHECK_EVERY`], and
/// `claude agents` stands in from the first disagreement until a check agrees
/// again, run at most every [`AGENTS_EVERY`].
#[derive(Debug, Default)]
pub struct ClaudeLister {
    checked_at: Option<Instant>,
    agreed: bool,
    /// What `claude agents` last said while disagreeing, and when.
    standing_in: Option<(Instant, Listed)>,
}

/// One pass's sessions, with the pids of those the user named.
type Listed = (Vec<OpenSession>, Vec<u32>);

impl ClaudeLister {
    /// This pass's sessions, from `files` when they can be trusted, else
    /// from `agents`; `None` when neither could say.
    fn list(
        &mut self,
        now: Instant,
        files: impl FnOnce() -> Option<Listed>,
        agents: impl FnOnce() -> Option<Listed>,
    ) -> Option<Listed> {
        let Some(from_files) = files() else {
            crate::trace("session records unreadable: asking claude agents");
            return agents();
        };
        let due = !self.agreed
            || self
                .checked_at
                .is_none_or(|at| now.saturating_duration_since(at) >= CROSS_CHECK_EVERY);
        if !due {
            return Some(from_files);
        }
        if let Some((at, listed)) = &self.standing_in
            && now.saturating_duration_since(*at) < AGENTS_EVERY
        {
            return Some(listed.clone());
        }
        self.checked_at = Some(now);
        // `claude` could not be run: the records are all there is.
        let Some(from_agents) = agents() else {
            return Some(from_files);
        };
        let ids = |sessions: &[OpenSession]| {
            let mut ids: Vec<(Option<u32>, String)> = sessions
                .iter()
                .map(|s| (s.pid, s.session_id.clone()))
                .collect();
            ids.sort();
            ids
        };
        let (on_file, listed) = (ids(&from_files.0), ids(&from_agents.0));
        self.agreed = on_file == listed;
        if self.agreed {
            self.standing_in = None;
            Some(from_files)
        } else {
            self.standing_in = Some((now, from_agents.clone()));
            crate::trace(&format!(
                "session records {on_file:?} disagree with claude agents {listed:?}: using claude agents"
            ));
            Some(from_agents)
        }
    }
}

/// Claude Code's sessions for one pass. `agents` is what `claude agents`
/// gave, `None` when it could not be run or parsed. Codex's sessions do not
/// depend on it, so while Codex is listed (`have_codex`) a failure keeps the
/// Claude Code sessions of the `previous` pass, rather than flashing them
/// away or hiding Codex's; with nothing else to list it is `None`, and the
/// caller keeps the whole list it has.
fn claude_half(
    agents: Option<Vec<OpenSession>>,
    previous: &[OpenSession],
    have_codex: bool,
) -> Option<Vec<OpenSession>> {
    match agents {
        Some(sessions) => Some(sessions),
        None if have_codex => Some(
            previous
                .iter()
                .filter(|s| s.provider == Source::Claude)
                .cloned()
                .collect(),
        ),
        None => None,
    }
}

/// What is open, from local files alone -- no network -- so it can be shown
/// at once: Claude Code's session records (or `claude agents`), Codex's
/// rollouts, each directory's branch, the PR state last stored, whatever
/// Linear said before. [`ask_linear`] and [`GithubRound`] bring the rest.
/// `None` when `claude agents` could not be run or parsed and there are no
/// Codex sessions to list instead, so the caller keeps the list it already
/// has rather than flashing an empty one.
/// `previous`: the list the last pass published, whose Claude Code sessions
/// stand while `claude agents` fails.
/// `linear`: the tickets known so far, `None` to show none.
/// `codex`: the open Codex threads to list beside Claude Code's, when Codex
/// is installed.
/// `projects`: each directory's project, found once: it does not change.
pub fn load(
    caches: &mut CacheReader,
    linear: Option<&crate::linear::LinearCache>,
    codex: Option<&mut crate::codex_sessions::CodexSessions>,
    claude: &mut ClaudeLister,
    previous: &[OpenSession],
    projects: &mut HashMap<String, String>,
) -> Option<Vec<OpenSession>> {
    let codex_home = codex.as_ref().and_then(|_| sl_usage::codex::codex_home());
    let sessions_dir = records_dir();
    let listed = claude.list(
        Instant::now(),
        || sessions_from_files(&sessions_dir, sl_macos::process::started_at),
        || from_agents(&sessions_dir),
    );
    let (agents, named) = listed.map_or((None, Vec::new()), |(sessions, named)| {
        (Some(sessions), named)
    });
    let mut sessions = claude_half(agents, previous, codex_home.is_some())?;
    if let (Some(codex), Some(home)) = (codex, codex_home) {
        sessions.extend(codex.load(&home));
    }
    let store = gh::Store::open_default();
    for session in &mut sessions {
        let cwd = session.cwd.as_str();
        session.branch = sl_core::git::current_branch(Some(cwd));
        session.gh = store
            .as_ref()
            .zip(session.branch.as_deref())
            .and_then(|(store, branch)| gh::load(store, cwd, branch));
        session.linear = linear
            .zip(session.branch.as_deref())
            .and_then(|(cache, branch)| cache.cached(branch));
        session.project = projects
            .entry(session.cwd.clone())
            .or_insert_with(|| {
                sl_core::git::common_dir(cwd)
                    .map_or_else(|| dir_name(cwd), |dir| project_name(&dir))
            })
            .clone();
        // A Codex thread has no prompt cache to read, and its context use
        // comes from its own rollout.
        if session.provider != Source::Claude {
            continue;
        }
        session.cache = caches.read(&session.session_id);
        session.context = read_context(&sl_core::paths::cache_dir(), &session.session_id);
        // The title Claude Code's own session list shows, unless the user
        // named the session; the derived name is the directory's.
        if !session.pid.is_some_and(|pid| named.contains(&pid))
            && let Some(title) = caches.title(&session.session_id)
        {
            session.name = title;
        }
    }
    let open: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
    caches.retain(&open);
    projects.retain(|cwd, _| sessions.iter().any(|s| &s.cwd == cwd));
    sort_sessions(&mut sessions);
    Some(sessions)
}

/// What a pass hears from the network, as each part of it arrives.
#[derive(Debug)]
pub enum Arrival {
    /// Each directory's PR and CI, by directory; `None` where it has none.
    Gh(HashMap<String, Option<GhInfo>>),
    /// The tickets asked about, each `None` where the lookup failed.
    Linear(Vec<(String, Option<crate::linear::LinearInfo>)>),
}

/// Pairs to ask the network about: a directory and its branch, or a ticket
/// and a directory to ask from.
type Asks = Vec<(String, String)>;

/// What to ask the network about `sessions`: each directory with its branch,
/// and each ticket that is due, with a directory to ask from.
pub fn network_asks(
    sessions: &[OpenSession],
    linear: Option<&crate::linear::LinearCache>,
    now_ms: i64,
    pr_secs: u64,
) -> (Asks, Asks) {
    let mut dirs: Vec<(String, String)> = sessions
        .iter()
        .filter_map(|s| Some((s.cwd.clone(), s.branch.clone()?)))
        .collect();
    dirs.sort();
    dirs.dedup_by(|a, b| a.0 == b.0);
    let max_age = i64::try_from(pr_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(1000);
    let mut tickets: Vec<(String, String)> = linear
        .map(|cache| {
            sessions
                .iter()
                .filter_map(|s| {
                    Some((
                        sl_core::branch::ticket_id(s.branch.as_deref()?)?,
                        s.cwd.clone(),
                    ))
                })
                .filter(|(id, _)| cache.due(id, now_ms, max_age))
                .collect()
        })
        .unwrap_or_default();
    tickets.sort();
    tickets.dedup_by(|a, b| a.0 == b.0);
    (dirs, tickets)
}

/// Ask Linear about every ticket, all at once, and hand the answers to
/// `arrived`. GitHub is asked apart, by [`GithubRound`], so it never holds a
/// pass up.
pub fn ask_linear(
    tickets: &[(String, String)],
    ask: impl Fn(&str, &str) -> Option<crate::linear::LinearInfo> + Sync,
    arrived: impl FnOnce(Arrival),
) {
    if tickets.is_empty() {
        return;
    }
    let keys: std::collections::BTreeSet<&str> =
        tickets.iter().map(|(id, _)| id.as_str()).collect();
    let cwd_of: HashMap<&str, &str> = tickets
        .iter()
        .map(|(id, cwd)| (id.as_str(), cwd.as_str()))
        .collect();
    let found = each_dir(
        &keys,
        |id| cwd_of.get(id).and_then(|cwd| ask(id, cwd)),
        |_| None,
    );
    arrived(Arrival::Linear(found.into_iter().collect()));
}

/// The longest one round of GitHub requests may take, all of them together.
const GITHUB_ROUND: Duration = Duration::from_mins(1);

/// GitHub's part of the passes, on a thread of its own: the PRs of the
/// sessions' branches, then the stacks they are on. One round at a time; a
/// pass that comes while one is under way starts none. A round stores what
/// GitHub said (the next pass reads it back) and keeps the stacks it found,
/// then rings [`SESSIONS_BELL`] if anything changed, so it is shown at once.
#[derive(Clone, Default)]
struct GithubRound {
    busy: Arc<AtomicBool>,
    stacks: Arc<Mutex<Option<Vec<crate::stacks::StackView>>>>,
    cache: Arc<Mutex<crate::stacks::StackCache>>,
}

/// What a round is asked about.
struct RoundAsks {
    dirs: Vec<(String, String)>,
    sessions: Vec<OpenSession>,
    gone: Vec<crate::sticky::Remembered>,
    now_ms: i64,
    policy: gh::Policy,
    pr_age_ms: i64,
}

impl GithubRound {
    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Start a round unless one is under way. `fresh_ms`: see [`gh::sync`].
    fn start(&self, asks: RoundAsks, fresh_ms: Option<i64>) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let round = self.clone();
        let spawned = std::thread::Builder::new()
            .name("github".into())
            .spawn(move || {
                let changed = round.run(asks, fresh_ms);
                round.busy.store(false, Ordering::SeqCst);
                if changed {
                    SESSIONS_BELL.ring();
                }
            });
        if spawned.is_err() {
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    /// One round; whether it learned anything new.
    fn run(&self, asks: RoundAsks, fresh_ms: Option<i64>) -> bool {
        let RoundAsks {
            dirs,
            mut sessions,
            gone,
            now_ms,
            policy,
            pr_age_ms,
        } = asks;
        let budget = gh::api::Budget::new(GITHUB_ROUND);
        let post = |body: &str| gh::api::post_within(&budget, body);
        let before: Vec<Option<GhInfo>> = sessions.iter().map(|s| s.gh.clone()).collect();
        // A store that cannot be opened answers nothing, and what the
        // sessions show stays as it is. Without a token GitHub accepts, the
        // tray asks nothing (the status line asks by itself meanwhile, and
        // a claim taken only to fail would keep it out) and reads what is
        // stored.
        if let Some(store) = gh::Store::open_default() {
            let found = if gh::api::can_ask() {
                let seen = store.seen_since(now_ms - gh::sync::SEEN_FOR_MS);
                gh::sync(
                    &store,
                    &post,
                    &with_seen(dirs, seen),
                    now_ms,
                    &policy,
                    fresh_ms,
                )
            } else {
                dirs.iter()
                    .map(|(dir, branch)| (dir.clone(), gh::load(&store, dir, branch)))
                    .collect()
            };
            apply_arrival(&mut sessions, Arrival::Gh(found), None, now_ms);
        }
        let prs_changed = sessions.iter().map(|s| &s.gh).ne(before.iter());
        let found = {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::stacks::build(&sessions, &gone, &mut cache, now_ms, pr_age_ms, &budget)
        };
        let mut stacks = self
            .stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stacks_changed = stacks.as_ref() != Some(&found);
        *stacks = Some(found);
        prs_changed || stacks_changed
    }

    /// The stacks the last round found; `None` before the first.
    fn stacks(&self) -> Option<Vec<crate::stacks::StackView>> {
        self.stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The sessions' (directory, branch) pairs, then those the status line
/// showed in directories no session is in. Answers come back by directory,
/// so a session's own directory keeps its own branch: the status line may
/// have shown another there (Claude Code's worktree branch, or HEAD before
/// it moved).
fn with_seen(mut dirs: Asks, seen: Asks) -> Asks {
    for (dir, branch) in seen {
        if !dirs.iter().any(|(known, _)| *known == dir) {
            dirs.push((dir, branch));
        }
    }
    dirs
}

/// Fold `arrival` into `sessions` (and `linear`'s cache).
pub fn apply_arrival(
    sessions: &mut [OpenSession],
    arrival: Arrival,
    linear: Option<&mut crate::linear::LinearCache>,
    now_ms: i64,
) {
    match arrival {
        Arrival::Gh(by_dir) => {
            for session in sessions.iter_mut() {
                if let Some(info) = by_dir.get(&session.cwd) {
                    session.gh.clone_from(info);
                }
            }
        }
        Arrival::Linear(found) => {
            let Some(cache) = linear else { return };
            for (id, info) in found {
                cache.store(id, now_ms, info);
            }
            for session in sessions.iter_mut() {
                session.linear = session
                    .branch
                    .as_deref()
                    .and_then(|branch| cache.cached(branch));
            }
            cache.retain(
                &sessions
                    .iter()
                    .filter_map(|s| sl_core::branch::ticket_id(s.branch.as_deref()?))
                    .collect(),
            );
        }
    }
}

/// `resolve` for each of `dirs`, all at once; `failed` for one whose
/// thread panicked.
fn each_dir<T: Send>(
    dirs: &std::collections::BTreeSet<&str>,
    resolve: impl Fn(&str) -> T + Sync,
    failed: impl Fn(&str) -> T,
) -> HashMap<String, T> {
    std::thread::scope(|scope| {
        let resolve = &resolve;
        let running: Vec<_> = dirs
            .iter()
            .map(|&cwd| (cwd, scope.spawn(move || resolve(cwd))))
            .collect();
        running
            .into_iter()
            .map(|(cwd, thread)| {
                (
                    cwd.to_owned(),
                    thread.join().unwrap_or_else(|_| failed(cwd)),
                )
            })
            .collect()
    })
}

/// The context capture the status line wrote for `session_id`, if any.
fn read_context(cache_dir: &std::path::Path, session_id: &str) -> Option<ContextCapture> {
    let path = sl_core::paths::context_file(cache_dir, session_id)?;
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// What one poll found: the sessions running, those that were and are not
/// any more, and the PR stacks either kind is on. Published whole, so a
/// reader never sees one poll's open sessions beside another's closed ones.
#[derive(Debug, Clone, Default)]
pub struct Published {
    pub open: Vec<OpenSession>,
    pub closed: Vec<crate::sticky::Remembered>,
    pub stacks: Vec<crate::stacks::StackView>,
}

/// The latest [`Published`].
pub type Board = crate::latest::Latest<Published>;

/// Spawn the polling thread. Runs for the life of the process, like
/// `refresh::spawn`.
/// `sticky` remembers every session seen running; those no longer running are
/// published as closed, never mixed into the open ones.
pub fn spawn(
    board: Board,
    auto_awake: crate::awake::AutoCaffeinate,
    sticky: Arc<Mutex<crate::sticky::Sticky>>,
) {
    let spawned = std::thread::Builder::new()
        .name("sl-tray-sessions".to_owned())
        .spawn(move || {
            let mut notifier = Notifier::default();
            let mut pr_watcher = PrWatcher::default();
            let mut caches = CacheReader::default();
            let mut linear = crate::linear::LinearCache::default();
            let mut codex = crate::codex_sessions::CodexSessions::default();
            let mut exists = crate::sticky::ExistsCache::default();
            let github = GithubRound::default();
            let mut stack_watcher = crate::notify::StackWatcher::default();
            let mut claude = ClaudeLister::default();
            let mut projects: HashMap<String, String> = HashMap::new();
            // Rings when a session starts, ends or changes status: Claude Code
            // rewrites its record each time.
            let mut records = sl_usage::watch::Watch::with_bell(Some(|| SESSIONS_BELL.ring()));
            let records_dir = [records_dir()];
            // When the remembered list was last written; times alone are saved
            // at most this often.
            let mut saved_at = 0_i64;
            let notify_enabled = std::env::var_os("SL_TRAY_NO_NOTIFY").is_none();
            let awake_enabled = awake_enabled();
            let mut awake = crate::awake::KeepAwake::default();
            loop {
                SESSIONS_BELL.answer();
                // Only to keep watching (and start, once the directory exists):
                // what changed is read in full anyway.
                drop(records.take(&records_dir));
                let now = crate::snapshot::now_ms();
                let poll = crate::view_prefs::load_poll();
                let show_linear =
                    crate::view_prefs::load_switch(crate::view_prefs::Switch::ShowLinear);
                let previous = crate::latest::get(&board);
                let pr_age = i64::try_from(poll.pr_secs)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1000);
                if let Some(mut sessions) = load(
                    &mut caches,
                    show_linear.then_some(&linear),
                    Some(&mut codex),
                    &mut claude,
                    &previous.open,
                    &mut projects,
                ) {
                    note_any_open(!sessions.is_empty());
                    if awake_enabled {
                        let by = Awake::now(auto_awake.load(Ordering::Relaxed));
                        awake.set(sessions.iter().any(|s| by.holds(s)));
                    }
                    let live_ids: Vec<String> =
                        sessions.iter().map(|s| s.session_id.clone()).collect();
                    let live_ids: Vec<&str> = live_ids.iter().map(String::as_str).collect();
                    // Once ever: sessions from before the tray first ran.
                    let backfill = (!crate::sticky::backfill_done())
                        .then(|| crate::sticky::backfill(&live_ids, now));
                    let gone = {
                        let mut sticky = sticky
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(found) = backfill {
                            crate::trace(&format!(
                                "backfilled {} closed sessions from transcripts",
                                found.len()
                            ));
                            sticky.adopt(found);
                            sticky.save();
                            crate::sticky::mark_backfill_done();
                        }
                        let mut changed = sticky.observe(&sessions, now);
                        changed |= sticky.prune(&live_ids, now, |e| exists.exists(e, now));
                        if changed || now - saved_at >= STICKY_SAVE_EVERY_MS {
                            sticky.save();
                            saved_at = now;
                        }
                        sticky.closed(&live_ids, now, |e| exists.exists(e, now))
                    };
                    // What is open is shown now, from local files; the network
                    // fills in the rest as each part of it answers, the stacks
                    // (which need the PRs) last.
                    publish(&board, &sticky, &sessions, &gone, previous.stacks.clone());
                    crate::trace(&format!("published {} open sessions", sessions.len()));
                    let (dirs, tickets) =
                        network_asks(&sessions, show_linear.then_some(&linear), now, poll.pr_secs);
                    // GitHub, on its own thread, one round at a time; what it
                    // learns is stored, and shown from the next pass on. The
                    // PR setting is how often an active branch (pushed lately,
                    // its checks running) is asked about. A look (the menu
                    // opening) waits for a round that can take it.
                    if !github.is_busy() {
                        let fresh = LOOKED
                            .swap(false, Ordering::Relaxed)
                            .then_some(LOOK_FRESH_MS);
                        github.start(
                            RoundAsks {
                                dirs,
                                sessions: sessions.clone(),
                                gone: gone.clone(),
                                now_ms: now,
                                policy: gh::Policy::active_every(pr_age),
                                pr_age_ms: pr_age,
                            },
                            fresh,
                        );
                    }
                    ask_linear(&tickets, crate::linear::fetch, |arrival| {
                        crate::trace("arrived: Linear");
                        apply_arrival(
                            &mut sessions,
                            arrival,
                            show_linear.then_some(&mut linear),
                            now,
                        );
                        publish(&board, &sticky, &sessions, &gone, previous.stacks.clone());
                    });
                    let found = github.stacks().unwrap_or_else(|| previous.stacks.clone());
                    // Read each pass, so a change in Settings applies at once.
                    let wanted = crate::view_prefs::load_notify();
                    let tuning = crate::view_prefs::load_tuning();
                    let watched: Vec<Watched> = sessions
                        .iter()
                        .map(|s| Watched {
                            session_id: &s.session_id,
                            name: &s.name,
                            busy: s.activity == Activity::Busy,
                            cache: s.cache,
                        })
                        .collect();
                    let prs: Vec<PrWatch> = sessions
                        .iter()
                        .filter_map(|s| {
                            Some(PrWatch {
                                project: &s.project,
                                info: s.gh.as_ref()?,
                            })
                        })
                        .collect();
                    let cache_news = notifier
                        .due(&watched, now, tuning.cache_lead_mins)
                        .into_iter()
                        .filter(|_| wanted.cache_expiry);
                    let news: Vec<(String, String)> = cache_news
                        .chain(pr_watcher.due(&prs, wanted, now))
                        .chain(stack_watcher.due(&found, wanted.stack_rebase, wanted.pr_merged))
                        .collect();
                    for (title, body) in news {
                        crate::trace(&format!("notify: {title}: {body}"));
                        if notify_enabled {
                            notify::send(&title, &body);
                        }
                    }
                    publish(&board, &sticky, &sessions, &gone, found);
                }
                nap(|| {
                    SESSIONS_BELL.nap_secs(
                        SESSIONS_GAP,
                        if ANY_OPEN.load(Ordering::Relaxed) {
                            SESSIONS_BACKSTOP
                        } else {
                            IDLE_SESSIONS_CAP
                        },
                    )
                });
            }
        });
    if let Err(error) = spawned {
        crate::log(&format!("could not spawn the sessions thread: {error}"));
    }
}

/// Publish what a pass has so far: the open sessions, the closed ones still
/// remembered, the stacks. Under `sticky`, as a Dismiss takes `sticky` first
/// too, so it lands either before this (and is filtered out here) or after
/// (and removes it from what this publishes) -- never in between, to be
/// undone.
fn publish(
    board: &Board,
    sticky: &Mutex<crate::sticky::Sticky>,
    open: &[OpenSession],
    gone: &[crate::sticky::Remembered],
    stacks: Vec<crate::stacks::StackView>,
) {
    let sticky = sticky
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let closed = gone
        .iter()
        .filter(|entry| sticky.remembers(&entry.session_id))
        .cloned()
        .collect();
    crate::latest::set(
        board,
        Published {
            open: open.to_vec(),
            closed,
            stacks,
        },
    );
}

pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// How a piece of a menu row is coloured; mapped to a real colour by the
/// renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    Plain,
    Dim,
    Green,
    Yellow,
    /// Between yellow and red: wants attention soon. Reads on light and dark
    /// menus alike, where yellow text is faint on light ones.
    Orange,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub tone: Tone,
    /// How big it is drawn beside the rest of its line.
    pub size: SegSize,
}

/// A segment's size, by what it is rather than by guessing from its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SegSize {
    /// The line's own size.
    #[default]
    Line,
    /// A step smaller, after a title: a row's PR number.
    Smaller,
    /// Score dots, drawn small.
    Dots,
}

impl Seg {
    /// The same, a step smaller than its line.
    pub fn smaller(self) -> Self {
        Self {
            size: SegSize::Smaller,
            ..self
        }
    }

    /// The same, drawn as score dots.
    pub fn dots(self) -> Self {
        Self {
            size: SegSize::Dots,
            ..self
        }
    }
}

pub fn seg(text: impl Into<String>, tone: Tone) -> Seg {
    Seg {
        text: text.into(),
        tone,
        size: SegSize::Line,
    }
}

/// Which status icon a session's row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Working: its cache is being kept warm.
    Busy,
    /// Working, and holding the Mac awake (Auto caffeinate is on).
    Caffeinated,
    Idle,
    /// Idle, and the cache runs out within five minutes.
    Expiring,
    /// Idle, and the cache has already expired.
    Cold,
    /// Idle, and its PR's CI has failed.
    Failed,
    /// No longer running: remembered so it can be resumed.
    Closed,
    /// A stack of PRs, not a session.
    Stack,
}

/// The colour of a usage window's bar and pace line: red while it is ahead of
/// an even burn (heading past the limit before the reset) or nearly spent,
/// green otherwise.
pub fn window_tone(used_percent: f64, pace: Option<f64>) -> Tone {
    if used_percent >= 85.0 {
        Tone::Red
    } else if pace.is_some_and(|pace| pace.round() > 0.0) {
        Tone::Orange
    } else {
        Tone::Green
    }
}

/// `15% behind pace`, `28% ahead of pace` or `On pace`, in percentage points
/// of the window. Rounded the same way it is shown, so `0.4` ahead is on pace.
pub fn pace_text(pace: f64) -> String {
    let points = pace.round();
    if points > 0.0 {
        format!("{points:.0}% ahead of pace")
    } else if points < 0.0 {
        format!("{:.0}% behind pace", -points)
    } else {
        "On pace".to_owned()
    }
}

/// What the menu bar icon's badge says about the open sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarState {
    /// No session open: the icon is drawn dimmed, with no badge.
    NoSessions,
    Idle,
    Working,
    /// The tray is holding the Mac awake for a session.
    Caffeinated,
}

/// The two switches that keep the Mac awake for a session: "Auto
/// caffeinate" (`auto`, for one that is working) and Settings' `/rc` switch
/// (`rc`, for one with remote control on, busy or idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Awake {
    pub auto: bool,
    pub rc: bool,
}

impl Awake {
    /// `auto` as the tray holds it, and the `/rc` switch as Settings has it.
    pub fn now(auto: bool) -> Self {
        Self {
            auto,
            rc: crate::view_prefs::load_switch(crate::view_prefs::Switch::RcAwake),
        }
    }

    /// Whether the tray holds the Mac awake for `session`. The one
    /// definition, shared by the poller that runs `caffeinate` and the badge
    /// that reports it.
    pub fn holds(self, session: &OpenSession) -> bool {
        (self.rc && session.remote_control.is_some())
            || (self.auto && session.activity == Activity::Busy)
    }
}

/// The badge for the open sessions as a whole: caffeinated over working over
/// idle, since the more active state is the one worth seeing at a glance.
/// `awake_enabled` is `false` under `SL_TRAY_NO_CAFFEINATE`, when nothing is
/// actually held awake and the cup would be a lie.
pub fn bar_state(sessions: &[OpenSession], awake: Awake, awake_enabled: bool) -> BarState {
    if sessions.is_empty() {
        BarState::NoSessions
    } else if awake_enabled && sessions.iter().any(|s| awake.holds(s)) {
        BarState::Caffeinated
    } else if sessions.iter().any(|s| s.activity == Activity::Busy) {
        BarState::Working
    } else {
        BarState::Idle
    }
}

/// `SL_TRAY_NO_CAFFEINATE` unset: the tray may hold the Mac awake.
pub fn awake_enabled() -> bool {
    std::env::var_os("SL_TRAY_NO_CAFFEINATE").is_none()
}

pub fn session_icon(session: &OpenSession, now_ms: i64, keep_awake: bool) -> Icon {
    if session.activity == Activity::Busy {
        return if keep_awake {
            Icon::Caffeinated
        } else {
            Icon::Busy
        };
    }
    match session.cache.and_then(|cache| cache.remaining_ms(now_ms)) {
        Some(left) if left <= 0 => Icon::Cold,
        Some(left) if left <= 5 * 60_000 => Icon::Expiring,
        _ => Icon::Idle,
    }
}

/// A Regular menu row's title: the name, and the PR number after it.
pub fn row_title(session: &OpenSession) -> Vec<Seg> {
    let mut segs = vec![seg(&session.name, Tone::Plain)];
    if let Some(pr) = session.gh.as_ref().and_then(|g| g.pr_number) {
        segs.push(seg(format!("  #{pr}"), Tone::Dim).smaller());
    }
    segs
}

/// A Regular menu row's second line:
/// `datanaut · dn-479-base · draft · ✗ CI failed · idle 52m`.
pub fn row_detail(session: &OpenSession, now_ms: i64) -> Vec<Seg> {
    let mut details: Vec<Seg> = vec![seg(&session.project, Tone::Dim)];
    if let Some(branch) = &session.branch {
        details.push(seg(truncate(branch, BRANCH_WIDTH), Tone::Dim));
    }
    if let Some(info) = session.gh.as_ref() {
        if info.pr_number.is_some() {
            if info.is_draft == Some(true) {
                details.push(seg("draft", Tone::Yellow));
            }
            if let Some((text, tone)) = verdict_text(info) {
                details.push(seg(text.to_lowercase(), tone));
            }
        }
        if let Some(ci) = &info.ci {
            let old_for = info.old_for_ms(now_ms);
            let mut ci = match ci.state {
                CiState::Passing => seg("✓", Tone::Green),
                CiState::Pending => seg("CI …", Tone::Yellow),
                CiState::Failing => seg(
                    format!(
                        "✗ CI: {}",
                        ci.failed.first().map_or("failed", |job| job.name.as_str())
                    ),
                    Tone::Red,
                ),
            };
            // Long past when it was due again: nothing is asking GitHub.
            if let Some(age) = old_for {
                ci = seg(
                    format!("{} {} old", ci.text, sl_core::format::ago_short(age)),
                    Tone::Dim,
                );
            }
            details.push(ci);
        }
    }
    if session.remote_control.is_some() {
        details.push(seg("rc", Tone::Green));
    }
    if session.activity == Activity::Busy {
        details.push(seg("working", Tone::Dim));
    } else if let Some(since) = session
        .cache
        .map(|cache| cache.last_activity_ms)
        .or(session.last_activity_ms)
    {
        details.push(seg(
            format!(
                "idle {}",
                sl_core::format::fmt_elapsed((now_ms - since).max(0))
            ),
            Tone::Dim,
        ));
    }
    let mut segs = Vec::new();
    for (i, detail) in details.into_iter().enumerate() {
        if i > 0 {
            segs.push(seg(" · ", Tone::Dim));
        }
        segs.push(detail);
    }
    segs
}

/// A Regular menu row's right-hand column: what is left of the cache
/// (`7m left`, orange then red as it runs out), `warm` while working.
pub fn row_right(session: &OpenSession, now_ms: i64, tuning: &Tuning) -> Option<(String, Tone)> {
    // Warm, cold and the time left are the prompt cache's, which only Claude
    // Code's sessions have.
    if session.provider != Source::Claude {
        return None;
    }
    if session.activity == Activity::Busy {
        return Some(("warm".to_owned(), Tone::Dim));
    }
    let cache = session.cache?;
    if cache.compacted {
        return Some(("compacted".to_owned(), Tone::Dim));
    }
    let left = cache.remaining_ms(now_ms)?;
    if left <= 0 {
        return Some(("expired".to_owned(), Tone::Dim));
    }
    let tone = match cache_tone(left, tuning.needs_you_ms) {
        Tone::Plain => Tone::Dim,
        other => other,
    };
    Some((format!("{} left", sl_core::format::fmt_elapsed(left)), tone))
}

/// The steps a context ring is drawn in, so the icons are a small, reusable set.
pub const RING_STEPS: u8 = 24;

/// The glyph for a session's state, whatever its context: [`session_icon`],
/// except that an idle session whose CI failed shows that.
pub fn state_glyph(session: &OpenSession, now_ms: i64, keep_awake: bool) -> Icon {
    let icon = session_icon(session, now_ms, keep_awake);
    if matches!(icon, Icon::Idle | Icon::Expiring) && ci_failing(session) {
        Icon::Failed
    } else {
        icon
    }
}

/// How full a session's context is, as a ring: filled steps of
/// [`RING_STEPS`] and a colour as it nears the limit. `None` until the
/// status line has reported on the session. Orange from `tuning.ring_note`,
/// red from `tuning.ring_warn` (the status line's own `⚠` is at 85 whatever
/// these are).
pub fn context_ring(session: &OpenSession, tuning: &Tuning) -> Option<(u8, Tone)> {
    let context = session.context?;
    let share = (context.percent / 100.0).clamp(0.0, 1.0);
    let used = sl_core::num::trunc_u8((share * f64::from(RING_STEPS)).ceil()).max(1);
    let tone = if context.percent >= f64::from(tuning.ring_warn) {
        Tone::Red
    } else if context.percent >= f64::from(tuning.ring_note) {
        Tone::Orange
    } else {
        Tone::Plain
    };
    Some((used, tone))
}

/// How urgent a cache countdown is: red in its last five minutes, orange in
/// the ten before, plain otherwise.
pub fn cache_tone(left_ms: i64, needs_you_ms: i64) -> Tone {
    if left_ms <= 5 * 60_000 {
        Tone::Red
    } else if left_ms <= needs_you_ms {
        Tone::Orange
    } else {
        Tone::Plain
    }
}

fn ci_failing(session: &OpenSession) -> bool {
    session
        .gh
        .as_ref()
        .and_then(|info| info.ci.as_ref())
        .is_some_and(|ci| ci.state == CiState::Failing)
}

/// An idle session's cache running out within this long is worth a look now:
/// a turn before it expires is far cheaper than one after. The setting's
/// default: the tests pin the old behaviour with it.
#[cfg(test)]
pub const NEEDS_YOU_CACHE_MS: i64 = 15 * 60_000;

/// Which group a session is listed under, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    /// Idle, and worth acting on now: its cache is about to run out, its CI
    /// failed, or a reviewer asked for changes.
    NeedsYou,
    Working,
    Idle,
    /// Idle, its cache already gone: the next turn pays for it in full.
    Cold,
}

impl Urgency {
    pub fn label(self) -> &'static str {
        match self {
            Urgency::NeedsYou => "Needs you",
            Urgency::Working => "Working",
            Urgency::Idle => "Idle",
            Urgency::Cold => "Cold",
        }
    }
}

pub fn urgency(session: &OpenSession, now_ms: i64, tuning: &Tuning) -> Urgency {
    if session.activity == Activity::Busy {
        return Urgency::Working;
    }
    let changes_requested = session.gh.as_ref().is_some_and(|info| {
        info.pr_number.is_some() && info.verdict() == Some(Verdict::ChangesRequested)
    });
    if ci_failing(session) || changes_requested {
        return Urgency::NeedsYou;
    }
    match session.cache.and_then(|cache| cache.remaining_ms(now_ms)) {
        Some(left) if left <= 0 => Urgency::Cold,
        Some(left) if left <= tuning.needs_you_ms => Urgency::NeedsYou,
        _ => Urgency::Idle,
    }
}

/// The sessions in the order the Regular menu lists them, each with its group.
///
/// Fresh (`frozen` is `None`): by urgency, keeping `sessions`' own order within
/// a group. While the menu is open the order must not move under the pointer,
/// so `frozen` -- the order it opened with -- is kept: each entry takes its
/// session's latest state, a session that has closed keeps its last one, and
/// a session that opened since goes at the end.
pub fn arrange(
    sessions: &[OpenSession],
    frozen: Option<&[(OpenSession, Urgency)]>,
    now_ms: i64,
    tuning: &Tuning,
) -> Vec<(OpenSession, Urgency)> {
    let Some(frozen) = frozen else {
        let mut out: Vec<(OpenSession, Urgency)> = sessions
            .iter()
            .map(|s| (s.clone(), urgency(s, now_ms, tuning)))
            .collect();
        // Stable, so the project order `sessions` arrives in stands within a
        // group unless another order is chosen.
        match tuning.sort {
            SortOrder::Project => out.sort_by_key(|(_, group)| *group),
            SortOrder::Recent => {
                out.sort_by_key(|(s, group)| (*group, std::cmp::Reverse(last_touched(s))));
            }
            SortOrder::Started => {
                out.sort_by_key(|(s, group)| (*group, std::cmp::Reverse(s.started_ms)));
            }
        }
        return out;
    };
    let mut out: Vec<(OpenSession, Urgency)> = frozen
        .iter()
        .map(|(old, group)| {
            (
                sessions
                    .iter()
                    .find(|s| s.session_id == old.session_id)
                    .unwrap_or(old)
                    .clone(),
                *group,
            )
        })
        .collect();
    for session in sessions {
        if !frozen
            .iter()
            .any(|(old, _)| old.session_id == session.session_id)
        {
            // At the end of its own group, not of the list: appended after a
            // later group it would repeat its group's caption below it.
            let group = urgency(session, now_ms, tuning);
            let at = out
                .iter()
                .rposition(|(_, other)| *other <= group)
                .map_or(0, |i| i + 1);
            out.insert(at, (session.clone(), group));
        }
    }
    out
}

/// When a session was last busy: its last message, else the last time its
/// transcript moved, else when it started.
pub fn last_touched(session: &OpenSession) -> i64 {
    session
        .cache
        .map(|cache| cache.last_activity_ms)
        .or(session.last_activity_ms)
        .unwrap_or(session.started_ms)
}

/// Why a session shows no PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoPr {
    NotARepository,
    DetachedHead,
    /// The default branch is not where a PR comes from.
    DefaultBranch,
    /// A branch with none open yet.
    NoneYet,
    /// An old "none" that is being asked about again: it may have one now.
    Checking,
    /// Not asked about, as no GitHub token is stored.
    NoToken,
    /// Not asked about, as GitHub refused the token (expired, revoked).
    TokenRefused,
}

impl NoPr {
    /// As a session's detail line says it.
    pub fn text(self) -> &'static str {
        match self {
            NoPr::NotARepository => "Not a git repository",
            NoPr::DetachedHead => "Detached HEAD · no PR",
            NoPr::DefaultBranch => "Default branch · no PR",
            NoPr::NoneYet => "No open PR for this branch",
            NoPr::Checking => "Checking for a PR…",
            NoPr::NoToken => "No GitHub token · add one in Settings",
            NoPr::TokenRefused => "GitHub token expired or revoked · replace it in Settings",
        }
    }

    /// As the title of a session's submenu says it.
    pub fn title(self) -> &'static str {
        match self {
            NoPr::NoneYet => "No PR yet",
            NoPr::Checking => "Checking PR…",
            NoPr::NoToken => "No GitHub token",
            NoPr::TokenRefused => "GitHub token expired",
            other => other.text(),
        }
    }
}

/// Why `session` has no PR to show; `None` when it has one.
pub fn no_pr(session: &OpenSession) -> Option<NoPr> {
    if session
        .gh
        .as_ref()
        .is_some_and(|info| info.pr_number.is_some())
    {
        return None;
    }
    Some(match session.branch.as_deref() {
        None => NoPr::NotARepository,
        Some(branch) if branch.starts_with('@') => NoPr::DetachedHead,
        Some(branch) if !gh::worth_fetching(branch) => NoPr::DefaultBranch,
        // Not asked about yet, and no way to ask.
        Some(_) if session.gh.is_none() && gh::api::token_missing() => NoPr::NoToken,
        Some(_) if session.gh.is_none() && gh::api::token_refused() => NoPr::TokenRefused,
        // Not asked about yet: the first answer is on its way.
        Some(_) if session.gh.is_none() => NoPr::Checking,
        Some(_) => NoPr::NoneYet,
    })
}

/// `DN-479 · In Progress`: the ticket, and where it stands.
pub fn linear_segments(info: &crate::linear::LinearInfo) -> Vec<Seg> {
    vec![
        seg(format!("{} · ", info.id), Tone::Dim),
        seg(info.state.clone(), crate::linear::state_tone(&info.state)),
    ]
}

/// `→ base`, then `tip · stacked on #parent` under it, only when the PR targets
/// a non-default base. A long base is cut in the middle, where a branch name
/// says least, so the submenu does not stretch to fit it.
pub fn base_segments(info: &GhInfo) -> Option<Vec<Seg>> {
    let base = info
        .base
        .as_deref()
        .filter(|b| gh::is_non_default_base(b))?;
    let mut segs = vec![seg(
        format!("→ {}", truncate_middle(base, BASE_WIDTH)),
        Tone::Dim,
    )];
    let below: Vec<String> = info
        .base_sha
        .iter()
        .cloned()
        .chain(info.base_pr.map(|pr| format!("stacked on #{pr}")))
        .collect();
    if !below.is_empty() {
        segs.push(seg(format!("\n{}", below.join(" · ")), Tone::Dim));
    }
    Some(segs)
}

/// Base branch names are cut to this many characters.
const BASE_WIDTH: usize = 44;

/// `text` cut to `width` characters by an ellipsis in the middle.
pub fn truncate_middle(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let head = (width - 1).div_ceil(2);
    let tail = width - 1 - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - tail).collect();
    format!("{start}…{end}")
}

fn review_state(review: &Review) -> Seg {
    match review.state {
        ReviewState::Approved => seg("approved", Tone::Green),
        ReviewState::ChangesRequested => seg("changes requested", Tone::Red),
        ReviewState::Commented => seg("commented", Tone::Dim),
        ReviewState::Dismissed => seg("dismissed", Tone::Dim),
        ReviewState::Other => seg("reviewed", Tone::Dim),
    }
}

/// ` · risk 4/5 · quality 4/5`. A high risk is bad and a high quality is good,
/// so the two are coloured in opposite directions.
/// A score's colour: green when it is good, red when it is bad, yellow
/// between. Risk is good when low, quality when high.
pub fn score_tone(score: Score, high_is_good: bool) -> Tone {
    let share = f64::from(score.value) / f64::from(score.max.max(1));
    let good = if high_is_good {
        share >= 0.8
    } else {
        share <= 0.4
    };
    let bad = if high_is_good {
        share <= 0.4
    } else {
        share >= 0.8
    };
    if good {
        Tone::Green
    } else if bad {
        Tone::Red
    } else {
        Tone::Yellow
    }
}

fn score_segments(assessment: &Assessment) -> Vec<Seg> {
    let mut out = Vec::new();
    for (label, score, high_is_good) in [
        ("risk", assessment.risk, false),
        ("quality", assessment.quality, true),
    ] {
        if let Some(score) = score {
            out.push(seg(" · ", Tone::Dim));
            out.push(seg(
                format!("{label} {}/{}", score.value, score.max),
                score_tone(score, high_is_good),
            ));
        }
    }
    out
}

/// The review outcome in a word, with its colour, when there is one.
pub fn verdict_text(info: &GhInfo) -> Option<(&'static str, Tone)> {
    match info.verdict()? {
        Verdict::Approved => Some(("Approved", Tone::Green)),
        Verdict::ChangesRequested => Some(("Changes requested", Tone::Red)),
    }
}

/// One entry per reviewer: who reviewed and what they said, then who has been
/// asked and has not answered. At most [`MAX_REVIEWERS`]. The name is `None`
/// for the "no reviews yet" placeholder, which has nobody to show feedback of.
pub fn reviewer_entries(info: &GhInfo) -> Vec<(Option<String>, Vec<Seg>)> {
    let mut rows: Vec<(Option<String>, Vec<Seg>)> = info
        .reviews
        .iter()
        .map(|review| {
            let mut segs = vec![
                seg(format!("{} ", review.author), Tone::Plain),
                review_state(review),
            ];
            if let Some(assessment) = info
                .assessment
                .as_ref()
                .filter(|a| a.author == review.author)
            {
                segs.extend(score_segments(assessment));
            }
            (Some(review.author.clone()), segs)
        })
        .collect();
    // A score posted as a plain comment, by someone who left no review.
    if let Some(assessment) = info
        .assessment
        .as_ref()
        .filter(|a| !info.reviews.iter().any(|r| r.author == a.author))
    {
        let mut segs = vec![
            seg(format!("{} ", assessment.author), Tone::Plain),
            seg("scored", Tone::Dim),
        ];
        segs.extend(score_segments(assessment));
        rows.push((Some(assessment.author.clone()), segs));
    }
    let answered: Vec<&str> = info.reviews.iter().map(|r| r.author.as_str()).collect();
    rows.extend(
        info.requested
            .iter()
            .filter(|name| !answered.contains(&name.as_str()))
            .map(|name| {
                (
                    Some(name.clone()),
                    vec![
                        seg(format!("{name} "), Tone::Plain),
                        seg("review requested", Tone::Yellow),
                    ],
                )
            }),
    );
    if rows.is_empty() {
        rows.push((None, vec![seg("No reviews yet", Tone::Dim)]));
    }
    rows.truncate(MAX_REVIEWERS);
    rows
}

/// [`reviewer_entries`] as the submenu shows them: `claude commented   Risk
/// ●●●●○   Quality ●●●●○`, the scores as dots coloured by how good they are.
pub fn reviewer_lines(info: &GhInfo) -> Vec<(Option<String>, Vec<Seg>)> {
    reviewer_entries(info)
        .into_iter()
        .map(|(name, segs)| {
            let Some(author) = name.clone() else {
                return (name, segs);
            };
            let mut out = vec![seg(format!("{author} "), Tone::Plain)];
            // The state, without the text scores that follow it.
            if let Some(state) = segs.get(1) {
                out.push(Seg {
                    tone: if state.tone == Tone::Plain {
                        Tone::Dim
                    } else {
                        state.tone
                    },
                    ..state.clone()
                });
            }
            if let Some(assessment) = info.assessment.as_ref().filter(|a| a.author == author) {
                for (label, score, high_is_good) in [
                    ("Risk", assessment.risk, false),
                    ("Quality", assessment.quality, true),
                ] {
                    if let Some(score) = score {
                        let filled = usize::from(score.value.min(score.max));
                        let empty = usize::from(score.max) - filled;
                        out.push(seg(format!("   {label} "), Tone::Dim));
                        out.push(seg("●".repeat(filled), score_tone(score, high_is_good)).dots());
                        out.push(seg("○".repeat(empty), Tone::Dim).dots());
                    }
                }
            }
            (name, out)
        })
        .collect()
}

#[cfg(test)]
fn plain(segs: &[Seg]) -> String {
    segs.iter().map(|seg| seg.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_core::gh::{Ci, FailedJob};

    #[test]
    fn every_directory_is_resolved_at_once() {
        let dirs: std::collections::BTreeSet<&str> = ["/a", "/b", "/c"].into_iter().collect();
        // Each waits (up to 5 s) for all three to be running at once.
        let running = std::sync::atomic::AtomicUsize::new(0);
        let found = each_dir(
            &dirs,
            |cwd| {
                running.fetch_add(1, Ordering::SeqCst);
                let start = Instant::now();
                while running.load(Ordering::SeqCst) < 3
                    && start.elapsed() < std::time::Duration::from_secs(5)
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                format!("{cwd} beside {}", running.load(Ordering::SeqCst))
            },
            |cwd| format!("{cwd} failed"),
        );
        assert_eq!(
            found.get("/b").map(String::as_str),
            Some("/b beside 3"),
            "all three ran together: {found:?}"
        );
        assert_eq!(found.len(), 3);
    }

    /// A reader keeps the whole poll it took, open and closed alike, while
    /// an edit swaps in a changed copy for the next reader.
    #[test]
    fn a_reader_keeps_the_poll_it_took_while_an_edit_swaps_in_a_new_one() {
        let board: Board = crate::latest::new(Published {
            open: vec![session("a", Activity::Idle), session("b", Activity::Busy)],
            ..Published::default()
        });
        let before = crate::latest::get(&board);
        crate::latest::edit(&board, |published| published.open.retain(|s| s.name != "a"));
        let names = |p: &Published| p.open.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&before), ["a", "b"], "the reader's copy is untouched");
        assert_eq!(
            names(&crate::latest::get(&board)),
            ["b"],
            "the next reader sees the edit"
        );
    }

    /// A change of setting ends a long pause at once: within 100 ms, where it
    /// used to take up to a second.
    #[test]
    fn a_nap_ends_as_soon_as_a_change_shortens_it() {
        let wanted = Arc::new(std::sync::atomic::AtomicU64::new(3600));
        let napping = Arc::clone(&wanted);
        let start = Instant::now();
        let sleeper = std::thread::spawn(move || nap(|| napping.load(Ordering::SeqCst)));
        std::thread::sleep(Duration::from_millis(50));
        wanted.store(0, Ordering::SeqCst);
        wake();
        sleeper.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "woke at once: {:?}",
            start.elapsed()
        );
    }

    /// Only a change of "any session open" wakes the naps.
    #[test]
    fn sessions_opening_or_all_closing_wakes_the_naps() {
        let start = ANY_OPEN.load(Ordering::Relaxed);
        let before = *WAKE.0.lock().unwrap();
        assert!(note_any_open(!start), "a flip wakes");
        assert!(*WAKE.0.lock().unwrap() > before, "and the naps see it");
        assert!(note_any_open(start), "and back");
        assert!(
            !note_any_open(start) && !note_any_open(start),
            "no change, no wake"
        );
        assert_eq!(ANY_OPEN.load(Ordering::Relaxed), start);
    }

    /// Unwoken, it sleeps the whole interval rather than asking every second.
    #[test]
    fn a_nap_sleeps_its_interval_without_asking_again() {
        let wakes = || *WAKE.0.lock().unwrap();
        let asked = std::sync::atomic::AtomicU64::new(0);
        let (before, start) = (wakes(), Instant::now());
        nap(|| {
            asked.fetch_add(1, Ordering::SeqCst);
            1
        });
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "the whole second: {:?}",
            start.elapsed()
        );
        // Once to start, once when the wait ends, and once per wake meanwhile
        // (tests running alongside save settings, which wakes it).
        let woken = wakes() - before;
        assert!(
            asked.load(Ordering::SeqCst) <= 2 + woken,
            "asked {} times for {woken} wakes",
            asked.load(Ordering::SeqCst)
        );
    }

    // The behaviour before these were settings: the tests below pin it, with
    // the default tuning.
    fn urgency(session: &OpenSession, now_ms: i64) -> Urgency {
        super::urgency(session, now_ms, &Tuning::default())
    }

    fn row_right(session: &OpenSession, now_ms: i64) -> Option<(String, Tone)> {
        super::row_right(session, now_ms, &Tuning::default())
    }

    fn context_ring(session: &OpenSession) -> Option<(u8, Tone)> {
        super::context_ring(session, &Tuning::default())
    }

    fn arrange(
        sessions: &[OpenSession],
        frozen: Option<&[(OpenSession, Urgency)]>,
        now_ms: i64,
    ) -> Vec<(OpenSession, Urgency)> {
        super::arrange(sessions, frozen, now_ms, &Tuning::default())
    }

    fn session(name: &str, activity: Activity) -> OpenSession {
        OpenSession {
            session_id: name.into(),
            cache: None,
            name: name.into(),
            project: "p".into(),
            cwd: "/a/b".into(),
            activity,
            started_ms: 0,
            branch: None,
            gh: None,
            remote_control: None,
            pid: None,
            context: None,
            linear: None,
            provider: Source::Claude,
            last_activity_ms: None,
        }
    }

    #[test]
    fn agents_json_parses_and_busy_sessions_sort_first() {
        let sessions = parse_agents(
            r#"[{"cwd":"/a/old","name":"old","status":"idle","startedAt":1},
                {"cwd":"/a/new","name":"new","status":"idle","startedAt":5},
                {"cwd":"/a/work","name":"work","status":"busy","startedAt":2,"pid":7}]"#,
        )
        .unwrap();
        let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["work", "new", "old"]);
        assert_eq!(sessions[0].activity, Activity::Busy);
    }

    #[test]
    fn a_missing_name_falls_back_to_the_directory() {
        let sessions = parse_agents(r#"[{"cwd":"/Users/x/proj/","status":"weird"}]"#).unwrap();
        assert_eq!(sessions[0].name, "proj");
        assert_eq!(sessions[0].activity, Activity::Other);
        assert_eq!(parse_agents("not json"), None);
    }

    #[test]
    fn the_row_shows_project_branch_draft_and_ci_and_the_title_the_pr() {
        let detail = |s: &OpenSession| plain(&row_detail(s, 0));
        let mut s = session("sl", Activity::Busy);
        assert_eq!(detail(&s), "p · working");
        s.activity = Activity::Idle;
        s.branch = Some("main".into());
        assert_eq!(detail(&s), "p · main");
        s.branch = Some("feat/x".into());
        s.gh = Some(GhInfo {
            pr_number: Some(730),
            is_draft: Some(true),
            ci: Some(Ci {
                state: CiState::Passing,
                failed: vec![],
                pending: 0,
            }),
            ..GhInfo::default()
        });
        assert_eq!(detail(&s), "p · feat/x · draft · ✓");
        assert_eq!(
            plain(&row_title(&s)),
            "sl  #730",
            "the PR number rides on the title"
        );
    }

    #[test]
    fn failing_ci_names_its_first_job_in_red() {
        let mut s = session("sl", Activity::Busy);
        s.branch = Some("feat/x".into());
        let failed = |n: &str| FailedJob {
            name: n.into(),
            link: None,
        };
        s.gh = Some(GhInfo {
            pr_number: Some(1),
            ci: Some(Ci {
                state: CiState::Failing,
                failed: vec![failed("a"), failed("b")],
                pending: 0,
            }),
            ..GhInfo::default()
        });
        let segs = row_detail(&s, 0);
        assert_eq!(plain(&segs), "p · feat/x · ✗ CI: a · working");
        assert_eq!(
            segs.iter()
                .find(|seg| seg.text.starts_with('✗'))
                .map(|seg| seg.tone),
            Some(Tone::Red),
            "failing CI is red"
        );
    }

    #[test]
    fn a_reviewers_risk_and_quality_ride_on_their_row_coloured_by_direction() {
        let info = GhInfo {
            pr_number: Some(736),
            reviews: vec![Review {
                author: "claude".into(),
                state: ReviewState::Commented,
            }],
            assessment: Some(Assessment {
                author: "claude".into(),
                risk: Some(Score { value: 4, max: 5 }),
                quality: Some(Score { value: 4, max: 5 }),
            }),
            ..GhInfo::default()
        };
        let row = &reviewer_entries(&info)[0].1;
        let text: String = row.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(text, "claude commented · risk 4/5 · quality 4/5");
        let tone_of = |needle: &str| row.iter().find(|g| g.text.contains(needle)).map(|g| g.tone);
        assert_eq!(tone_of("risk"), Some(Tone::Red));
        assert_eq!(tone_of("quality"), Some(Tone::Green));
    }

    /// Size is said by what a segment is, not guessed from its tone or text.
    #[test]
    fn a_rows_pr_number_is_a_step_smaller_and_score_dots_are_dots() {
        let mut s = session("sl", Activity::Busy);
        s.gh = Some(GhInfo {
            pr_number: Some(781),
            ..GhInfo::default()
        });
        let sizes: Vec<SegSize> = row_title(&s).iter().map(|seg| seg.size).collect();
        assert_eq!(
            sizes,
            [SegSize::Line, SegSize::Smaller],
            "the name, then its PR number"
        );
        let info = GhInfo {
            assessment: Some(Assessment {
                author: "claude".into(),
                risk: Some(Score { value: 2, max: 5 }),
                quality: None,
            }),
            reviews: vec![Review {
                author: "claude".into(),
                state: ReviewState::Commented,
            }],
            ..GhInfo::default()
        };
        let (_, segs) = reviewer_lines(&info).into_iter().next().unwrap();
        let dots: Vec<&str> = segs
            .iter()
            .filter(|seg| seg.size == SegSize::Dots)
            .map(|seg| seg.text.as_str())
            .collect();
        assert_eq!(dots, ["●●", "○○○"]);
        assert!(
            segs.iter()
                .filter(|seg| seg.size != SegSize::Dots)
                .all(|seg| seg.size == SegSize::Line),
            "the rest at the line's size"
        );
    }

    #[test]
    fn pace_reads_in_rounded_points_and_colours_the_window() {
        assert_eq!(pace_text(-15.2), "15% behind pace");
        assert_eq!(pace_text(27.6), "28% ahead of pace");
        assert_eq!(pace_text(0.4), "On pace");
        assert_eq!(pace_text(-0.4), "On pace");
        assert_eq!(window_tone(18.0, Some(-15.0)), Tone::Green);
        assert_eq!(
            window_tone(42.0, Some(28.0)),
            Tone::Orange,
            "ahead of pace is the warning"
        );
        assert_eq!(
            window_tone(90.0, Some(-5.0)),
            Tone::Red,
            "nearly out is past the warning"
        );
        assert_eq!(
            window_tone(42.0, Some(0.4)),
            Tone::Green,
            "on pace is not ahead"
        );
        assert_eq!(
            window_tone(90.0, Some(-5.0)),
            Tone::Red,
            "nearly spent is red whatever the pace"
        );
        assert_eq!(window_tone(42.0, None), Tone::Green);
    }

    #[test]
    fn the_bar_badge_prefers_caffeinated_then_working_then_idle() {
        // The `/rc` switch on, as it starts.
        let (off, auto) = (
            Awake {
                auto: false,
                rc: true,
            },
            Awake {
                auto: true,
                rc: true,
            },
        );
        assert_eq!(bar_state(&[], off, true), BarState::NoSessions);
        let idle = session("idle", Activity::Idle);
        let busy = session("busy", Activity::Busy);
        assert_eq!(
            bar_state(std::slice::from_ref(&idle), off, true),
            BarState::Idle
        );
        assert_eq!(
            bar_state(&[idle.clone(), busy.clone()], off, true),
            BarState::Working
        );

        // Auto caffeinate counts only while working; `/rc` counts busy or idle.
        assert_eq!(
            bar_state(std::slice::from_ref(&idle), auto, true),
            BarState::Idle
        );
        assert_eq!(
            bar_state(&[idle.clone(), busy.clone()], auto, true),
            BarState::Caffeinated
        );
        let rc = OpenSession {
            remote_control: Some("session_x".into()),
            ..idle.clone()
        };
        assert_eq!(
            bar_state(std::slice::from_ref(&rc), off, true),
            BarState::Caffeinated,
            "/rc ignores the switch"
        );
        assert!(off.holds(&rc));
        assert!(
            !Awake {
                auto: false,
                rc: false
            }
            .holds(&rc),
            "unless its own switch is off"
        );
        assert!(
            Awake {
                auto: true,
                rc: false
            }
            .holds(&OpenSession {
                activity: Activity::Busy,
                ..rc.clone()
            }),
            "working still counts under Auto caffeinate"
        );

        // With caffeinate disabled nothing is held awake.
        assert_eq!(bar_state(&[rc], off, false), BarState::Idle);
        assert_eq!(bar_state(&[busy], auto, false), BarState::Working);
    }

    #[test]
    fn review_verdict_shows_on_the_row_and_changes_requested_wins() {
        use sl_core::gh::Review;
        let review = |a: &str, st: &str| Review {
            author: a.into(),
            state: ReviewState::from_api(st).unwrap(),
        };
        let detail = |s: &OpenSession| plain(&row_detail(s, 0));
        let mut s = session("sl", Activity::Busy);
        s.branch = Some("feat/x".into());
        s.gh = Some(GhInfo {
            pr_number: Some(734),
            review_decision: Some("CHANGES_REQUESTED".into()),
            reviews: vec![
                review("ann", "COMMENTED"),
                review("bob", "CHANGES_REQUESTED"),
            ],
            ..GhInfo::default()
        });
        assert_eq!(detail(&s), "p · feat/x · changes requested · working");
        assert_eq!(
            verdict_text(s.gh.as_ref().unwrap()),
            Some(("Changes requested", Tone::Red))
        );
        // No review rule on the repo: fall back to the reviews themselves.
        let set = |s: &mut OpenSession, decision: Option<&str>, reviews: Vec<Review>| {
            let info = s.gh.as_mut().unwrap();
            info.review_decision = decision.map(String::from);
            info.reviews = reviews;
        };
        set(
            &mut s,
            None,
            vec![
                review("ann", "APPROVED"),
                review("bob", "CHANGES_REQUESTED"),
            ],
        );
        assert_eq!(detail(&s), "p · feat/x · changes requested · working");
        set(&mut s, None, vec![review("ann", "APPROVED")]);
        assert_eq!(detail(&s), "p · feat/x · approved · working");
        set(&mut s, None, vec![review("ann", "COMMENTED")]);
        assert_eq!(detail(&s), "p · feat/x · working");
    }

    #[test]
    fn project_names_come_from_the_common_dir_and_skip_branch_named_checkouts() {
        assert_eq!(
            project_name("/Users/j/Developer/datanaut/main/.git"),
            "datanaut"
        );
        assert_eq!(
            project_name("/Users/j/Developer/statusline/.git"),
            "statusline"
        );
        assert_eq!(project_name("/srv/git/repo.git"), "repo");
        assert_eq!(project_name("/x/y/master/.git/"), "y");
    }

    #[test]
    fn sessions_group_by_project_then_busy_then_newest() {
        let mk = |name: &str, project: &str, activity, started_ms| OpenSession {
            project: project.into(),
            started_ms,
            ..session(name, activity)
        };
        let mut all = vec![
            mk("b-idle", "beta", Activity::Idle, 9),
            mk("a-old", "alpha", Activity::Idle, 1),
            mk("a-new", "alpha", Activity::Idle, 5),
            mk("a-busy", "alpha", Activity::Busy, 0),
        ];
        sort_sessions(&mut all);
        let names: Vec<_> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a-busy", "a-new", "a-old", "b-idle"]);
    }

    #[test]
    fn idle_rows_show_the_last_activity_and_the_cache_countdown() {
        let mut s = session("sl", Activity::Idle);
        s.branch = Some("main".into());
        let min = 60_000;
        let at = |last: i64, ttl| {
            Some(CacheInfo {
                last_activity_ms: last,
                ttl_ms: ttl,
                compacted: false,
            })
        };
        s.cache = at(0, Some(60 * min));
        let detail = |s: &OpenSession, now| plain(&row_detail(s, now));
        assert_eq!(detail(&s, 12 * min), "p · main · idle 12m");
        assert_eq!(
            row_right(&s, 12 * min),
            Some(("48m left".to_owned(), Tone::Dim))
        );
        assert_eq!(
            row_right(&s, 50 * min).map(|(_, tone)| tone),
            Some(Tone::Orange),
            "inside the needs-you window"
        );
        assert_eq!(
            row_right(&s, 56 * min).map(|(_, tone)| tone),
            Some(Tone::Red),
            "about to expire"
        );
        assert_eq!(detail(&s, 61 * min), "p · main · idle 1h 1m");
        assert_eq!(
            row_right(&s, 61 * min),
            Some(("expired".to_owned(), Tone::Dim))
        );
        s.cache = at(0, None);
        assert_eq!(row_right(&s, min), None, "no lifetime known: no countdown");
        s.activity = Activity::Busy;
        assert_eq!(
            detail(&s, min),
            "p · main · working",
            "a working session keeps its cache warm: nothing to count down"
        );
        assert_eq!(row_right(&s, min), Some(("warm".to_owned(), Tone::Dim)));
    }

    #[test]
    fn a_compacted_session_is_idle_not_cold_and_says_so() {
        let mut s = session("sl", Activity::Idle);
        s.cache = Some(CacheInfo {
            last_activity_ms: 0,
            ttl_ms: None,
            compacted: true,
        });
        let hours = 3 * 60 * 60_000;
        assert_eq!(urgency(&s, hours), Urgency::Idle);
        assert_eq!(session_icon(&s, hours, false), Icon::Idle);
        assert_eq!(
            row_right(&s, hours),
            Some(("compacted".to_owned(), Tone::Dim))
        );
        assert_eq!(
            plain(&row_detail(&s, hours)),
            "p · idle 3h",
            "idle since the compaction"
        );
    }

    #[test]
    fn the_icon_follows_activity_and_the_cache_clock() {
        let min = 60_000;
        let mut s = session("sl", Activity::Idle);
        assert_eq!(
            session_icon(&s, 0, false),
            Icon::Idle,
            "no cache information: just idle"
        );
        s.cache = Some(CacheInfo {
            last_activity_ms: 0,
            ttl_ms: Some(60 * min),
            compacted: false,
        });
        assert_eq!(session_icon(&s, 10 * min, false), Icon::Idle);
        assert_eq!(session_icon(&s, 56 * min, false), Icon::Expiring);
        assert_eq!(session_icon(&s, 61 * min, false), Icon::Cold);
        s.activity = Activity::Busy;
        assert_eq!(
            session_icon(&s, 61 * min, false),
            Icon::Busy,
            "a working session is never cold"
        );
        assert_eq!(
            session_icon(&s, 0, true),
            Icon::Caffeinated,
            "busy with Auto caffeinate on"
        );
        s.activity = Activity::Idle;
        assert_eq!(
            session_icon(&s, 0, true),
            Icon::Idle,
            "idle: not caffeinating even with Auto caffeinate on"
        );
    }

    #[test]
    fn a_branch_not_asked_about_yet_reads_as_checking_and_an_answered_one_as_none_yet() {
        let mut s = session("sl", Activity::Idle);
        s.branch = Some("feat/x".into());
        s.gh = None;
        assert_eq!(no_pr(&s), Some(NoPr::Checking));
        s.gh = Some(GhInfo::default());
        assert_eq!(no_pr(&s), Some(NoPr::NoneYet));
    }

    #[test]
    fn long_branches_are_cut() {
        let mut s = session("sl", Activity::Busy);
        s.branch = Some("x".repeat(60));
        let segs = row_detail(&s, 0);
        let branch = &segs[2].text;
        assert!(branch.ends_with('…'), "{branch}");
        assert_eq!(branch.chars().count(), BRANCH_WIDTH);
    }

    #[test]
    fn submenu_lines_describe_the_pr_reviewers_and_ci() {
        use sl_core::gh::Review;
        let review = |a: &str, st: &str| Review {
            author: a.into(),
            state: ReviewState::from_api(st).unwrap(),
        };
        let mut s = session("sl", Activity::Busy);
        s.branch = Some("feat/x".into());
        // GitHub answered: no PR.
        s.gh = Some(GhInfo::default());
        assert_eq!(
            no_pr(&s).map(NoPr::text),
            Some("No open PR for this branch")
        );
        assert_eq!(
            no_pr(&s).map(NoPr::title),
            Some("No PR yet"),
            "the submenu's title says it shorter"
        );
        s.branch = Some("main".into());
        assert_eq!(no_pr(&s).map(NoPr::text), Some("Default branch · no PR"));
        s.branch = Some("@bd014d0f1e".into());
        assert_eq!(no_pr(&s).map(NoPr::text), Some("Detached HEAD · no PR"));
        assert_eq!(no_pr(&s).map(NoPr::title), Some("Detached HEAD · no PR"));
        s.branch = None;
        assert_eq!(no_pr(&s).map(NoPr::text), Some("Not a git repository"));

        let failed = |n: &str, l: Option<&str>| FailedJob {
            name: n.into(),
            link: l.map(String::from),
        };
        s.branch = Some("feat/x".into());
        let info = GhInfo {
            pr_number: Some(734),
            is_draft: Some(false),
            base: Some("develop".into()),
            base_sha: Some("1adc44e1c5".into()),
            base_pr: Some(729),
            review_decision: Some("CHANGES_REQUESTED".into()),
            reviews: vec![
                review("ann", "APPROVED"),
                review("bob", "CHANGES_REQUESTED"),
                review("claude", "COMMENTED"),
            ],
            requested: vec!["bob".into(), "cy".into()],
            ci: Some(gh::Ci {
                state: CiState::Failing,
                failed: vec![
                    failed("lint", Some("https://github.com/o/r/actions/runs/9/job/42")),
                    failed("ext", None),
                ],
                pending: 0,
            }),
            ..GhInfo::default()
        };
        s.gh = Some(info.clone());
        assert_eq!(no_pr(&s), None, "it has one");
        let header = |s: &OpenSession| {
            crate::tray_model::pr_pills(s.gh.as_ref().unwrap(), crate::tray_model::PillsFor::Header)
                .0
                .into_iter()
                .map(|p| p.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            header(&s),
            ["Changes requested"],
            "not a draft: nothing to say"
        );
        assert_eq!(
            plain(&base_segments(&info).unwrap()),
            "→ develop\n1adc44e1c5 · stacked on #729"
        );
        let rows: Vec<String> = reviewer_entries(&info)
            .iter()
            .map(|(_, r)| plain(r))
            .collect();
        assert_eq!(
            rows,
            [
                "ann approved",
                "bob changes requested",
                "claude commented",
                "cy review requested"
            ]
        );
        assert_eq!(
            plain(&reviewer_entries(&GhInfo::default())[0].1),
            "No reviews yet"
        );
        let draft = GhInfo {
            pr_number: Some(1),
            is_draft: Some(true),
            ..GhInfo::default()
        };
        s.gh = Some(draft);
        assert_eq!(header(&s), ["Draft"]);
    }

    #[test]
    fn remote_control_follows_the_bridge_id_in_the_process_file() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(
            dir.join("1.json"),
            r#"{"pid":1,"bridgeSessionId":"session_abc"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("2.json"), r#"{"pid":2}"#).unwrap();
        assert_eq!(remote_control_id(&dir, 1).as_deref(), Some("session_abc"));
        assert_eq!(remote_control_id(&dir, 2), None);
        assert_eq!(remote_control_id(&dir, 3), None);
        std::fs::write(
            dir.join("4.json"),
            r#"{"pid":4,"name":"statusline-f8","nameSource":"derived"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("5.json"),
            r#"{"pid":5,"name":"My session","nameSource":"user"}"#,
        )
        .unwrap();
        assert!(
            !named_by_user(&dir, 4),
            "a derived name gives way to the title"
        );
        assert!(named_by_user(&dir, 5));
        assert!(!named_by_user(&dir, 2));
    }

    /// A record as Claude Code writes it, trimmed to what is read.
    fn record(dir: &std::path::Path, pid: u32, id: &str, started_ms: i64, extra: &str) {
        let json = format!(
            r#"{{"pid":{pid},"sessionId":"{id}","cwd":"/a/{id}","startedAt":{started_ms},"kind":"interactive"{extra}}}"#
        );
        std::fs::write(dir.join(format!("{pid}.json")), json).unwrap();
    }

    #[test]
    fn the_session_records_list_what_is_running_and_nothing_else() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path();
        // Started at 1000 s, recorded half a second later: running.
        record(
            dir,
            10,
            "live",
            1_000_500,
            r#","status":"busy","name":"Mine","nameSource":"user","bridgeSessionId":"session_rc""#,
        );
        record(
            dir,
            11,
            "derived",
            1_000_000,
            r#","status":"idle","name":"proj","nameSource":"derived""#,
        );
        // The process is gone; its record stayed behind.
        record(dir, 12, "dead", 1_000_000, "");
        // The pid now belongs to a process that started after the session.
        record(dir, 13, "reused", 1_000_000, "");
        record(
            dir,
            14,
            "waiting",
            2_000_000,
            r#","status":"waiting","waitingFor":"input needed""#,
        );
        std::fs::write(dir.join("10.0123abcd.key"), "secret").unwrap();
        std::fs::write(dir.join("notes.json"), "{}").unwrap();
        let start_of = |pid| match pid {
            10 | 14 => Some(1000),
            11 => Some(999),
            13 => Some(1001),
            _ => None,
        };
        let (sessions, named) = sessions_from_files(dir, start_of).unwrap();
        let ids: Vec<_> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["live", "waiting", "derived"],
            "busy first, then the most recently started, as parse_agents sorts"
        );
        assert_eq!(sessions[0].activity, Activity::Busy);
        assert_eq!(sessions[0].name, "Mine");
        assert_eq!(sessions[0].remote_control.as_deref(), Some("session_rc"));
        assert_eq!(sessions[0].pid, Some(10));
        assert_eq!(
            sessions[1].activity,
            Activity::Other,
            "`waiting` is shown as idle, as from claude agents"
        );
        assert_eq!(sessions[2].remote_control, None);
        assert_eq!(named, [10], "a derived name gives way to the title");
        // The same record, through claude agents, reads the same.
        let agents = parse_agents(&format!(
            "[{}]",
            std::fs::read_to_string(dir.join("14.json")).unwrap()
        ))
        .unwrap();
        assert_eq!(agents[0], sessions[1]);
    }

    #[test]
    fn a_record_that_cannot_be_read_hands_the_pass_to_claude_agents() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path();
        record(dir, 10, "live", 1_000_000, "");
        let alive = |_| Some(1000);
        assert_eq!(
            sessions_from_files(&dir.join("missing"), alive).map(|(s, _)| s.len()),
            Some(0),
            "no directory: no sessions, not unreadable records"
        );
        // Half-written, for a process that is running.
        std::fs::write(dir.join("11.json"), r#"{"pid":11,"sessionId":"#).unwrap();
        assert!(sessions_from_files(dir, alive).is_none());
        // ...but a dead process's broken record is just skipped.
        assert_eq!(
            sessions_from_files(dir, |pid| (pid == 10).then_some(1000)).map(|(s, _)| s.len()),
            Some(1)
        );
        // A format without the session id, or for another pid.
        std::fs::write(dir.join("11.json"), r#"{"pid":11,"cwd":"/a"}"#).unwrap();
        assert!(sessions_from_files(dir, alive).is_none());
        std::fs::write(
            dir.join("11.json"),
            r#"{"pid":12,"sessionId":"x","cwd":"/a"}"#,
        )
        .unwrap();
        assert!(sessions_from_files(dir, alive).is_none());
        std::fs::remove_file(dir.join("11.json")).unwrap();
        assert_eq!(
            sessions_from_files(dir, alive).map(|(s, _)| s.len()),
            Some(1)
        );
    }

    #[test]
    fn the_records_are_used_only_while_claude_agents_agrees() {
        let listed = |ids: &[&str]| -> Option<Listed> {
            Some((
                ids.iter().map(|id| session(id, Activity::Idle)).collect(),
                Vec::new(),
            ))
        };
        let names = |l: Option<Listed>| {
            l.map(|(s, _)| s.into_iter().map(|s| s.session_id).collect::<Vec<_>>())
        };
        let start = Instant::now();
        let mut lister = ClaudeLister::default();
        let asked = std::cell::Cell::new(0);
        let agents = |ids: &'static [&'static str]| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                listed(ids)
            }
        };
        // The first pass asks, and they agree: the records are used.
        assert_eq!(
            names(lister.list(start, || listed(&["a"]), agents(&["a"]))),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(asked.get(), 1);
        // Until the next check, claude agents is not run.
        let soon = start + Duration::from_secs(60);
        assert_eq!(
            names(lister.list(soon, || listed(&["a", "b"]), agents(&["a"]))),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(asked.get(), 1);
        // The check finds them apart: claude agents wins, and is asked again
        // (no more often than every AGENTS_EVERY) until they agree again.
        let later = start + CROSS_CHECK_EVERY;
        assert_eq!(
            names(lister.list(later, || listed(&["a", "stale"]), agents(&["a"]))),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(
            names(lister.list(
                later + Duration::from_secs(3),
                || listed(&["a", "stale"]),
                agents(&["a"])
            )),
            Some(vec!["a".to_owned()]),
            "a pass soon after reuses its answer"
        );
        assert_eq!(asked.get(), 2);
        assert_eq!(
            names(lister.list(
                later + Duration::from_secs(15),
                || listed(&["a", "stale"]),
                agents(&["a"])
            )),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(asked.get(), 3);
        assert_eq!(
            names(lister.list(
                later + Duration::from_secs(30),
                || listed(&["a"]),
                agents(&["a"])
            )),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(
            lister
                .list(
                    later + Duration::from_secs(45),
                    || listed(&["a"]),
                    agents(&["a"])
                )
                .map(|l| l.0.len()),
            Some(1)
        );
        assert_eq!(asked.get(), 4, "agreed again: back to the records alone");
        // Unreadable records: claude agents, whatever was agreed.
        assert_eq!(
            names(lister.list(later + Duration::from_secs(60), || None, agents(&["z"]))),
            Some(vec!["z".to_owned()])
        );
        // claude cannot be run: the records are all there is; neither, nothing.
        let mut fresh = ClaudeLister::default();
        assert_eq!(
            names(fresh.list(start, || listed(&["a"]), || None)),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(names(fresh.list(start, || None, || None)), None);
    }

    #[test]
    fn the_needs_you_threshold_moves_the_group_the_pill_and_the_colour_together() {
        let min = 60_000;
        let idle = with_cache(session("s", Activity::Idle), 0, 60 * min);
        let at = |mins: i64| Tuning {
            needs_you_ms: mins * min,
            ..Tuning::default()
        };
        // 25 minutes left.
        assert_eq!(super::urgency(&idle, 35 * min, &at(15)), Urgency::Idle);
        assert_eq!(super::urgency(&idle, 35 * min, &at(30)), Urgency::NeedsYou);
        assert_eq!(super::cache_tone(25 * min, 30 * min), Tone::Orange);
        assert_eq!(super::cache_tone(25 * min, 15 * min), Tone::Plain);
        assert_eq!(
            super::cache_tone(4 * min, 30 * min),
            Tone::Red,
            "the last five minutes are red whatever it is"
        );
        assert_eq!(
            super::row_right(&idle, 35 * min, &at(30)),
            Some(("25m left".to_owned(), Tone::Orange))
        );
        assert_eq!(
            super::row_right(&idle, 35 * min, &at(15)),
            Some(("25m left".to_owned(), Tone::Dim))
        );
    }

    #[test]
    fn the_ring_turns_at_the_chosen_points() {
        let mut s = session("s", Activity::Idle);
        s.context = Some(ContextCapture {
            percent: 75.0,
            used_tokens: None,
            limit_tokens: None,
            captured_at_ms: 0,
        });
        let at = |note, warn| Tuning {
            ring_note: note,
            ring_warn: warn,
            ..Tuning::default()
        };
        assert_eq!(
            super::context_ring(&s, &at(70, 85)).map(|r| r.1),
            Some(Tone::Orange)
        );
        assert_eq!(
            super::context_ring(&s, &at(80, 90)).map(|r| r.1),
            Some(Tone::Plain)
        );
        assert_eq!(
            super::context_ring(&s, &at(60, 75)).map(|r| r.1),
            Some(Tone::Red),
            "at the warning point counts"
        );
    }

    #[test]
    fn the_sort_order_applies_within_each_group_only() {
        let mut old = session("old", Activity::Idle);
        old.started_ms = 100;
        old.last_activity_ms = Some(900);
        let mut new = session("new", Activity::Idle);
        new.started_ms = 500;
        new.last_activity_ms = Some(200);
        let mut busy = session("busy", Activity::Busy);
        busy.started_ms = 1;
        let all = [old, new, busy];
        let names = |sort| {
            super::arrange(
                &all,
                None,
                0,
                &Tuning {
                    sort,
                    ..Tuning::default()
                },
            )
            .into_iter()
            .map(|(s, _)| s.name)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            names(SortOrder::Project),
            ["busy", "old", "new"],
            "as it arrives; Working leads Idle"
        );
        assert_eq!(
            names(SortOrder::Recent),
            ["busy", "old", "new"],
            "old was touched last"
        );
        assert_eq!(
            names(SortOrder::Started),
            ["busy", "new", "old"],
            "new started last"
        );
    }

    #[test]
    fn a_ring_ends_a_long_nap_but_no_sooner_than_the_gap() {
        static BELL: Bell = Bell::new();
        assert_eq!(BELL.nap_secs(1, 3600), 3600, "quiet: the backstop");
        let start = Instant::now();
        let sleeper = std::thread::spawn(|| nap(|| BELL.nap_secs(1, 3600)));
        std::thread::sleep(Duration::from_millis(50));
        BELL.ring();
        BELL.ring();
        sleeper.join().unwrap();
        let slept = start.elapsed();
        assert!(
            slept >= Duration::from_secs(1) && slept < Duration::from_secs(10),
            "woke after {slept:?}"
        );
        assert_eq!(BELL.nap_secs(1, 3600), 1, "rung until answered");
        BELL.answer();
        assert_eq!(BELL.nap_secs(1, 3600), 3600);
    }

    fn ticket(id: &str) -> crate::linear::LinearInfo {
        crate::linear::LinearInfo {
            id: id.into(),
            url: format!("https://linear.app/x/issue/{id}"),
            state: "In Progress".into(),
        }
    }

    #[test]
    fn linear_is_asked_about_each_ticket_once_and_not_at_all_with_none_due() {
        let mut got = Vec::new();
        ask_linear(
            &[
                ("DN-1".to_owned(), "/a".to_owned()),
                ("DN-2".to_owned(), "/b".to_owned()),
            ],
            |id, _| Some(ticket(id)),
            |arrival| got.push(arrival),
        );
        let [Arrival::Linear(found)] = got.as_slice() else {
            panic!("one Linear arrival: {got:?}");
        };
        let mut ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["DN-1", "DN-2"]);
        ask_linear(
            &[],
            |_, _| panic!("Linear asked with nothing due"),
            |_| panic!("an arrival with nothing asked"),
        );
    }

    #[test]
    fn a_branch_the_status_line_showed_never_replaces_a_sessions_own() {
        let pair = |dir: &str, branch: &str| (dir.to_owned(), branch.to_owned());
        assert_eq!(
            with_seen(
                vec![pair("/a", "feat")],
                vec![pair("/a", "old"), pair("/wt", "other")]
            ),
            vec![pair("/a", "feat"), pair("/wt", "other")]
        );
    }

    #[test]
    fn a_github_round_runs_one_at_a_time() {
        let round = GithubRound::default();
        round.busy.store(true, Ordering::SeqCst);
        // Under way: a second start does nothing (no thread, still busy).
        round.start(
            RoundAsks {
                dirs: Vec::new(),
                sessions: Vec::new(),
                gone: Vec::new(),
                now_ms: 0,
                policy: gh::Policy::BASE,
                pr_age_ms: 0,
            },
            None,
        );
        assert!(round.is_busy());
        assert_eq!(round.stacks(), None, "no round ran");
    }

    #[test]
    fn answers_fold_into_the_sessions_and_a_failed_lookup_keeps_what_was_known() {
        let mut a = session("a", Activity::Idle);
        a.cwd = "/a".into();
        a.branch = Some("jakub/dn-1-x".into());
        let mut b = session("b", Activity::Idle);
        b.cwd = "/b".into();
        b.branch = Some("main".into());
        let mut sessions = vec![a, b];
        let mut cache = crate::linear::LinearCache::default();
        let gh = HashMap::from([
            (
                "/a".to_owned(),
                Some(GhInfo {
                    pr_number: Some(7),
                    ..GhInfo::default()
                }),
            ),
            ("/b".to_owned(), None),
        ]);
        apply_arrival(&mut sessions, Arrival::Gh(gh), Some(&mut cache), 0);
        assert_eq!(sessions[0].gh.as_ref().and_then(|g| g.pr_number), Some(7));
        assert!(sessions[1].gh.is_none());
        apply_arrival(
            &mut sessions,
            Arrival::Linear(vec![("DN-1".into(), Some(ticket("DN-1")))]),
            Some(&mut cache),
            0,
        );
        assert_eq!(
            sessions[0].linear.as_ref().map(|t| t.id.as_str()),
            Some("DN-1")
        );
        // Linear down: the ticket stays as it was.
        apply_arrival(
            &mut sessions,
            Arrival::Linear(vec![("DN-1".into(), None)]),
            Some(&mut cache),
            60_000,
        );
        assert_eq!(
            sessions[0].linear.as_ref().map(|t| t.id.as_str()),
            Some("DN-1")
        );
        assert!(sessions[1].linear.is_none());
    }

    #[test]
    fn a_pass_asks_once_per_directory_and_only_about_tickets_that_are_due() {
        let at = |cwd: &str, branch: &str| {
            let mut s = session(cwd, Activity::Idle);
            s.cwd = cwd.into();
            s.branch = Some(branch.into());
            s
        };
        let sessions = [
            at("/a", "jakub/dn-1-x"),
            at("/a", "jakub/dn-1-x"),
            at("/b", "jakub/dn-2-y"),
            at("/c", "main"),
        ];
        let mut cache = crate::linear::LinearCache::default();
        let (dirs, tickets) = network_asks(&sessions, Some(&cache), 0, 60);
        assert_eq!(
            dirs.iter().map(|(cwd, _)| cwd.as_str()).collect::<Vec<_>>(),
            ["/a", "/b", "/c"]
        );
        assert_eq!(
            tickets
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["DN-1", "DN-2"]
        );
        cache.store("DN-1".into(), 0, Some(ticket("DN-1")));
        cache.store("DN-2".into(), 0, None);
        let due = |now| {
            network_asks(&sessions, Some(&cache), now, 60)
                .1
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        assert!(due(59_000).is_empty(), "both answered a moment ago");
        assert_eq!(
            due(60_000),
            ["DN-1"],
            "a found ticket after the PR interval; a miss five times later"
        );
        assert_eq!(due(300_000), ["DN-1", "DN-2"]);
        assert!(
            network_asks(&sessions, None, 0, 60).1.is_empty(),
            "Linear switched off: never asked"
        );
    }

    #[test]
    fn codex_sessions_do_not_depend_on_claude_agents_succeeding() {
        let claude = session("c", Activity::Idle);
        let mut codex = session("x", Activity::Idle);
        codex.provider = Source::Codex;
        let previous = [claude.clone(), codex];
        // It worked: its list, whatever came before.
        assert_eq!(
            claude_half(Some(vec![]), &previous, true).map(|l| l.len()),
            Some(0)
        );
        // It failed, Codex is listed: the Claude sessions of the last pass
        // stand (Codex's are read afresh), so nothing flashes away.
        let kept = claude_half(None, &previous, true).unwrap();
        assert_eq!(
            kept.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["c"]
        );
        // It failed, nothing else to list: the caller keeps its own list.
        assert!(claude_half(None, &previous, false).is_none());
    }

    fn with_cache(mut s: OpenSession, last: i64, ttl: i64) -> OpenSession {
        s.cache = Some(CacheInfo {
            last_activity_ms: last,
            ttl_ms: Some(ttl),
            compacted: false,
        });
        s
    }

    fn with_ci(mut s: OpenSession, state: CiState) -> OpenSession {
        s.gh = Some(GhInfo {
            pr_number: Some(7),
            ci: Some(Ci {
                state,
                failed: Vec::new(),
                pending: 0,
            }),
            ..GhInfo::default()
        });
        s
    }

    #[test]
    fn urgency_puts_work_first_then_what_is_about_to_cost_more() {
        let min = 60_000;
        let idle = || with_cache(session("s", Activity::Idle), 0, 60 * min);
        assert_eq!(urgency(&session("s", Activity::Busy), 0), Urgency::Working);
        assert_eq!(
            urgency(&with_cache(session("s", Activity::Busy), 0, min), 10 * min),
            Urgency::Working,
            "working keeps its cache warm"
        );
        assert_eq!(urgency(&idle(), 10 * min), Urgency::Idle);
        assert_eq!(
            urgency(&idle(), 45 * min),
            Urgency::NeedsYou,
            "15 minutes left"
        );
        assert_eq!(urgency(&idle(), 44 * min), Urgency::Idle);
        assert_eq!(urgency(&idle(), 60 * min), Urgency::Cold);
        assert_eq!(
            urgency(&session("s", Activity::Idle), 0),
            Urgency::Idle,
            "no cache information"
        );
        assert_eq!(
            urgency(&with_ci(idle(), CiState::Failing), 70 * min),
            Urgency::NeedsYou,
            "failed CI outranks a cold cache"
        );
        assert_eq!(
            urgency(&with_ci(idle(), CiState::Passing), 70 * min),
            Urgency::Cold
        );
        let mut changes = with_ci(idle(), CiState::Passing);
        changes.gh.as_mut().unwrap().review_decision = Some("CHANGES_REQUESTED".into());
        assert_eq!(urgency(&changes, 0), Urgency::NeedsYou);
    }

    #[test]
    fn arranging_groups_by_urgency_and_holds_still_while_the_menu_is_open() {
        let min = 60_000;
        let cold = with_cache(session("cold", Activity::Idle), 0, min);
        let busy = session("busy", Activity::Busy);
        let idle = session("idle", Activity::Idle);
        let names = |list: &[(OpenSession, Urgency)]| {
            list.iter()
                .map(|(s, g)| format!("{}:{g:?}", s.name))
                .collect::<Vec<_>>()
        };
        let fresh = arrange(&[cold.clone(), idle.clone(), busy.clone()], None, 10 * min);
        assert_eq!(names(&fresh), ["busy:Working", "idle:Idle", "cold:Cold"]);

        // While open: "busy" goes idle and "idle" closes; a new one opens.
        let now_idle = session("busy", Activity::Idle);
        let new = session("new", Activity::Busy);
        let held = arrange(&[cold.clone(), now_idle, new], Some(&fresh), 10 * min);
        // A newcomer joins the end of its own group, so no caption repeats.
        assert_eq!(
            names(&held),
            ["busy:Working", "new:Working", "idle:Idle", "cold:Cold"],
            "slots keep their place and group"
        );
        assert_eq!(
            held[0].0.activity,
            Activity::Idle,
            "but show the latest state"
        );
    }

    #[test]
    fn every_row_rings_its_context_round_its_state() {
        let ctx = |percent: f64| {
            Some(ContextCapture {
                percent,
                used_tokens: None,
                limit_tokens: None,
                captured_at_ms: 0,
            })
        };
        let mut busy = session("b", Activity::Busy);
        assert_eq!(context_ring(&busy), None, "no reading yet");
        busy.context = ctx(80.0);
        assert_eq!(
            context_ring(&busy),
            Some((20, Tone::Orange)),
            "a working session has its ring too"
        );
        assert_eq!(state_glyph(&busy, 0, true), Icon::Caffeinated);
        let failing = with_ci(session("f", Activity::Idle), CiState::Failing);
        assert_eq!(state_glyph(&failing, 0, false), Icon::Failed);
    }

    #[test]
    fn long_base_branches_are_cut_in_the_middle() {
        assert_eq!(truncate_middle("short", 10), "short");
        assert_eq!(truncate_middle("abcdefghij", 7), "abc…hij");
        let base = "jakub/dn-530-m22-m23-watch-as-a-trigger-type-with-its-dependency-edges";
        let cut = truncate_middle(base, BASE_WIDTH);
        assert_eq!(cut.chars().count(), BASE_WIDTH);
        assert!(
            cut.starts_with("jakub/dn-530") && cut.ends_with("dependency-edges"),
            "{cut}"
        );
    }

    #[test]
    fn regular_rows_put_the_cache_in_its_own_column() {
        let min = 60_000;
        let mut s = with_cache(session("DN-479 / M4", Activity::Idle), 0, 60 * min);
        s.project = "datanaut".into();
        s.branch = Some("jakub/dn-479".into());
        assert_eq!(
            plain(&row_detail(&s, 52 * min)),
            "datanaut · jakub/dn-479 · idle 52m"
        );
        assert_eq!(
            row_right(&s, 52 * min),
            Some(("8m left".to_owned(), Tone::Orange))
        );
        assert_eq!(
            row_right(&s, 10 * min),
            Some(("50m left".to_owned(), Tone::Dim))
        );
        assert_eq!(
            row_right(&s, 61 * min),
            Some(("expired".to_owned(), Tone::Dim))
        );
        s.activity = Activity::Busy;
        assert_eq!(row_right(&s, 0), Some(("warm".to_owned(), Tone::Dim)));
        assert_eq!(
            plain(&row_detail(&s, 0)),
            "datanaut · jakub/dn-479 · working"
        );
        let failing = with_ci(session("x", Activity::Idle), CiState::Failing);
        assert_eq!(plain(&row_title(&failing)), "x  #7");
    }

    #[test]
    fn a_session_appearing_while_the_menu_is_open_joins_its_own_group() {
        let now = 1_000_000;
        let working = OpenSession {
            activity: Activity::Busy,
            ..session("w1", Activity::Busy)
        };
        let idle = session("i1", Activity::Idle);
        let frozen = arrange(&[working.clone(), idle.clone()], None, now);
        let newcomer = OpenSession {
            activity: Activity::Busy,
            ..session("w2", Activity::Busy)
        };
        let arranged = arrange(&[working, idle, newcomer], Some(&frozen), now);
        let ids: Vec<&str> = arranged
            .iter()
            .map(|(s, _)| s.session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["w1", "w2", "i1"],
            "beside the other working session, not after the idle one"
        );
        let groups: Vec<Urgency> = arranged.iter().map(|(_, g)| *g).collect();
        assert!(
            groups.windows(2).all(|pair| pair[0] <= pair[1]),
            "no group appears twice: {groups:?}"
        );
    }
}
