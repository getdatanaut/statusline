//! Best-effort capture of each session's context use, for the tray: the same
//! numbers the status line's Context bar shows, written to
//! `<cache_dir>/context/<session id>.json` every time the line renders. The
//! tray draws them as the ring on each idle session's row, so it never has to
//! guess a session's window size or auto-compact setting.
//!
//! Like the rate-limit capture (`limits`), it runs after the line is printed
//! and fails silently: it is a side effect of the status line, never a
//! condition of it.

use std::path::Path;

use sl_core::types::{ContextCapture, StatusPayload};

use crate::render::context_use;

/// Capture `payload`'s context use for its session. Nothing is written for a
/// payload with no session id or no context figures yet.
pub fn capture_context(
    cache_dir: &Path,
    payload: &StatusPayload,
    auto_compact_window: Option<i64>,
    now_ms: i64,
) {
    let Some(path) = payload
        .session_id
        .as_deref()
        .and_then(|id| sl_core::paths::context_file(cache_dir, id))
    else {
        return;
    };
    let Some((percent, tokens)) = context_use(payload, auto_compact_window) else {
        return;
    };
    // A session that has not run a turn since Claude Code started reports
    // nothing used; that is no reading, and a 0% ring would claim one.
    if percent <= 0.0 {
        return;
    }
    // A session's first capture clears out those of sessions gone for a week,
    // and any temp file a failed write left behind.
    if !path.exists() {
        crate::state::prune_stale(&cache_dir.join(sl_core::paths::CONTEXT_DIR), now_ms);
    }
    let capture = ContextCapture {
        percent,
        used_tokens: tokens.map(|(used, _)| used),
        limit_tokens: tokens.map(|(_, limit)| limit),
        captured_at_ms: now_ms,
    };
    // Atomically: the tray reads on its own schedule and must never catch a
    // half-written file.
    sl_core::paths::write_json_atomic(&path, &capture);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_core::types::ContextWindow;
    use std::fs;
    use std::path::PathBuf;

    /// A scratch directory that does *not* exist yet, inside one removed
    /// when the guard is dropped: every test also covers the "cache dir has
    /// never been created" path.
    fn temp_dir() -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path().join("cache");
        (guard, dir)
    }

    fn payload(session: Option<&str>, input: Option<i64>) -> StatusPayload {
        StatusPayload {
            session_id: session.map(str::to_owned),
            context_window: Some(ContextWindow {
                total_input_tokens: input,
                total_output_tokens: Some(10_000),
                context_window_size: Some(200_000),
                used_percentage: Some(12.0),
                current_usage: None,
            }),
            ..StatusPayload::default()
        }
    }

    #[test]
    fn the_capture_holds_what_the_context_bar_shows() {
        let (_dir_guard, dir) = temp_dir();
        capture_context(
            &dir,
            &payload(Some("abc-123"), Some(70_000)),
            Some(160_000),
            42,
        );
        let text =
            fs::read_to_string(sl_core::paths::context_file(&dir, "abc-123").unwrap()).unwrap();
        let capture: ContextCapture = serde_json::from_str(&text).unwrap();
        assert_eq!(capture.used_tokens, Some(80_000));
        assert_eq!(
            capture.limit_tokens,
            Some(160_000),
            "the auto-compact window, being smaller"
        );
        assert!((capture.percent - 50.0).abs() < 1e-9);
        assert_eq!(capture.captured_at_ms, 42);
    }

    #[test]
    fn early_payloads_fall_back_to_the_percentage_and_odd_ids_write_nothing() {
        let (_dir_guard, dir) = temp_dir();
        capture_context(&dir, &payload(Some("early"), None), None, 1);
        let capture: ContextCapture = serde_json::from_str(
            &fs::read_to_string(sl_core::paths::context_file(&dir, "early").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!((capture.percent, capture.used_tokens), (12.0, None));
        capture_context(&dir, &payload(Some("../escape"), Some(1)), None, 1);
        capture_context(&dir, &payload(None, Some(1)), None, 1);
        let names: Vec<String> = fs::read_dir(dir.join(sl_core::paths::CONTEXT_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["early.json"]);
    }

    #[test]
    fn a_new_sessions_capture_prunes_week_old_ones() {
        let (_dir_guard, dir) = temp_dir();
        let context_dir = dir.join(sl_core::paths::CONTEXT_DIR);
        fs::create_dir_all(&context_dir).unwrap();
        let week_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(8 * 24);
        for name in ["gone.json", "gone.json.tmp.123", "kept.json"] {
            fs::write(context_dir.join(name), "{}").unwrap();
        }
        for name in ["gone.json", "gone.json.tmp.123"] {
            fs::File::options()
                .write(true)
                .open(context_dir.join(name))
                .unwrap()
                .set_modified(week_ago)
                .unwrap();
        }
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        capture_context(&dir, &payload(Some("new"), Some(1)), None, now_ms);
        let mut names: Vec<String> = fs::read_dir(&context_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["kept.json", "new.json"]);
    }
}
