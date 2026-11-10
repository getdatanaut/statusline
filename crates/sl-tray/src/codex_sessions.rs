//! The Codex threads open right now, as [`OpenSession`]s beside Claude Code's.
//!
//! Codex has no `claude agents --json`: what is open has to be read off the
//! filesystem, read-only. Checked against live `codex` TUIs (0.163, with the
//! 0.164-0.167 app-server daemon):
//!
//! - A thread being written holds `thread-writer-locks/<thread>.lock`, an
//!   `flock` its writer takes and drops (and deletes the file) when it lets go.
//!   A lock file nobody holds is a stale one a crash left behind.
//! - The holder is whichever app-server has the thread loaded. For the TUI that
//!   is a *shared* managed daemon, not the TUI process, and it keeps a thread
//!   loaded for a minute after its TUI quits (`thread_unload_delay_secs`), and
//!   for a moment while the resume picker previews one. So a thread the daemon
//!   holds counts only when a live interactive `codex` is in its directory --
//!   per directory, as many threads as there are such processes, most recently
//!   active first.
//! - A TUI locks a fresh thread before its first message; until then it has no
//!   rollout, or one with no turn, and is not a session yet.
//! - Runs that Claude Code's Codex plugin starts (`originator: "Claude Code"`)
//!   and subagents (`source: {"subagent": ...}`) are not sessions of their own.
//!
//! Everything else -- the branch, the PR and its CI, the Linear ticket -- comes
//! from the session's directory, the same way as for Claude Code's.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sl_core::types::ContextCapture;

use crate::sessions::{Activity, OpenSession};
use crate::view_prefs::Source;

/// How much of a rollout's start holds its `session_meta` (the largest seen is
/// under 24 KB, most of it the model's instructions).
const HEAD_BYTES: u64 = 64 * 1024;
/// How much of its end is searched for the latest turn event.
const TAIL_BYTES: u64 = 256 * 1024;
const LSOF_TIMEOUT: Duration = Duration::from_secs(5);
/// How long what `lsof` and `ps` said is used without asking again, while
/// none of the lock files and processes it was about has changed.
const ASKED_MAX_AGE: Duration = Duration::from_secs(30);
/// How long what they said last stands in when asking again fails.
const ASKED_STANDS_FOR: Duration = Duration::from_mins(2);

/// What `lsof` or `ps` said, and the processes it named, each with when it
/// started.
#[derive(Debug)]
struct Asked<T> {
    at: Instant,
    answer: T,
    pids: Vec<(u32, Option<u64>)>,
}

impl<T: Clone> Asked<T> {
    fn new(at: Instant, answer: T, pids: impl IntoIterator<Item = u32>) -> Self {
        Self {
            at,
            answer,
            pids: pids
                .into_iter()
                .map(|pid| (pid, sl_macos::process::started_at(pid)))
                .collect(),
        }
    }

    /// The answer, unless it may have changed since: it is older than
    /// [`ASKED_MAX_AGE`], or a process it named has exited (or its pid is
    /// another's now). `started` is when a process started.
    fn holding(&self, now: Instant, started: impl Fn(u32) -> Option<u64>) -> Option<T> {
        (now.duration_since(self.at) < ASKED_MAX_AGE
            && self.pids.iter().all(|(pid, at)| started(*pid) == *at))
        .then(|| self.answer.clone())
    }

    /// The answer, to stand in for one that could not be had: unless it is
    /// older than [`ASKED_STANDS_FOR`].
    fn standing(&self, now: Instant) -> Option<T> {
        (now.duration_since(self.at) < ASKED_STANDS_FOR).then(|| self.answer.clone())
    }
}

/// What one rollout says.
#[derive(Debug, Clone, Default)]
struct RolloutFacts {
    meta: Option<Meta>,
    /// The latest `task_started`, `task_complete` or `turn_aborted`: whether a
    /// turn is running, and when the thread was last active.
    working: bool,
    last_activity_ms: Option<i64>,
    context: Option<ContextCapture>,
}

#[derive(Debug, Clone)]
struct Meta {
    cwd: String,
    started_ms: i64,
    /// A plugin run or a subagent: not a session of its own.
    hidden: bool,
}

/// A rollout's facts, kept against its size and mtime so an unchanged one is
/// not read again, and where its last whole line read ends: a rollout only
/// grows, so a grown one is read from there on alone.
#[derive(Debug)]
struct Held {
    size: u64,
    mtime: i64,
    read_to: u64,
    facts: RolloutFacts,
}

/// What is kept between passes.
#[derive(Debug)]
pub struct CodexSessions {
    facts: HashMap<PathBuf, Held>,
    /// The rollouts' directories, read again only when they change.
    walk: sl_usage::paths::Walk,
    /// Which process holds each lock file, for the lock files it was asked
    /// about.
    holders: Option<(Locks, Asked<Holders>)>,
    /// How many interactive `codex` processes run in each directory.
    room: Option<Asked<HashMap<String, usize>>>,
}

/// Each lock file's thread and path.
type Locks = Vec<(String, PathBuf)>;
/// Each held lock's thread, and the process holding it.
type Holders = Vec<(String, u32)>;

impl Default for CodexSessions {
    fn default() -> Self {
        Self {
            facts: HashMap::new(),
            walk: sl_usage::paths::Walk::new(sl_usage::codex::is_rollout)
                .ringing(|| crate::sessions::SESSIONS_BELL.ring()),
            holders: None,
            room: None,
        }
    }
}

impl CodexSessions {
    /// The open Codex sessions under `home`, unenriched: no branch or PR yet.
    pub fn load(&mut self, home: &Path) -> Vec<OpenSession> {
        let mut locks = lock_files(home);
        if locks.is_empty() {
            self.facts.clear();
            self.holders = None;
            return Vec::new();
        }
        locks.sort();
        let holders = self.holders_of(locks, Instant::now());
        if holders.is_empty() {
            return Vec::new();
        }
        let rollouts: HashMap<String, PathBuf> =
            sl_usage::codex::codex_files_with(home, &mut self.walk)
                .into_iter()
                .filter_map(|path| Some((sl_usage::codex::thread_of(&path)?, path)))
                .collect();
        let daemon = daemon_pid(home);
        let names = thread_names(home);

        let mut candidates: Vec<(OpenSession, bool)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for (thread, pid) in holders {
            let Some(path) = rollouts.get(&thread) else {
                continue;
            };
            seen.insert(path.clone());
            let facts = self.facts_of(path);
            let Some(meta) = facts.meta.as_ref().filter(|meta| !meta.hidden) else {
                continue;
            };
            let Some(last_activity_ms) = facts.last_activity_ms else {
                continue;
            };
            let by_daemon = daemon == Some(pid);
            let session = OpenSession {
                session_id: thread.clone(),
                name: names
                    .get(&thread)
                    .cloned()
                    .unwrap_or_else(|| dir_name(&meta.cwd)),
                project: String::new(),
                cwd: meta.cwd.clone(),
                activity: if facts.working {
                    Activity::Busy
                } else {
                    Activity::Idle
                },
                started_ms: meta.started_ms,
                branch: None,
                gh: None,
                cache: None,
                remote_control: None,
                pid: (!by_daemon).then_some(pid),
                context: facts.context,
                linear: None,
                provider: Source::Codex,
                last_activity_ms: Some(last_activity_ms),
            };
            candidates.push((session, by_daemon));
        }
        self.facts.retain(|path, _| seen.contains(path));

        // The daemon's threads, matched to the interactive processes by
        // directory: per directory, no more threads than processes, most
        // recently active first.
        let daemon_held: Vec<&OpenSession> = candidates
            .iter()
            .filter(|(_, by_daemon)| *by_daemon)
            .map(|(session, _)| session)
            .collect();
        let room = if daemon_held.is_empty() {
            HashMap::new()
        } else {
            self.room_of(daemon, Instant::now())
        };
        let mut by_cwd: HashMap<&str, Vec<&OpenSession>> = HashMap::new();
        for session in daemon_held {
            by_cwd
                .entry(session.cwd.as_str())
                .or_default()
                .push(session);
        }
        let mut kept: HashSet<String> = HashSet::new();
        for (cwd, mut sessions) in by_cwd {
            sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_ms));
            kept.extend(
                sessions
                    .into_iter()
                    .take(room.get(cwd).copied().unwrap_or(0))
                    .map(|session| session.session_id.clone()),
            );
        }
        candidates
            .into_iter()
            .filter(|(session, by_daemon)| !by_daemon || kept.contains(&session.session_id))
            .map(|(session, _)| session)
            .collect()
    }

    /// Which process holds each of `locks`: what `lsof` said last while it
    /// still holds, else asked again. When `lsof` fails, what it said last
    /// stands for a while, rather than every session closing at once.
    fn holders_of(&mut self, locks: Vec<(String, PathBuf)>, now: Instant) -> Vec<(String, u32)> {
        if let Some((asked_for, asked)) = &self.holders
            && *asked_for == locks
            && let Some(holders) = asked.holding(now, sl_macos::process::started_at)
        {
            return holders;
        }
        match lock_holders(&locks) {
            Some(holders) => {
                let pids: Vec<u32> = holders.iter().map(|(_, pid)| *pid).collect();
                self.holders = Some((locks, Asked::new(now, holders.clone(), pids)));
                holders
            }
            None => self
                .holders
                .as_ref()
                .and_then(|(_, asked)| asked.standing(now))
                .unwrap_or_default(),
        }
    }

    /// How many interactive `codex` processes run in each directory: what
    /// `ps` and `lsof` said last while it still holds and the lock holders
    /// have not been asked about since, else asked again; failing that, what
    /// they said last, for a while.
    fn room_of(&mut self, daemon: Option<u32>, now: Instant) -> HashMap<String, usize> {
        let holders_at = self.holders.as_ref().map(|(_, asked)| asked.at);
        if let Some(asked) = &self.room
            && holders_at.is_none_or(|at| asked.at >= at)
            && let Some(room) = asked.holding(now, sl_macos::process::started_at)
        {
            return room;
        }
        match interactive_cwds(daemon) {
            Some((room, pids)) => {
                self.room = Some(Asked::new(now, room.clone(), pids));
                room
            }
            None => self
                .room
                .as_ref()
                .and_then(|asked| asked.standing(now))
                .unwrap_or_default(),
        }
    }

    fn facts_of(&mut self, path: &Path) -> RolloutFacts {
        let Some((size, mtime)) = stat(path) else {
            return RolloutFacts::default();
        };
        let held = self.facts.get(path);
        if let Some(held) = held
            && held.size == size
            && held.mtime == mtime
        {
            return held.facts.clone();
        }
        let grown = held
            .filter(|held| held.facts.meta.is_some() && held.read_to <= size && !compressed(path));
        let (facts, read_to) = match grown {
            Some(held) => read_appended(path, held.read_to, size, &held.facts),
            None => read_facts(path, size),
        }
        .unwrap_or_default();
        self.facts.insert(
            path.to_path_buf(),
            Held {
                size,
                mtime,
                read_to,
                facts: facts.clone(),
            },
        );
        facts
    }
}

fn stat(path: &Path) -> Option<(u64, i64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = i64::try_from(
        metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    Some((metadata.len(), mtime))
}

fn dir_name(cwd: &str) -> String {
    cwd.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(cwd)
        .to_owned()
}

/// Every per-thread lock file, as `(thread, path)`.
fn lock_files(home: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(home.join("thread-writer-locks")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let thread = name
                .strip_suffix(".lock")
                .filter(|thread| !thread.starts_with('.'))?
                .to_ascii_lowercase();
            Some((thread, entry.path()))
        })
        .collect()
}

/// Which process holds each lock file open: one `lsof` over all of them.
/// A file no process has open is stale, and left out. `None` when `lsof`
/// failed.
fn lock_holders(locks: &[(String, PathBuf)]) -> Option<Vec<(String, u32)>> {
    let mut args: Vec<String> = vec![
        "-n".into(),
        "-P".into(),
        "-F".into(),
        "pn".into(),
        "--".into(),
    ];
    args.extend(locks.iter().map(|(_, path)| path.display().to_string()));
    Some(holders_in(locks, &lsof(&args)?))
}

/// Which of `locks` each process in `lsof`'s output holds. `lsof` names a
/// file by its resolved path (`/private/tmp`, not `/tmp`; a `CODEX_HOME`
/// reached through a symlink, resolved), so the locks are resolved to match.
fn holders_in(locks: &[(String, PathBuf)], output: &str) -> Vec<(String, u32)> {
    let by_path: HashMap<PathBuf, &str> = locks
        .iter()
        .map(|(thread, path)| {
            let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            (path, thread.as_str())
        })
        .collect();
    let mut held = Vec::new();
    for (pid, name) in lsof_fields(output) {
        if let Some(thread) = by_path.get(Path::new(&name)) {
            held.push(((*thread).to_owned(), pid));
        }
    }
    held.sort();
    held.dedup_by(|a, b| a.0 == b.0);
    held
}

/// `(pid, name)` pairs from `lsof -F pn` output: a `p` line starts a process,
/// each `n` line after it is one of its files.
fn lsof_fields(output: &str) -> Vec<(u32, String)> {
    let mut pairs = Vec::new();
    let mut pid = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let (Some(name), Some(pid)) = (line.strip_prefix('n'), pid) {
            pairs.push((pid, name.to_owned()));
        }
    }
    pairs
}

/// What `lsof args` printed; `None` when it could not run, ran out of time,
/// or failed without listing anything.
fn lsof(args: &[String]) -> Option<String> {
    // `-w`: no warnings on stderr, so whatever it says there is an error.
    let mut command = std::process::Command::new("/usr/sbin/lsof");
    command.arg("-w").args(args).current_dir("/");
    let ran = sl_core::process::run(command, LSOF_TIMEOUT).ok()?;
    // `lsof` exits 1 when some of the files (or all) are open by nobody, and
    // with an error for one gone since it was listed; its output still lists
    // the ones that are open.
    (ran.success || ran.stderr.trim().is_empty() || ran.stdout.starts_with('p'))
        .then_some(ran.stdout)
}

/// The managed app-server daemon's pid, from its own pid file.
fn daemon_pid(home: &Path) -> Option<u32> {
    #[derive(Deserialize)]
    struct PidFile {
        pid: u32,
    }
    let text = std::fs::read_to_string(home.join("app-server-daemon/daemon.pid")).ok()?;
    serde_json::from_str::<PidFile>(&text)
        .ok()
        .map(|file| file.pid)
}

/// The directories interactive `codex` processes run in, and how many in
/// each: the TUIs (and anything else running `codex` that is not the daemon
/// or one of its helpers). Also those processes. `None` when `ps` or `lsof`
/// failed.
fn interactive_cwds(daemon: Option<u32>) -> Option<(HashMap<String, usize>, Vec<u32>)> {
    let ps = crate::actions::run_for("/bin/ps", &["-axo", "pid=,comm=,args="], "/", LSOF_TIMEOUT)
        .ok()?;
    let pids: Vec<u32> = ps
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let comm = parts.next()?;
            let args: Vec<&str> = parts.collect();
            let is_codex = comm == "codex" || comm.ends_with("/codex");
            let helper = args.iter().any(|arg| {
                matches!(
                    *arg,
                    "app-server"
                        | "exec-server"
                        | "mcp-server"
                        | "--managed-daemon"
                        | "pid-update-loop"
                )
            });
            (is_codex && !helper && Some(pid) != daemon).then_some(pid)
        })
        .collect();
    if pids.is_empty() {
        return Some((HashMap::new(), pids));
    }
    let listed: Vec<String> = pids.iter().map(u32::to_string).collect();
    let args = [
        "-n".to_owned(),
        "-P".to_owned(),
        "-a".to_owned(),
        "-d".to_owned(),
        "cwd".to_owned(),
        "-F".to_owned(),
        "pn".to_owned(),
        "-p".to_owned(),
        listed.join(","),
    ];
    let mut counts = HashMap::new();
    for (_, cwd) in lsof_fields(&lsof(&args)?) {
        *counts.entry(cwd).or_insert(0) += 1;
    }
    Some((counts, pids))
}

/// The newest name each thread was given (`/rename`, or Codex's own title),
/// from `session_index.jsonl`.
fn thread_names(home: &Path) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        thread_name: Option<String>,
    }
    let Ok(text) = std::fs::read_to_string(home.join("session_index.jsonl")) else {
        return HashMap::new();
    };
    let mut names = HashMap::new();
    for entry in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
    {
        if let Some(name) = entry.thread_name.filter(|name| !name.trim().is_empty()) {
            names.insert(entry.id.to_ascii_lowercase(), name);
        }
    }
    names
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: serde_json::Value,
}

fn compressed(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".zst")
}

/// How many bytes of `bytes` are whole lines: up to its last line break.
fn whole_lines(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at as u64 + 1)
}

/// The facts a rollout gives: its meta from the head, its latest turn event
/// and context use from the tail; and where its last whole line ends. A
/// compressed rollout is an old one, never open; it is read whole all the
/// same.
fn read_facts(path: &Path, size: u64) -> Option<(RolloutFacts, u64)> {
    if compressed(path) {
        let bytes = sl_usage::codex::rollout_bytes(path)?;
        return Some((parse_facts(&bytes, &bytes), size));
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.by_ref()
        .take(HEAD_BYTES.min(size))
        .read_to_end(&mut head)
        .ok()?;
    let mut facts = parse_facts(&head, &[]);
    let mut turn_found = false;
    // A long turn of many tool calls can push its own `task_started` far back:
    // read further back, a block at a time, until a turn event is found. Each
    // block is read and searched once; the line it starts in the middle of is
    // carried over to be finished by the block before.
    let mut end = size;
    let mut read_to = None;
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let from = end.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut block = Vec::new();
        file.by_ref()
            .take(end - from)
            .read_to_end(&mut block)
            .ok()?;
        let read_to = *read_to.get_or_insert(from + whole_lines(&block));
        block.extend_from_slice(&carry);
        let start = if from == 0 {
            0
        } else {
            block
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(block.len(), |at| at + 1)
        };
        scan_tail(&block[start..], &mut facts, &mut turn_found);
        if turn_found || from == 0 {
            return Some((facts, read_to));
        }
        // A turn event is a short line: one longer than a block is not
        // worth finishing.
        carry = if start as u64 <= TAIL_BYTES {
            block[..start].to_vec()
        } else {
            Vec::new()
        };
        end = from;
    }
}

/// `known`, a rollout's facts as far as `from`, brought up to date with what
/// was appended since, up to `size`: a turn event or context reading there is
/// the newer one. Also where the last whole line read ends.
fn read_appended(
    path: &Path,
    from: u64,
    size: u64,
    known: &RolloutFacts,
) -> Option<(RolloutFacts, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.take(size - from).read_to_end(&mut bytes).ok()?;
    let mut fresh = RolloutFacts::default();
    let mut turn_found = false;
    scan_tail(&bytes, &mut fresh, &mut turn_found);
    let mut facts = known.clone();
    if turn_found {
        facts.working = fresh.working;
        facts.last_activity_ms = fresh.last_activity_ms;
    }
    if fresh.context.is_some() {
        facts.context = fresh.context;
    }
    Some((facts, from + whole_lines(&bytes)))
}

fn parse_facts(head: &[u8], tail: &[u8]) -> RolloutFacts {
    let mut facts = RolloutFacts::default();
    // The first line is the rollout's own `session_meta`.
    if let Some(first) = head.split(|byte| *byte == b'\n').next()
        && let Ok(line) = serde_json::from_slice::<Line>(first)
        && line.kind == "session_meta"
    {
        let payload = &line.payload;
        let originator = payload
            .get("originator")
            .and_then(|o| o.as_str())
            .unwrap_or_default();
        let subagent = payload
            .get("source")
            .and_then(|source| source.as_object())
            .is_some_and(|source| source.contains_key("subagent"));
        if let Some(cwd) = payload.get("cwd").and_then(|cwd| cwd.as_str()) {
            facts.meta = Some(Meta {
                cwd: cwd.to_owned(),
                started_ms: line
                    .timestamp
                    .as_deref()
                    .and_then(sl_core::format::parse_iso8601_to_epoch_ms)
                    .unwrap_or(0),
                hidden: originator == "Claude Code" || subagent,
            });
        }
    }
    scan_tail(tail, &mut facts, &mut false);
    facts
}

/// Newest line first, the latest turn event and the latest context reading
/// in `tail`, each unless `facts` already has one from a newer stretch
/// (`turn_found`, for the turn).
fn scan_tail(tail: &[u8], facts: &mut RolloutFacts, turn_found: &mut bool) {
    for line in tail.rsplit(|byte| *byte == b'\n') {
        if *turn_found && facts.context.is_some() {
            break;
        }
        let head = &line[..line.len().min(200)];
        let has = |needle: &[u8]| head.windows(needle.len()).any(|window| window == needle);
        let turn = has(br#""type":"task_started""#)
            || has(br#""type":"task_complete""#)
            || has(br#""type":"turn_aborted""#);
        let count = has(br#""type":"token_count""#);
        if !(turn && !*turn_found || count && facts.context.is_none()) {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<Line>(line) else {
            continue;
        };
        let at = parsed
            .timestamp
            .as_deref()
            .and_then(sl_core::format::parse_iso8601_to_epoch_ms);
        let event = parsed
            .payload
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        if turn && !*turn_found {
            *turn_found = true;
            facts.working = event == "task_started";
            facts.last_activity_ms = at;
        } else if count && facts.context.is_none() {
            facts.context = context_of(&parsed.payload, at.unwrap_or(0));
        }
    }
}

/// Context use from a `token_count`: the last request's input over the model's
/// window, as the TUI's own context meter reads it.
fn context_of(payload: &serde_json::Value, at_ms: i64) -> Option<ContextCapture> {
    let info = payload.get("info")?;
    let used = info
        .get("last_token_usage")?
        .get("input_tokens")?
        .as_i64()?;
    let limit = info
        .get("model_context_window")?
        .as_i64()
        .filter(|limit| *limit > 0)?;
    Some(ContextCapture {
        percent: used as f64 / limit as f64 * 100.0,
        used_tokens: Some(used),
        limit_tokens: Some(limit),
        captured_at_ms: at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(originator: &str, source: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-11-09T22:10:32.945Z","type":"session_meta","payload":{{"id":"x","cwd":"/repo","originator":"{originator}","source":{source}}}}}"#
        )
    }

    fn event(ts: &str, kind: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"{kind}","turn_id":"t"}}}}"#
        )
    }

    fn token_count(used: i64, window: i64) -> String {
        format!(
            r#"{{"timestamp":"2026-11-09T22:10:36.025Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{used}}},"model_context_window":{window}}},"rate_limits":null}}}}"#
        )
    }

    fn facts(lines: &[String]) -> RolloutFacts {
        let bytes = lines.join("\n").into_bytes();
        parse_facts(&bytes, &bytes)
    }

    #[test]
    fn a_turn_running_is_working_and_a_finished_one_idle_since_it_ended() {
        let working = facts(&[
            meta("codex-tui", r#""cli""#),
            event("2026-11-09T22:10:00.000Z", "task_complete"),
            event("2026-11-09T22:12:00.000Z", "task_started"),
        ]);
        assert!(working.working);
        let idle = facts(&[
            meta("codex-tui", r#""cli""#),
            event("2026-11-09T22:10:00.000Z", "task_started"),
            event("2026-11-09T22:11:00.000Z", "turn_aborted"),
        ]);
        assert!(!idle.working);
        assert_eq!(
            idle.last_activity_ms,
            sl_core::format::parse_iso8601_to_epoch_ms("2026-11-09T22:11:00.000Z")
        );
        assert_eq!(
            idle.meta.as_ref().map(|m| (m.cwd.as_str(), m.hidden)),
            Some(("/repo", false))
        );
    }

    #[test]
    fn a_thread_with_no_turn_yet_has_no_activity() {
        assert_eq!(
            facts(&[meta("codex-tui", r#""cli""#)]).last_activity_ms,
            None
        );
    }

    #[test]
    fn plugin_runs_and_subagents_are_hidden() {
        assert!(
            facts(&[meta("Claude Code", r#""exec""#)])
                .meta
                .unwrap()
                .hidden
        );
        assert!(
            facts(&[meta("codex-tui", r#"{"subagent":{"other":"guardian"}}"#)])
                .meta
                .unwrap()
                .hidden
        );
        assert!(
            facts(&[meta("codex-tui", r#"{"subagent":"review"}"#)])
                .meta
                .unwrap()
                .hidden
        );
        assert!(
            !facts(&[meta("zed", r#""vscode""#)]).meta.unwrap().hidden,
            "an IDE session is a session"
        );
    }

    #[test]
    fn context_use_is_the_last_requests_input_over_the_window() {
        let found = facts(&[
            meta("codex-tui", r#""cli""#),
            token_count(47_500, 475_000),
            event("2026-11-09T22:12:00.000Z", "task_complete"),
        ]);
        let context = found.context.expect("a reading");
        assert!((context.percent - 10.0).abs() < 1e-9);
        assert_eq!(
            (context.used_tokens, context.limit_tokens),
            (Some(47_500), Some(475_000))
        );
    }

    #[test]
    fn lsof_fields_pair_each_file_with_its_process() {
        let out = "p41098\nf54\nn/h/thread-writer-locks/a.lock\nf68\nn/h/thread-writer-locks/b.lock\np27857\nn/h/thread-writer-locks/c.lock\n";
        assert_eq!(
            lsof_fields(out),
            vec![
                (41098, "/h/thread-writer-locks/a.lock".to_owned()),
                (41098, "/h/thread-writer-locks/b.lock".to_owned()),
                (27857, "/h/thread-writer-locks/c.lock".to_owned())
            ]
        );
    }

    #[test]
    fn a_lock_is_matched_by_its_resolved_path_as_lsof_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("thread-writer-locks")).unwrap();
        std::fs::write(real.join("thread-writer-locks/abc.lock"), "").unwrap();
        let home = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &home).unwrap();
        let locks = lock_files(&home);
        let named = std::fs::canonicalize(real.join("thread-writer-locks/abc.lock")).unwrap();
        let output = format!("p42\nf3\nn{}\n", named.display());
        assert_eq!(holders_in(&locks, &output), [("abc".to_owned(), 42)]);
    }

    /// A turn of many tool calls writes far more than [`TAIL_BYTES`] after
    /// its `task_started`: the session must stay Busy, not drop out.
    #[test]
    fn a_long_turn_is_found_further_back_than_the_tail() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let filler = format!(
            r#"{{"timestamp":"2026-11-09T22:12:01.000Z","type":"response_item","payload":{{"type":"function_call_output","output":"{}"}}}}"#,
            "x".repeat(4000)
        );
        let mut lines = vec![
            meta("codex-tui", r#""cli""#),
            event("2026-11-09T22:12:00.000Z", "task_started"),
        ];
        lines.extend(std::iter::repeat_n(filler, 100));
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > TAIL_BYTES, "the turn's start is outside the tail");
        let (found, _) = read_facts(&path, size).unwrap();
        assert!(found.working, "still the turn that started 400 KB ago");
        assert!(found.last_activity_ms.is_some());

        lines.push(event("2026-11-09T22:20:00.000Z", "task_complete"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (found, _) = read_facts(&path, std::fs::metadata(&path).unwrap().len()).unwrap();
        assert!(!found.working);
    }

    /// A [`CodexSessions`] that watches no directory.
    fn quiet_sessions() -> CodexSessions {
        CodexSessions {
            facts: HashMap::new(),
            walk: sl_usage::paths::Walk::new(sl_usage::codex::is_rollout),
            holders: None,
            room: None,
        }
    }

    #[test]
    fn what_lsof_said_holds_until_a_process_it_named_goes_or_it_grows_old() {
        let t0 = Instant::now();
        let asked = Asked {
            at: t0,
            answer: 1,
            pids: vec![(7, Some(100))],
        };
        let soon = t0 + Duration::from_secs(5);
        let running = |pid: u32| (pid == 7).then_some(100);
        assert_eq!(asked.holding(soon, running), Some(1));
        assert_eq!(asked.holding(soon, |_| None), None, "it exited");
        assert_eq!(
            asked.holding(soon, |_| Some(200)),
            None,
            "its pid is another's"
        );
        assert_eq!(asked.holding(t0 + ASKED_MAX_AGE, running), None, "too old");
        assert_eq!(
            asked.standing(t0 + ASKED_MAX_AGE),
            Some(1),
            "still good to stand in when asking fails"
        );
        assert_eq!(asked.standing(t0 + ASKED_STANDS_FOR), None);
    }

    #[test]
    fn the_same_locks_held_by_running_processes_are_not_asked_about_again() {
        let mut sessions = quiet_sessions();
        let locks = vec![("abc".to_owned(), PathBuf::from("/nowhere/abc.lock"))];
        let me = std::process::id();
        let now = Instant::now();
        sessions.holders = Some((
            locks.clone(),
            Asked::new(now, vec![("abc".to_owned(), me)], [me]),
        ));
        assert_eq!(
            sessions.holders_of(locks, now + Duration::from_secs(3)),
            [("abc".to_owned(), me)],
            "what lsof said, not asked again: it would find nobody holding /nowhere"
        );
    }

    fn filler(len: usize) -> String {
        format!(
            r#"{{"timestamp":"2026-11-09T22:12:01.000Z","type":"response_item","payload":{{"type":"function_call_output","output":"{}"}}}}"#,
            "x".repeat(len)
        )
    }

    #[test]
    fn a_turn_event_cut_by_a_block_edge_is_found_blocks_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let head = format!("{}\n", meta("codex-tui", r#""cli""#));
        let started = format!("{}\n", event("2026-11-09T22:12:00.000Z", "task_started"));
        let body: String = std::iter::repeat_n(filler(4000) + "\n", 200).collect();
        let newest = format!("{}\n", token_count(47_500, 475_000));
        let rollout = |pad: usize| format!("{head}{started}{body}{}\n{newest}", filler(pad));
        // Pad so that a block edge falls 10 bytes into `task_started`'s line.
        let edge = (head.len() + 10) as u64;
        let unpadded = rollout(0).len() as u64;
        let blocks = (unpadded - edge).div_ceil(TAIL_BYTES);
        let text = rollout(usize::try_from(edge + blocks * TAIL_BYTES - unpadded).unwrap());
        std::fs::write(&path, &text).unwrap();
        let size = text.len() as u64;
        assert!(blocks >= 3 && (size - edge).is_multiple_of(TAIL_BYTES));
        let (found, read_to) = read_facts(&path, size).unwrap();
        assert!(found.working, "the turn whose line two blocks share");
        assert!(found.context.is_some(), "and the newest reading");
        assert_eq!(read_to, size);
    }

    #[test]
    fn a_grown_rollout_is_read_from_where_the_last_read_ended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let text = format!(
            "{}\n{}\n",
            meta("codex-tui", r#""cli""#),
            event("2026-11-09T22:12:00.000Z", "task_started")
        );
        std::fs::write(&path, &text).unwrap();
        let mut sessions = quiet_sessions();
        let first = sessions.facts_of(&path);
        assert!(first.working);
        // What was read is not read again: changed in place (which no writer
        // does), it still counts as it was.
        let text =
            text.replace("task_started", "task_startex") + &token_count(47_500, 475_000) + "\n";
        std::fs::write(&path, &text).unwrap();
        let grown = sessions.facts_of(&path);
        assert!(grown.working);
        assert_eq!(grown.last_activity_ms, first.last_activity_ms);
        assert!(grown.context.is_some(), "the reading appended since");
        // A line caught half written is read whole once it is.
        let done = event("2026-11-09T22:20:00.000Z", "task_complete");
        let (start, rest) = done.split_at(40);
        std::fs::write(&path, text.clone() + start).unwrap();
        assert!(sessions.facts_of(&path).working);
        std::fs::write(&path, text + start + rest + "\n").unwrap();
        assert!(!sessions.facts_of(&path).working);
    }
}
