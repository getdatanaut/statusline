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
use std::time::Duration;

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

/// What one rollout says, kept against its size and mtime so an unchanged one
/// is not read again.
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

/// What is kept between passes.
#[derive(Debug)]
pub struct CodexSessions {
    facts: HashMap<PathBuf, (u64, i64, RolloutFacts)>,
    /// The rollouts' directories, read again only when they change.
    walk: sl_usage::paths::Walk,
}

impl Default for CodexSessions {
    fn default() -> Self {
        Self { facts: HashMap::new(), walk: sl_usage::paths::Walk::new(sl_usage::codex::is_rollout).ringing(|| crate::sessions::SESSIONS_BELL.ring()) }
    }
}

impl CodexSessions {
    /// The open Codex sessions under `home`, unenriched: no branch or PR yet.
    pub fn load(&mut self, home: &Path) -> Vec<OpenSession> {
        let locks = lock_files(home);
        if locks.is_empty() {
            self.facts.clear();
            return Vec::new();
        }
        let holders = lock_holders(&locks);
        if holders.is_empty() {
            return Vec::new();
        }
        let rollouts: HashMap<String, PathBuf> =
            sl_usage::codex::codex_files_with(home, &mut self.walk).into_iter().filter_map(|path| Some((sl_usage::codex::thread_of(&path)?, path))).collect();
        let daemon = daemon_pid(home);
        let names = thread_names(home);

        let mut candidates: Vec<(OpenSession, bool)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for (thread, pid) in holders {
            let Some(path) = rollouts.get(&thread) else { continue };
            seen.insert(path.clone());
            let facts = self.facts_of(path);
            let Some(meta) = facts.meta.as_ref().filter(|meta| !meta.hidden) else { continue };
            let Some(last_activity_ms) = facts.last_activity_ms else { continue };
            let by_daemon = daemon == Some(pid);
            let session = OpenSession {
                session_id: thread.clone(),
                name: names.get(&thread).cloned().unwrap_or_else(|| dir_name(&meta.cwd)),
                project: String::new(),
                cwd: meta.cwd.clone(),
                activity: if facts.working { Activity::Busy } else { Activity::Idle },
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
        let daemon_held: Vec<&OpenSession> = candidates.iter().filter(|(_, by_daemon)| *by_daemon).map(|(session, _)| session).collect();
        let room = if daemon_held.is_empty() { HashMap::new() } else { interactive_cwds(daemon) };
        let mut by_cwd: HashMap<&str, Vec<&OpenSession>> = HashMap::new();
        for session in daemon_held {
            by_cwd.entry(session.cwd.as_str()).or_default().push(session);
        }
        let mut kept: HashSet<String> = HashSet::new();
        for (cwd, mut sessions) in by_cwd {
            sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_ms));
            kept.extend(sessions.into_iter().take(room.get(cwd).copied().unwrap_or(0)).map(|session| session.session_id.clone()));
        }
        candidates.into_iter().filter(|(session, by_daemon)| !by_daemon || kept.contains(&session.session_id)).map(|(session, _)| session).collect()
    }

    fn facts_of(&mut self, path: &Path) -> RolloutFacts {
        let Some((size, mtime)) = stat(path) else { return RolloutFacts::default() };
        if let Some((held_size, held_mtime, facts)) = self.facts.get(path)
            && *held_size == size
            && *held_mtime == mtime
        {
            return facts.clone();
        }
        let facts = read_facts(path, size).unwrap_or_default();
        self.facts.insert(path.to_path_buf(), (size, mtime, facts.clone()));
        facts
    }
}

fn stat(path: &Path) -> Option<(u64, i64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = i64::try_from(metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()).unwrap_or(i64::MAX);
    Some((metadata.len(), mtime))
}

fn dir_name(cwd: &str) -> String {
    cwd.rsplit('/').find(|part| !part.is_empty()).unwrap_or(cwd).to_owned()
}

/// Every per-thread lock file, as `(thread, path)`.
fn lock_files(home: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(home.join("thread-writer-locks")) else { return Vec::new() };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let thread = name.strip_suffix(".lock").filter(|thread| !thread.starts_with('.'))?.to_ascii_lowercase();
            Some((thread, entry.path()))
        })
        .collect()
}

/// Which process holds each lock file open: one `lsof` over all of them.
/// A file no process has open is stale, and left out.
fn lock_holders(locks: &[(String, PathBuf)]) -> Vec<(String, u32)> {
    let mut args: Vec<String> = vec!["-n".into(), "-P".into(), "-F".into(), "pn".into(), "--".into()];
    args.extend(locks.iter().map(|(_, path)| path.display().to_string()));
    let Some(output) = lsof(&args) else { return Vec::new() };
    let by_path: HashMap<String, &str> = locks.iter().map(|(thread, path)| (path.display().to_string(), thread.as_str())).collect();
    let mut held = Vec::new();
    for (pid, name) in lsof_fields(&output) {
        if let Some(thread) = by_path.get(&name) {
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

fn lsof(args: &[String]) -> Option<String> {
    // `-w`: no warnings on stderr, which would stand in for the output below.
    let args: Vec<&str> = std::iter::once("-w").chain(args.iter().map(String::as_str)).collect();
    // `lsof` exits 1 when some of the files are open by nobody; its output
    // still lists the ones that are, and `run_for` hands it back as the error.
    match crate::actions::run_for("/usr/sbin/lsof", &args, "/", LSOF_TIMEOUT) {
        Ok(out) => Some(out),
        Err(out) => out.starts_with('p').then_some(out),
    }
}

/// The managed app-server daemon's pid, from its own pid file.
fn daemon_pid(home: &Path) -> Option<u32> {
    #[derive(Deserialize)]
    struct PidFile {
        pid: u32,
    }
    let text = std::fs::read_to_string(home.join("app-server-daemon/daemon.pid")).ok()?;
    serde_json::from_str::<PidFile>(&text).ok().map(|file| file.pid)
}

/// The directories interactive `codex` processes run in, and how many in
/// each: the TUIs (and anything else running `codex` that is not the daemon
/// or one of its helpers).
fn interactive_cwds(daemon: Option<u32>) -> HashMap<String, usize> {
    let Ok(ps) = crate::actions::run_for("/bin/ps", &["-axo", "pid=,comm=,args="], "/", LSOF_TIMEOUT) else { return HashMap::new() };
    let pids: Vec<String> = ps
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let comm = parts.next()?;
            let args: Vec<&str> = parts.collect();
            let is_codex = comm == "codex" || comm.ends_with("/codex");
            let helper = args.iter().any(|arg| matches!(*arg, "app-server" | "exec-server" | "mcp-server" | "--managed-daemon" | "pid-update-loop"));
            (is_codex && !helper && Some(pid) != daemon).then(|| pid.to_string())
        })
        .collect();
    if pids.is_empty() {
        return HashMap::new();
    }
    let args = ["-n".to_owned(), "-P".to_owned(), "-a".to_owned(), "-d".to_owned(), "cwd".to_owned(), "-F".to_owned(), "pn".to_owned(), "-p".to_owned(), pids.join(",")];
    let mut counts = HashMap::new();
    for (_, cwd) in lsof_fields(&lsof(&args).unwrap_or_default()) {
        *counts.entry(cwd).or_insert(0) += 1;
    }
    counts
}

/// The newest name each thread was given (`/rename`, or Codex's own title),
/// from `session_index.jsonl`.
fn thread_names(home: &Path) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        thread_name: Option<String>,
    }
    let Ok(text) = std::fs::read_to_string(home.join("session_index.jsonl")) else { return HashMap::new() };
    let mut names = HashMap::new();
    for entry in text.lines().filter_map(|line| serde_json::from_str::<Entry>(line).ok()) {
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

/// The facts a rollout gives: its meta from the head, its latest turn event
/// and context use from the tail. A compressed rollout is an old one, never
/// open; it is read whole all the same.
fn read_facts(path: &Path, size: u64) -> Option<RolloutFacts> {
    if path.to_string_lossy().ends_with(".zst") {
        let bytes = sl_usage::codex::rollout_bytes(path)?;
        return Some(parse_facts(&bytes, &bytes));
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.by_ref().take(HEAD_BYTES.min(size)).read_to_end(&mut head).ok()?;
    // A long turn of many tool calls can push its own `task_started` far back:
    // read further back, a block at a time, until a turn event is found.
    let mut from = size;
    let mut tail: Vec<u8> = Vec::new();
    loop {
        from = from.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut block = Vec::new();
        file.by_ref().take(TAIL_BYTES.min(size - from)).read_to_end(&mut block).ok()?;
        block.extend_from_slice(&tail);
        tail = block;
        let facts = parse_facts(&head, &tail);
        if facts.last_activity_ms.is_some() || from == 0 {
            return Some(facts);
        }
    }
}

fn parse_facts(head: &[u8], tail: &[u8]) -> RolloutFacts {
    let mut facts = RolloutFacts::default();
    // The first line is the rollout's own `session_meta`.
    if let Some(first) = head.split(|byte| *byte == b'\n').next()
        && let Ok(line) = serde_json::from_slice::<Line>(first)
        && line.kind == "session_meta"
    {
        let payload = &line.payload;
        let originator = payload.get("originator").and_then(|o| o.as_str()).unwrap_or_default();
        let subagent = payload.get("source").and_then(|source| source.as_object()).is_some_and(|source| source.contains_key("subagent"));
        if let Some(cwd) = payload.get("cwd").and_then(|cwd| cwd.as_str()) {
            facts.meta = Some(Meta {
                cwd: cwd.to_owned(),
                started_ms: line.timestamp.as_deref().and_then(sl_core::format::parse_iso8601_to_epoch_ms).unwrap_or(0),
                hidden: originator == "Claude Code" || subagent,
            });
        }
    }
    // Newest first: the latest turn event, and the latest context reading.
    let mut turn_found = false;
    for line in tail.rsplit(|byte| *byte == b'\n') {
        if turn_found && facts.context.is_some() {
            break;
        }
        let head = &line[..line.len().min(200)];
        let has = |needle: &[u8]| head.windows(needle.len()).any(|window| window == needle);
        let turn = has(br#""type":"task_started""#) || has(br#""type":"task_complete""#) || has(br#""type":"turn_aborted""#);
        let count = has(br#""type":"token_count""#);
        if !(turn && !turn_found || count && facts.context.is_none()) {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<Line>(line) else { continue };
        let at = parsed.timestamp.as_deref().and_then(sl_core::format::parse_iso8601_to_epoch_ms);
        let event = parsed.payload.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        if turn && !turn_found {
            turn_found = true;
            facts.working = event == "task_started";
            facts.last_activity_ms = at;
        } else if count && facts.context.is_none() {
            facts.context = context_of(&parsed.payload, at.unwrap_or(0));
        }
    }
    facts
}

/// Context use from a `token_count`: the last request's input over the model's
/// window, as the TUI's own context meter reads it.
fn context_of(payload: &serde_json::Value, at_ms: i64) -> Option<ContextCapture> {
    let info = payload.get("info")?;
    let used = info.get("last_token_usage")?.get("input_tokens")?.as_i64()?;
    let limit = info.get("model_context_window")?.as_i64().filter(|limit| *limit > 0)?;
    Some(ContextCapture { percent: used as f64 / limit as f64 * 100.0, used_tokens: Some(used), limit_tokens: Some(limit), captured_at_ms: at_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(originator: &str, source: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-09T22:10:32.945Z","type":"session_meta","payload":{{"id":"x","cwd":"/repo","originator":"{originator}","source":{source}}}}}"#)
    }

    fn event(ts: &str, kind: &str) -> String {
        format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"{kind}","turn_id":"t"}}}}"#)
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
        let working = facts(&[meta("codex-tui", r#""cli""#), event("2026-11-09T22:10:00.000Z", "task_complete"), event("2026-11-09T22:12:00.000Z", "task_started")]);
        assert!(working.working);
        let idle = facts(&[meta("codex-tui", r#""cli""#), event("2026-11-09T22:10:00.000Z", "task_started"), event("2026-11-09T22:11:00.000Z", "turn_aborted")]);
        assert!(!idle.working);
        assert_eq!(idle.last_activity_ms, sl_core::format::parse_iso8601_to_epoch_ms("2026-11-09T22:11:00.000Z"));
        assert_eq!(idle.meta.as_ref().map(|m| (m.cwd.as_str(), m.hidden)), Some(("/repo", false)));
    }

    #[test]
    fn a_thread_with_no_turn_yet_has_no_activity() {
        assert_eq!(facts(&[meta("codex-tui", r#""cli""#)]).last_activity_ms, None);
    }

    #[test]
    fn plugin_runs_and_subagents_are_hidden() {
        assert!(facts(&[meta("Claude Code", r#""exec""#)]).meta.unwrap().hidden);
        assert!(facts(&[meta("codex-tui", r#"{"subagent":{"other":"guardian"}}"#)]).meta.unwrap().hidden);
        assert!(facts(&[meta("codex-tui", r#"{"subagent":"review"}"#)]).meta.unwrap().hidden);
        assert!(!facts(&[meta("zed", r#""vscode""#)]).meta.unwrap().hidden, "an IDE session is a session");
    }

    #[test]
    fn context_use_is_the_last_requests_input_over_the_window() {
        let found = facts(&[meta("codex-tui", r#""cli""#), token_count(47_500, 475_000), event("2026-11-09T22:12:00.000Z", "task_complete")]);
        let context = found.context.expect("a reading");
        assert!((context.percent - 10.0).abs() < 1e-9);
        assert_eq!((context.used_tokens, context.limit_tokens), (Some(47_500), Some(475_000)));
    }

    #[test]
    fn lsof_fields_pair_each_file_with_its_process() {
        let out = "p41098\nf54\nn/h/thread-writer-locks/a.lock\nf68\nn/h/thread-writer-locks/b.lock\np27857\nn/h/thread-writer-locks/c.lock\n";
        assert_eq!(
            lsof_fields(out),
            vec![(41098, "/h/thread-writer-locks/a.lock".to_owned()), (41098, "/h/thread-writer-locks/b.lock".to_owned()), (27857, "/h/thread-writer-locks/c.lock".to_owned())]
        );
    }

    /// A turn of many tool calls writes far more than [`TAIL_BYTES`] after
    /// its `task_started`: the session must stay Busy, not drop out.
    #[test]
    fn a_long_turn_is_found_further_back_than_the_tail() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let filler = format!(r#"{{"timestamp":"2026-11-09T22:12:01.000Z","type":"response_item","payload":{{"type":"function_call_output","output":"{}"}}}}"#, "x".repeat(4000));
        let mut lines = vec![meta("codex-tui", r#""cli""#), event("2026-11-09T22:12:00.000Z", "task_started")];
        lines.extend(std::iter::repeat_n(filler, 100));
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > TAIL_BYTES, "the turn's start is outside the tail");
        let found = read_facts(&path, size).unwrap();
        assert!(found.working, "still the turn that started 400 KB ago");
        assert!(found.last_activity_ms.is_some());

        lines.push(event("2026-11-09T22:20:00.000Z", "task_complete"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        let found = read_facts(&path, std::fs::metadata(&path).unwrap().len()).unwrap();
        assert!(!found.working);
    }
}
