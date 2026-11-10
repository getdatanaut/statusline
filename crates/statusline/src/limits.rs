//! Best-effort capture of the status-line payload's `rate_limits`, for the
//! tray to read.
//!
//! Claude Code hands the status line its rate-limit windows on every turn and
//! nothing else on this machine ever sees them: the tray has no way to ask for
//! them, and asking Anthropic for them would mean a Keychain token and a
//! network call, which this workspace deliberately does not do. So the hook
//! that already receives them writes them down.
//!
//! Two rules shape everything below.
//!
//! 1. **The value is stored verbatim.** It is captured as a raw
//!    `serde_json::Value` straight off the payload, *not* routed through
//!    [`sl_core::types::RateLimits`]: a field this workspace does not model
//!    yet has to survive into the file, or the capture is only ever as good as
//!    the last time somebody updated a struct.
//! 2. **Nothing here may affect the status line.** Every error is swallowed.
//!    A full disk, a read-only cache directory or a payload in a shape nobody
//!    expected all mean "no capture this turn" and nothing else -- the line has
//!    already been printed by the time any of this runs.
//!
//! Every open session writes the same file, each with the limits from its own
//! last API response, so an idle session's old reading would otherwise
//! replace a fresh one whenever its line re-renders. A window already in the
//! file therefore survives a write that is older: one that resets earlier, or
//! the same window with less used, as use never falls within a window.
//!
//! For the same reason the capture is dated by when its session last heard
//! from the API, not by when its line rendered: an idle session re-renders
//! with the same old reading every `refreshInterval`, and a fresh date on it
//! would show the tray a frozen percentage as live. Claude Code appends each
//! response to the session's transcript, so the transcript's last write is
//! that time; the date only ever moves forward.

use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::Value;

/// Capture the payload's `rate_limits` into `<cache_dir>/limits.json`.
///
/// `raw` is the untouched stdin payload, re-parsed here rather than read off
/// the already-deserialized [`sl_core::types::StatusPayload`] -- that is the
/// whole point (see rule 1 in the module docs), and a second parse of one
/// small JSON document costs nothing next to the render that just ran.
///
/// A payload with no `rate_limits` (an API-key user, or a session that has not
/// made its first API call yet) leaves any existing capture alone rather than
/// overwriting it with nothing: the tray decides what a capture that has
/// stopped being refreshed is worth, and it needs the timestamp to do that.
pub fn capture_rate_limits(cache_dir: &Path, raw: &str, now_ms: i64) {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    let Some(rate_limits) = payload.get("rate_limits").filter(|value| !value.is_null()) else {
        return;
    };
    let existing = sl_core::paths::read_json::<Value>(&cache_dir.join(sl_core::paths::LIMITS_FILE));
    let merged = match existing
        .as_ref()
        .and_then(|capture| capture.get("rate_limits"))
    {
        Some(old) => keep_newest(rate_limits, old),
        None => rate_limits.clone(),
    };
    let observed = observed_at(&payload, now_ms);
    let captured_at = existing
        .as_ref()
        .and_then(|capture| capture.get("captured_at_ms"))
        .and_then(Value::as_i64)
        .map_or(observed, |previous| previous.clamp(observed, now_ms));
    write_capture(cache_dir, &merged, captured_at);
}

/// When the payload's session last heard from the API: its transcript's last
/// write, no later than now. `now_ms` for a payload that names no
/// transcript it can read, as nothing then says the reading is any older.
fn observed_at(payload: &Value, now_ms: i64) -> i64 {
    payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .and_then(|path| std::fs::metadata(path).ok()?.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .map_or(now_ms, |ms| ms.min(now_ms))
}

/// Two `resets_at` this close are the same window: an ISO form can differ in
/// its fractions of a second from one response to the next.
const SAME_WINDOW_MS: i64 = 60_000;

/// When a window resets and how much of it is used, if it says both.
fn window_rank(window: &Value) -> Option<(i64, f64)> {
    let resets =
        serde_json::from_value::<sl_core::types::ResetsAt>(window.get("resets_at")?.clone())
            .ok()?;
    let used = window
        .get("used_percentage")
        .or_else(|| window.get("utilization"))?
        .as_f64()?;
    Some((sl_core::format::resets_at_ms(&resets)?, used))
}

/// `incoming`, verbatim, but with each window that `existing` holds a newer
/// reading of taken from `existing` instead.
fn keep_newest(incoming: &Value, existing: &Value) -> Value {
    let mut merged = incoming.clone();
    let (Some(out), Some(old)) = (merged.as_object_mut(), existing.as_object()) else {
        return merged;
    };
    for (key, window) in out.iter_mut() {
        let (Some((new_reset, new_used)), Some(kept)) = (window_rank(window), old.get(key)) else {
            continue;
        };
        let Some((old_reset, old_used)) = window_rank(kept) else {
            continue;
        };
        let newer = if (old_reset - new_reset).abs() <= SAME_WINDOW_MS {
            old_used > new_used
        } else {
            old_reset > new_reset
        };
        if newer {
            *window = kept.clone();
        }
    }
    merged
}

/// Write the capture atomically -- the tray reads this file on its own
/// schedule and must never catch a half-written one.
fn write_capture(cache_dir: &Path, rate_limits: &Value, now_ms: i64) {
    // The two key names are the file's whole contract with
    // `sl_core::types::LimitsCapture`; `capture_round_trips_through_the_readers_view`
    // below is what keeps the two ends agreeing.
    let capture = serde_json::json!({ "captured_at_ms": now_ms, "rate_limits": rate_limits });
    sl_core::paths::write_json_atomic(&cache_dir.join(sl_core::paths::LIMITS_FILE), &capture);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_core::types::LimitsCapture;
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

    fn read_file(dir: &Path) -> Option<String> {
        fs::read_to_string(dir.join(sl_core::paths::LIMITS_FILE)).ok()
    }

    /// The shape Claude Code 2.1.258 actually sends: `used_percentage` (the
    /// header's 0-1 utilization times 100) and `resets_at` as epoch seconds.
    const PAYLOAD: &str = r#"{
      "session_id": "abc",
      "rate_limits": {
        "five_hour": {"used_percentage": 12.5, "resets_at": 1787580599},
        "seven_day": {"used_percentage": 15.0, "resets_at": 1787889599}
      }
    }"#;

    #[test]
    fn capture_creates_the_directory_and_writes_valid_json() {
        let (_dir_guard, dir) = temp_dir();
        assert!(!dir.exists(), "the fixture directory must not exist yet");
        capture_rate_limits(&dir, PAYLOAD, 1_788_200_392_112);

        let written: Value =
            serde_json::from_str(&read_file(&dir).expect("limits.json is written"))
                .expect("valid JSON");
        assert_eq!(written["captured_at_ms"], 1_788_200_392_112i64);
        assert_eq!(written["rate_limits"]["five_hour"]["used_percentage"], 12.5);
        assert_eq!(
            written["rate_limits"]["seven_day"]["resets_at"],
            1_787_889_599i64
        );
    }

    #[test]
    fn capture_round_trips_through_the_readers_view() {
        let (_dir_guard, dir) = temp_dir();
        capture_rate_limits(&dir, PAYLOAD, 1_788_200_392_112);

        let capture: LimitsCapture =
            serde_json::from_str(&read_file(&dir).expect("limits.json is written"))
                .expect("reader parses");
        assert_eq!(capture.captured_at_ms, Some(1_788_200_392_112));
        let limits = capture
            .rate_limits
            .expect("rate_limits survives the round trip");
        assert_eq!(
            limits.five_hour.expect("five_hour").used_percentage,
            Some(12.5)
        );
        assert_eq!(
            limits.seven_day.expect("seven_day").used_percentage,
            Some(15.0)
        );
    }

    #[test]
    fn unmodelled_fields_survive_verbatim() {
        // The point of capturing a raw `Value`: `limits[]` is not in any
        // payload this workspace has seen, and a field nobody has thought of
        // yet must still land in the file.
        let payload = r#"{"rate_limits": {"five_hour": {"used_percentage": 1},
            "limits": [{"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}}}],
            "something_invented_next_year": {"nested": [1, 2, 3]}}}"#;
        let (_dir_guard, dir) = temp_dir();
        capture_rate_limits(&dir, payload, 1);

        let written: Value =
            serde_json::from_str(&read_file(&dir).expect("limits.json is written"))
                .expect("valid JSON");
        assert_eq!(
            written["rate_limits"]["limits"][0]["scope"]["model"]["display_name"],
            "Fable"
        );
        assert_eq!(
            written["rate_limits"]["something_invented_next_year"]["nested"][2],
            3
        );
    }

    fn payload(five_used: f64, five_resets: i64, week_used: f64, week_resets: i64) -> String {
        format!(
            r#"{{"rate_limits": {{"five_hour": {{"used_percentage": {five_used}, "resets_at": {five_resets}}}, "seven_day": {{"used_percentage": {week_used}, "resets_at": {week_resets}}}}}}}"#
        )
    }

    fn used(dir: &Path) -> (f64, f64) {
        let written: Value = serde_json::from_str(&read_file(dir).unwrap()).unwrap();
        (
            written["rate_limits"]["five_hour"]["used_percentage"]
                .as_f64()
                .unwrap(),
            written["rate_limits"]["seven_day"]["used_percentage"]
                .as_f64()
                .unwrap(),
        )
    }

    #[test]
    fn an_idle_sessions_older_reading_does_not_replace_a_newer_one() {
        let (_dir_guard, dir) = temp_dir();
        capture_rate_limits(&dir, &payload(63.0, 1_000, 35.0, 9_000), 1);
        // Seen live: a session idle since the window was at 16% re-rendered.
        capture_rate_limits(&dir, &payload(16.0, 1_000, 29.0, 9_000), 2);
        assert_eq!(
            used(&dir),
            (63.0, 35.0),
            "less used in the same window is older"
        );

        capture_rate_limits(&dir, &payload(64.0, 1_000, 34.0, 9_000), 3);
        assert_eq!(used(&dir), (64.0, 35.0), "each window on its own");
    }

    fn captured_at(dir: &Path) -> i64 {
        let written: Value = serde_json::from_str(&read_file(dir).unwrap()).unwrap();
        written["captured_at_ms"].as_i64().unwrap()
    }

    #[test]
    fn a_capture_is_dated_by_its_sessions_last_response_not_its_last_render() {
        let (dir_guard, dir) = temp_dir();
        let transcript = |name: &str, at_ms: i64| {
            let path = dir_guard.path().join(name);
            fs::write(&path, "{}\n").unwrap();
            let at = std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(u64::try_from(at_ms).unwrap());
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(at)
                .unwrap();
            path
        };
        let rendered = |path: &Path| {
            format!(
                r#"{{"transcript_path": {:?}, "rate_limits": {{"five_hour": {{"used_percentage": 16, "resets_at": 1000}}}}}}"#,
                path.to_str().unwrap()
            )
        };
        let minute = 60_000;
        let answered = 1_788_200_000_000;
        let session = transcript("session.jsonl", answered);

        capture_rate_limits(&dir, &rendered(&session), answered + minute);
        assert_eq!(captured_at(&dir), answered, "the response, not the render");
        capture_rate_limits(&dir, &rendered(&session), answered + 10 * minute);
        assert_eq!(
            captured_at(&dir),
            answered,
            "an idle session re-rendering the same reading"
        );
        let older = transcript("older.jsonl", answered - 30 * minute);
        capture_rate_limits(&dir, &rendered(&older), answered + 11 * minute);
        assert_eq!(captured_at(&dir), answered, "never moved back");

        // The session answers again with the same percentage: a new reading.
        let again = transcript("session.jsonl", answered + 12 * minute);
        capture_rate_limits(&dir, &rendered(&again), answered + 12 * minute + 500);
        assert_eq!(captured_at(&dir), answered + 12 * minute);

        let now = answered + 13 * minute;
        capture_rate_limits(&dir, &payload(16.0, 1_000, 1.0, 9_000), now);
        assert_eq!(captured_at(&dir), now, "no transcript to read: now");
    }

    #[test]
    fn a_new_window_replaces_the_old_one_and_an_old_window_does_not() {
        let (_dir_guard, dir) = temp_dir();
        capture_rate_limits(&dir, &payload(90.0, 1_000, 35.0, 9_000), 1);
        capture_rate_limits(&dir, &payload(2.0, 19_000, 35.0, 9_000), 2);
        assert_eq!(
            used(&dir).0,
            2.0,
            "a later reset is a newer window, however little is used"
        );
        capture_rate_limits(&dir, &payload(95.0, 1_000, 35.0, 9_000), 3);
        assert_eq!(
            used(&dir).0,
            2.0,
            "a session still in the last window is stale"
        );
    }

    #[test]
    fn iso_resets_in_the_same_second_are_one_window() {
        let (_dir_guard, dir) = temp_dir();
        let iso = |used: f64, at: &str| {
            format!(
                r#"{{"rate_limits": {{"five_hour": {{"utilization": {used}, "resets_at": "{at}"}}}}}}"#
            )
        };
        capture_rate_limits(&dir, &iso(40.0, "2026-08-24T14:09:59.911461+00:00"), 1);
        capture_rate_limits(&dir, &iso(30.0, "2026-08-24T14:09:59.402000+00:00"), 2);
        let written: Value = serde_json::from_str(&read_file(&dir).unwrap()).unwrap();
        assert_eq!(written["rate_limits"]["five_hour"]["utilization"], 40.0);
    }

    #[test]
    fn a_payload_without_rate_limits_leaves_an_existing_capture_alone() {
        let (_dir_guard, dir) = temp_dir();
        capture_rate_limits(&dir, PAYLOAD, 100);
        for absent in [r#"{"session_id": "abc"}"#, r#"{"rate_limits": null}"#] {
            capture_rate_limits(&dir, absent, 200);
            let written: Value =
                serde_json::from_str(&read_file(&dir).expect("the old capture is still there"))
                    .expect("valid JSON");
            assert_eq!(
                written["captured_at_ms"], 100,
                "{absent} must not overwrite a real capture"
            );
        }
    }

    #[test]
    fn every_failure_is_silent_and_writes_nothing() {
        let (_dir_guard, dir) = temp_dir();
        capture_rate_limits(&dir, "{not json", 1);
        capture_rate_limits(&dir, "", 1);
        capture_rate_limits(&dir, "[]", 1);
        capture_rate_limits(&dir, "42", 1);
        assert!(
            read_file(&dir).is_none(),
            "nothing parseable, so nothing written"
        );

        // An unwritable destination: `limits.json` already exists as a
        // directory, so both the temp write and the rename fail.
        let (_blocked_guard, blocked) = temp_dir();
        fs::create_dir_all(blocked.join(sl_core::paths::LIMITS_FILE)).unwrap();
        capture_rate_limits(&blocked, PAYLOAD, 1);
        assert!(
            read_file(&blocked).is_none(),
            "a blocked write is a no-op, not a panic"
        );
    }
}
