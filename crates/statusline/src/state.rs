use crate::types::SessionState;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const PRUNE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// State is keyed by session_id, one file per session. A single shared file
/// (what claude-pulse does) makes every session in every project increment
/// the same counter, which is how a fresh project ends up showing thousands
/// of tool uses.
pub fn default_state_dir() -> PathBuf {
    sl_core::paths::cache_dir().join("sessions")
}

fn session_path(state_dir: &Path, session_id: &str) -> PathBuf {
    // session_id is a UUID, but never trust it as a path component
    let sanitized: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    state_dir.join(format!("{sanitized}.json"))
}

pub fn read_session_state(state_dir: &Path, session_id: &str) -> Option<SessionState> {
    sl_core::paths::read_json(&session_path(state_dir, session_id))
}

/// PostToolUse hook entry point: bump this session's counters.
pub fn bump_session_state(state_dir: &Path, session_id: &str, tool_name: Option<&str>, now_ms: i64) -> SessionState {
    fs::create_dir_all(state_dir).ok();
    let prev = read_session_state(state_dir, session_id);
    let next = SessionState {
        tool_count: prev.as_ref().map_or(0, |p| p.tool_count) + 1,
        session_start: prev.as_ref().map_or(now_ms, |p| p.session_start),
        last_tool: tool_name.map(String::from).or_else(|| prev.as_ref().and_then(|p| p.last_tool.clone())),
        last_refresh: now_ms,
    };

    sl_core::paths::write_json_atomic(&session_path(state_dir, session_id), &next);

    if next.tool_count % 200 == 1 {
        prune_stale(state_dir, now_ms);
    }
    next
}

/// Drop session files untouched for a week so the dir doesn't grow forever.
pub fn prune_stale(state_dir: &Path, now_ms: i64) {
    let Ok(entries) = fs::read_dir(state_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .is_some_and(|dur| now_ms - i64::try_from(dur.as_millis()).unwrap_or(i64::MAX) > PRUNE_AFTER_MS);
        if stale {
            // a concurrent hook may have removed it already
            fs::remove_file(&path).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};


    /// A scratch directory of its own, removed when the guard is dropped.
    fn temp_dir() -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path().to_path_buf();
        (guard, dir)
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(FileTimes::new().set_modified(when)).unwrap();
    }

    #[test]
    fn unknown_session_has_no_state() {
        assert!(read_session_state(&temp_dir().1, "nope").is_none());
    }

    #[test]
    fn bump_counts_tools_and_keeps_the_original_start_time() {
        let (_dir_guard, dir) = temp_dir();
        let t0 = 1_000_000;
        bump_session_state(&dir, "s1", Some("Bash"), t0);
        let state = bump_session_state(&dir, "s1", Some("Edit"), t0 + 60_000);
        assert_eq!(state.tool_count, 2);
        assert_eq!(state.session_start, t0);
        assert_eq!(state.last_tool.as_deref(), Some("Edit"));
        assert_eq!(read_session_state(&dir, "s1").unwrap().tool_count, 2);
    }

    #[test]
    fn sessions_are_isolated_the_claude_pulse_bug() {
        let (_dir_guard, dir) = temp_dir();
        for i in 0..5 {
            bump_session_state(&dir, "busy-session", Some("Bash"), 1_000 + i);
        }
        // A different session (e.g. a brand-new project) must start from
        // zero, not inherit the other session's count.
        assert!(read_session_state(&dir, "fresh-session").is_none());
        let fresh = bump_session_state(&dir, "fresh-session", Some("Read"), 9_999);
        assert_eq!(fresh.tool_count, 1);
        assert_eq!(read_session_state(&dir, "busy-session").unwrap().tool_count, 5);
    }

    #[test]
    fn hostile_session_ids_cannot_escape_the_state_dir() {
        let (_dir_guard, dir) = temp_dir();
        let state = bump_session_state(&dir, "../../evil", Some("Bash"), 1_000);
        assert_eq!(state.tool_count, 1);
        assert_eq!(read_session_state(&dir, "../../evil").unwrap().tool_count, 1);
    }

    #[test]
    fn corrupt_state_files_read_as_none() {
        let (_dir_guard, dir) = temp_dir();
        fs::write(dir.join("bad.json"), "{not json").unwrap();
        assert!(read_session_state(&dir, "bad").is_none());
    }

    #[test]
    fn prune_removes_only_week_old_sessions() {
        let (_dir_guard, dir) = temp_dir();
        let now_ms = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
        bump_session_state(&dir, "old", Some("Bash"), now_ms);
        bump_session_state(&dir, "recent", Some("Bash"), now_ms);
        let old_time = SystemTime::now() - Duration::from_hours(8 * 24);
        set_mtime(&dir.join("old.json"), old_time);
        prune_stale(&dir, now_ms);
        assert!(read_session_state(&dir, "old").is_none());
        assert!(read_session_state(&dir, "recent").is_some());
    }
}
