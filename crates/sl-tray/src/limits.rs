//! Reading the status-line hook's rate-limit capture.
//!
//! `statusline::limits` writes `limits.json` on every Claude Code turn; this
//! reads it. That is the entire mechanism -- there is no Keychain access and no
//! network call anywhere in this workspace, so a window Claude Code has not
//! told the status line about is a window the tray does not know either.
//!
//! Same silent-skip discipline as the rest of the crate: a missing file (the
//! tray running before Claude Code ever has), a truncated one, a file some
//! other tool put there -- all read as `None`. The one thing that is *not*
//! silent is the age of a capture that did parse, which the tray shows and the
//! title marks, because a frozen percentage presented as live is the failure
//! mode that actually matters here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sl_core::types::LimitsCapture;

use crate::snapshot::{LimitWindow, LimitsSnapshot};

/// The capture's windows, re-read only when its file changes: a `stat()` per
/// paint. The usage refresh reads the capture too, but only as often as it
/// runs -- every 5 minutes at its slowest, past the 2 minutes after which a
/// capture is shown as stale -- so between refreshes the paint asks this.
pub struct CaptureWatch {
    path: PathBuf,
    /// The file's modification time and length when last read.
    seen: Option<(std::time::SystemTime, u64)>,
    folded: Option<LimitsSnapshot>,
    /// Reads, for the tests: a file that has not changed is not read again.
    reads: usize,
}

impl CaptureWatch {
    pub fn new(path: PathBuf) -> Self {
        Self { path, seen: None, folded: None, reads: 0 }
    }

    /// The latest capture's windows; `None` while there is no usable capture.
    pub fn current(&mut self) -> Option<LimitsSnapshot> {
        let stamp = std::fs::metadata(&self.path).ok().and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if stamp != self.seen {
            self.seen = stamp;
            self.reads += 1;
            self.folded = stamp.and_then(|_| crate::snapshot::fold_rate_limits(read_capture(&self.path).as_ref()));
        }
        self.folded
    }

    /// `snapshot`'s windows, or the watched ones if those were captured later.
    pub fn fresher(&mut self, snapshot: Option<LimitsSnapshot>) -> Option<LimitsSnapshot> {
        match (snapshot, self.current()) {
            (Some(held), Some(watched)) if watched.captured_at_ms <= held.captured_at_ms => Some(held),
            (held, watched) => watched.or(held),
        }
    }
}

/// The capture at [`sl_core::paths::limits_path`], or `None`.
///
/// Read fresh on every refresh rather than cached: the hook rewrites the file
/// each turn, and a capture read once at launch would be exactly the frozen
/// number this module exists to avoid presenting.
pub fn load_capture() -> Option<LimitsCapture> {
    read_capture(&sl_core::paths::limits_path())
}

/// [`load_capture`] against an explicit path, which is what the tests use.
pub fn read_capture(path: &Path) -> Option<LimitsCapture> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

// --------------------------------------------------------------------------
// Codex
// --------------------------------------------------------------------------

/// Codex's rate limits, as last seen in its rollouts.
///
/// Codex reports them only inside its transcripts: every
/// `event_msg/token_count` carries `rate_limits` with a `primary` and maybe a
/// `secondary` window (`used_percent`, `window_minutes`, `resets_at` in epoch
/// seconds). There is no file like Claude's `limits.json`, and asking the
/// service would mean auth and the network, which this tray never does. So the
/// numbers are only ever as fresh as the last Codex turn, and are labelled
/// "last seen".
///
/// Each rollout's newest reading is kept against the file's size and mtime, so
/// a refresh re-reads only rollouts that changed; the newest reading across all
/// of them, by its own timestamp, wins -- not the newest file, since a
/// subagent's rollout can be written after its parent's last turn.
#[derive(Debug, Default)]
pub struct CodexLimits {
    files: HashMap<PathBuf, (u64, i64, Option<CodexReading>)>,
}

/// One `token_count.rate_limits` reading: when, and its windows.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CodexReading {
    at_ms: i64,
    windows: [Option<CodexWindow>; 2],
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CodexWindow {
    used_percent: f64,
    window_minutes: Option<u32>,
    resets_at_ms: Option<i64>,
}

/// How much of a rollout's end is read first; the whole file only when its
/// last reading is further back than this.
const TAIL_BYTES: u64 = 256 * 1024;

/// A window this long or shorter is shown as the session window; longer, as
/// the weekly one. Codex has only ever sent the 10080-minute window here.
const SHORT_WINDOW_MINUTES: u32 = 24 * 60;

impl CodexLimits {
    /// The newest reading across `files`, folded for the meters.
    pub fn load(&mut self, files: &[PathBuf]) -> Option<LimitsSnapshot> {
        let mut stated: Vec<(&PathBuf, u64, i64)> = files.iter().filter_map(|path| stat(path).map(|(size, mtime)| (path, size, mtime))).collect();
        stated.sort_by_key(|stated| std::cmp::Reverse(stated.2));
        self.files.retain(|path, _| files.contains(path));

        let mut best: Option<CodexReading> = None;
        for (path, size, mtime) in stated {
            // A line is written at its own time, so nothing in a file last
            // written before the best reading can beat it.
            if best.is_some_and(|best| mtime < best.at_ms) {
                break;
            }
            let reading = match self.files.get(path) {
                Some((held_size, held_mtime, reading)) if *held_size == size && *held_mtime == mtime => *reading,
                _ => {
                    let reading = read_reading(path, size);
                    self.files.insert(path.clone(), (size, mtime, reading));
                    reading
                }
            };
            if let Some(reading) = reading
                && best.is_none_or(|best| reading.at_ms > best.at_ms)
            {
                best = Some(reading);
            }
        }
        best.map(fold_reading)
    }
}

fn stat(path: &Path) -> Option<(u64, i64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = i64::try_from(metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()).unwrap_or(i64::MAX);
    Some((metadata.len(), mtime))
}

/// The newest reading in a rollout: its tail first, the whole file when the
/// tail has none. A compressed rollout is decoded whole.
fn read_reading(path: &Path, size: u64) -> Option<CodexReading> {
    use std::io::{Read, Seek, SeekFrom};
    if path.to_string_lossy().ends_with(".zst") {
        return newest_reading(&sl_usage::codex::rollout_bytes(path)?);
    }
    let mut file = std::fs::File::open(path).ok()?;
    if size > TAIL_BYTES {
        file.seek(SeekFrom::Start(size - TAIL_BYTES)).ok()?;
        let mut tail = Vec::new();
        file.by_ref().take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
        if let Some(reading) = newest_reading(&tail) {
            return Some(reading);
        }
        file.seek(SeekFrom::Start(0)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(size).read_to_end(&mut bytes).ok()?;
    newest_reading(&bytes)
}

#[derive(Deserialize)]
struct TokenCountLine {
    timestamp: String,
    payload: TokenCountPayload,
}

#[derive(Deserialize)]
struct TokenCountPayload {
    rate_limits: Option<RawLimits>,
}

#[derive(Deserialize)]
struct RawLimits {
    primary: Option<RawWindow>,
    secondary: Option<RawWindow>,
}

#[derive(Deserialize)]
struct RawWindow {
    used_percent: Option<f64>,
    window_minutes: Option<u32>,
    resets_at: Option<f64>,
}

/// The last line of `bytes` that is a `token_count` with a non-null
/// `rate_limits`. A first line cut by the tail read fails to parse and is
/// passed over.
fn newest_reading(bytes: &[u8]) -> Option<CodexReading> {
    bytes.split(|byte| *byte == b'\n').rev().find_map(|line| {
        let head = &line[..line.len().min(200)];
        if !contains(head, br#""type":"token_count""#) || !contains(line, br#""rate_limits":{"#) {
            return None;
        }
        let parsed: TokenCountLine = serde_json::from_slice(line).ok()?;
        let at_ms = sl_core::format::parse_iso8601_to_epoch_ms(&parsed.timestamp)?;
        let limits = parsed.payload.rate_limits?;
        let window = |raw: Option<RawWindow>| {
            let raw = raw?;
            Some(CodexWindow { used_percent: raw.used_percent?, window_minutes: raw.window_minutes, resets_at_ms: raw.resets_at.map(|secs| sl_core::num::trunc_i64(secs * 1000.0)) })
        };
        let windows = [window(limits.primary), window(limits.secondary)];
        windows.iter().any(Option::is_some).then_some(CodexReading { at_ms, windows })
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// A reading as the meters take it: the shortest window as the session one
/// when it is a day or less, the shortest longer one as the weekly. A window
/// of a length never seen before is kept, in whichever slot fits.
fn fold_reading(reading: CodexReading) -> LimitsSnapshot {
    let mut windows: Vec<CodexWindow> = reading.windows.into_iter().flatten().collect();
    windows.sort_by_key(|window| window.window_minutes.unwrap_or(u32::MAX));
    let as_limit = |window: &CodexWindow| LimitWindow { used_percent: window.used_percent, resets_at_ms: window.resets_at_ms, window_minutes: window.window_minutes };
    let short = |window: &&CodexWindow| window.window_minutes.is_some_and(|minutes| minutes <= SHORT_WINDOW_MINUTES);
    LimitsSnapshot {
        captured_at_ms: reading.at_ms,
        five_hour: windows.iter().find(short).map(as_limit),
        seven_day: windows.iter().find(|window| !short(window)).map(as_limit),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_watch_reads_the_capture_again_only_when_it_changes() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let path = dir.join("limits.json");
        let capture = |at: i64, pct: u32| format!(r#"{{"captured_at_ms": {at}, "rate_limits": {{"five_hour": {{"used_percentage": {pct}, "resets_at": 1788210000}}}}}}"#);
        let mut watch = CaptureWatch::new(path.clone());
        assert_eq!(watch.current(), None, "no file yet");
        std::fs::write(&path, capture(1_000, 10)).unwrap();
        assert_eq!(watch.current().map(|l| l.captured_at_ms), Some(1_000));
        assert_eq!(watch.reads, 1, "read once it appeared");
        assert_eq!(watch.current().map(|l| l.captured_at_ms), Some(1_000));
        assert_eq!(watch.reads, 1, "unchanged: not read again");
        // A rewrite of another length (as the hook's next turn writes).
        std::fs::write(&path, capture(2_000_000, 25)).unwrap();
        assert_eq!(watch.current().map(|l| l.captured_at_ms), Some(2_000_000));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(watch.current(), None, "gone: nothing to show");
    }

    #[test]
    fn the_later_capture_wins_whichever_holds_it() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let path = dir.join("limits.json");
        let held = |at: i64| Some(LimitsSnapshot { captured_at_ms: at, ..LimitsSnapshot::default() });
        let mut watch = CaptureWatch::new(path.clone());
        assert_eq!(watch.fresher(held(5)), held(5), "no file: the snapshot's");
        assert_eq!(watch.fresher(None), None);
        std::fs::write(&path, r#"{"captured_at_ms": 7, "rate_limits": {"seven_day": {"used_percentage": 3}}}"#).unwrap();
        assert_eq!(watch.fresher(held(5)).map(|l| l.captured_at_ms), Some(7), "the file is newer");
        assert_eq!(watch.fresher(held(9)), held(9), "the snapshot is newer: never go back");
        assert_eq!(watch.fresher(held(7)), held(7), "the same capture: the snapshot's");
        assert_eq!(watch.fresher(None).map(|l| l.captured_at_ms), Some(7));
    }

    use super::*;
    use std::path::PathBuf;

    /// A `limits.json` holding `contents` (none: no file), in a directory
    /// removed when the guard is dropped.
    fn temp_file(contents: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let path = guard.path().join("limits.json");
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        (guard, path)
    }

    #[test]
    fn a_missing_file_is_none_not_an_error() {
        assert!(read_capture(&temp_file(None).1).is_none());
    }

    #[test]
    fn malformed_json_is_none() {
        // Half a write, which is what a reader that ignored the hook's
        // temp-file-and-rename would see.
        assert!(read_capture(&temp_file(Some(r#"{"captured_at_ms": 1788200392112, "rate_li"#)).1).is_none());
        assert!(read_capture(&temp_file(Some("")).1).is_none());
    }

    #[test]
    fn a_valid_capture_parses() {
        let (_guard, path) = temp_file(Some(
            r#"{"captured_at_ms": 1788200392112,
                "rate_limits": {"five_hour": {"used_percentage": 12.5, "resets_at": 1787580599}}}"#,
        ));
        let capture = read_capture(&path).expect("a valid capture parses");
        assert_eq!(capture.captured_at_ms, Some(1_788_200_392_112));
        assert_eq!(capture.rate_limits.expect("rate_limits").five_hour.expect("five_hour").used_percentage, Some(12.5));
    }

    #[test]
    fn a_file_with_no_rate_limits_key_still_parses() {
        // Not a case the hook writes (it skips a payload with no
        // `rate_limits`), but the file is on disk where anything can edit it,
        // and "parsed, and says nothing" is a different state from "did not
        // parse" -- only the first one has a usable timestamp.
        let capture = read_capture(&temp_file(Some(r#"{"captured_at_ms": 42}"#)).1).expect("parses");
        assert_eq!(capture.captured_at_ms, Some(42));
        assert!(capture.rate_limits.is_none());
    }

    fn token_count(ts: &str, used: f64, minutes: u32) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":{minutes},"resets_at":1794418728}},"secondary":null}}}}}}"#
        )
    }

    fn rollout(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn codex_limits_are_the_newest_reading_by_its_own_timestamp() {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path().to_path_buf();
        let null = r#"{"timestamp":"2026-11-09T23:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":null}}"#.to_owned();
        let parent = rollout(&dir, "parent.jsonl", &[token_count("2026-11-09T22:00:00.000Z", 30.0, 10_080), null]);
        // Written after the parent, but its reading is older.
        let child = rollout(&dir, "child.jsonl", &[token_count("2026-11-09T21:00:00.000Z", 20.0, 10_080)]);
        let mut limits = CodexLimits::default();
        let seen = limits.load(&[parent.clone(), child.clone()]).expect("a reading");
        assert_eq!(seen.seven_day.map(|w| w.used_percent), Some(30.0), "the null reading after it is passed over");
        assert_eq!(seen.five_hour, None);
        assert_eq!(seen.seven_day.and_then(|w| w.window_minutes), Some(10_080));
        assert_eq!(seen.captured_at_ms, sl_core::format::parse_iso8601_to_epoch_ms("2026-11-09T22:00:00.000Z").unwrap());

        // A newer turn in the child, and a short window: re-read, and it wins.
        std::fs::write(&child, token_count("2026-11-09T23:30:00.000Z", 5.0, 300) + "\n").unwrap();
        let seen = limits.load(&[parent, child]).expect("a reading");
        assert_eq!(seen.five_hour.map(|w| w.used_percent), Some(5.0), "a 5-hour window is the session one");
        assert_eq!(seen.seven_day, None);
    }

    #[test]
    fn no_rollout_or_no_reading_is_no_codex_limits() {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path().to_path_buf();
        let quiet = rollout(&dir, "quiet.jsonl", &[r#"{"timestamp":"2026-11-09T23:00:00.000Z","type":"session_meta","payload":{}}"#.to_owned()]);
        assert_eq!(CodexLimits::default().load(&[quiet]), None);
        assert_eq!(CodexLimits::default().load(&[]), None);
    }
}
