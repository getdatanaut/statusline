//! When a session last talked to the API, and how long its prompt cache lives.
//!
//! Anthropic's prompt cache expires a fixed time after its last use, so an
//! idle session's next message is cheap only while that clock has not run out.
//! Both facts are in the session's transcript: every assistant record carries
//! a `timestamp`, and its `usage.cache_creation` says which TTL the cache was
//! written with (`ephemeral_5m_input_tokens` or `ephemeral_1h_input_tokens`).
//!
//! Compaction resets that clock. It writes a `compact_boundary` system record
//! and no assistant record, and the next turn builds a fresh cache over the
//! compacted context, so a session idle since compacting has no cache to lose.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// How much of the end of a transcript is read: enough for the latest few
/// turns, which is all this looks at.
const TAIL_BYTES: u64 = 512 * 1024;
const FIVE_MIN_MS: i64 = 5 * 60_000;
const ONE_HOUR_MS: i64 = 60 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheInfo {
    /// Epoch ms of the latest main-thread assistant message.
    pub last_activity_ms: i64,
    /// The cache lifetime in effect, when the tail shows one.
    pub ttl_ms: Option<i64>,
    /// Compacted since its last turn: `last_activity_ms` is the compaction,
    /// and there is no TTL, as the old cache no longer matters.
    pub compacted: bool,
}

impl CacheInfo {
    /// Milliseconds until the cache expires; negative once it has.
    pub fn remaining_ms(&self, now_ms: i64) -> Option<i64> {
        self.ttl_ms.map(|ttl| self.last_activity_ms + ttl - now_ms)
    }
}

/// The transcript of `session_id`: `<config>/projects/<project>/<id>.jsonl`.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    for base in sl_usage::paths::claude_paths().ok()? {
        let Ok(projects) = std::fs::read_dir(base.join("projects")) else {
            continue;
        };
        for project in projects.flatten() {
            let candidate = project.path().join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Remembers where each session's transcript is and what its tail said last
/// time, so a poll costs one `stat` per open session unless the file changed.
#[derive(Default)]
pub struct CacheReader {
    paths: HashMap<String, PathBuf>,
    /// Each transcript's title, read the same incremental way.
    titles: crate::titles::TitleReader,
    /// By path: the `(length, modified)` the cached result was read at.
    tails: HashMap<PathBuf, ((u64, SystemTime), Option<CacheInfo>)>,
    /// How many tails were actually read and parsed; for tests.
    #[cfg(test)]
    parses: usize,
}

impl CacheReader {
    /// The cache state of `session_id`'s transcript.
    pub fn read(&mut self, session_id: &str) -> Option<CacheInfo> {
        let path = self.path(session_id)?;
        self.info(&path)
    }

    /// The title Claude Code gave `session_id`, if it has given one yet.
    pub fn title(&mut self, session_id: &str) -> Option<String> {
        let path = self.path(session_id)?;
        self.titles.read(&path)
    }

    fn path(&mut self, session_id: &str) -> Option<PathBuf> {
        // A remembered path is only trusted while the file is still there.
        if let Some(path) = self.paths.get(session_id).filter(|path| path.is_file()) {
            return Some(path.clone());
        }
        let path = find_transcript(session_id)?;
        self.paths.insert(session_id.to_owned(), path.clone());
        Some(path)
    }

    fn info(&mut self, path: &Path) -> Option<CacheInfo> {
        let meta = std::fs::metadata(path).ok()?;
        let stamp = (meta.len(), meta.modified().ok()?);
        if let Some((seen, info)) = self.tails.get(path)
            && *seen == stamp
        {
            return *info;
        }
        #[cfg(test)]
        {
            self.parses += 1;
        }
        let info = read_tail(path);
        self.tails.insert(path.to_owned(), (stamp, info));
        info
    }

    /// Forget sessions that are no longer open.
    pub fn retain(&mut self, open: &[&str]) {
        self.paths.retain(|id, _| open.contains(&id.as_str()));
        let keep: Vec<&PathBuf> = self.paths.values().collect();
        self.tails.retain(|path, _| keep.contains(&path));
        self.titles
            .retain(&keep.iter().map(|path| path.as_path()).collect::<Vec<_>>());
    }
}

fn read_tail(path: &Path) -> Option<CacheInfo> {
    parse_tail(&tail_text(path)?)
}

/// The last [`TAIL_BYTES`] of a transcript, from its first whole line.
pub(crate) fn tail_text(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // Starting mid-file, the first line is a fragment.
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    Some(text.to_owned())
}

/// The epoch ms of a record's `timestamp`.
fn timestamp_ms(record: &Value) -> Option<i64> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(sl_core::format::parse_iso8601_to_epoch_ms)
}

/// Scan transcript lines newest-first for the last activity and the TTL, back
/// to the latest compaction at most.
pub fn parse_tail(text: &str) -> Option<CacheInfo> {
    let mut last_activity = None;
    let mut ttl = None;
    for line in text.lines().rev() {
        // Cheap prefilter: skip the (large) lines that are neither assistant
        // turns nor compactions.
        let boundary = line.contains("compact_boundary");
        if !boundary && !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // A subagent's cache is its own; it says nothing about this session's.
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // The text also turns up inside messages: only the record itself counts.
        if boundary
            && record.get("type").and_then(Value::as_str) == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            match last_activity {
                // Nothing since compacting: no cache to keep warm.
                None => {
                    return Some(CacheInfo {
                        last_activity_ms: timestamp_ms(&record)?,
                        ttl_ms: None,
                        compacted: true,
                    });
                }
                // A turn since: its cache is new, and older TTLs say nothing.
                Some(_) => break,
            }
        }
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Claude Code's own stand-in for a reply (an API error, a rate limit,
        // "No response requested."): no request used the cache.
        if record.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>") {
            continue;
        }
        let Some(usage) = record.pointer("/message/usage") else {
            continue;
        };
        if last_activity.is_none() {
            last_activity = timestamp_ms(&record);
        }
        if ttl.is_none() {
            let created = |key: &str| {
                usage
                    .pointer(&format!("/cache_creation/{key}"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            if created("ephemeral_1h_input_tokens") > 0 {
                ttl = Some(ONE_HOUR_MS);
            } else if created("ephemeral_5m_input_tokens") > 0 {
                ttl = Some(FIVE_MIN_MS);
            }
        }
        if last_activity.is_some() && ttl.is_some() {
            break;
        }
    }
    Some(CacheInfo {
        last_activity_ms: last_activity?,
        ttl_ms: ttl,
        compacted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(ts: &str, one_hour: u64, five_min: u64, sidechain: bool) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":{sidechain},"timestamp":"{ts}","message":{{"usage":{{"cache_creation":{{"ephemeral_1h_input_tokens":{one_hour},"ephemeral_5m_input_tokens":{five_min}}}}}}}}}"#
        )
    }

    #[test]
    fn the_newest_message_gives_the_time_and_the_newest_creation_gives_the_ttl() {
        let text = [
            assistant("2026-10-28T10:00:00.000Z", 100, 0, false),
            r#"{"type":"user","timestamp":"2026-10-28T10:30:00.000Z"}"#.to_owned(),
            assistant("2026-10-28T10:40:00.000Z", 0, 0, false), // pure cache read: no TTL info
        ]
        .join("\n");
        let info = parse_tail(&text).unwrap();
        assert_eq!(info.last_activity_ms, 1_793_184_000_000); // 2026-10-28T10:40:00Z
        assert_eq!(
            info.ttl_ms,
            Some(ONE_HOUR_MS),
            "the TTL carries over from the last creation"
        );
        assert_eq!(
            info.remaining_ms(info.last_activity_ms + 50 * 60_000),
            Some(10 * 60_000)
        );
        assert_eq!(
            info.remaining_ms(info.last_activity_ms + 61 * 60_000),
            Some(-60_000)
        );
    }

    fn boundary(ts: &str) -> String {
        format!(
            r#"{{"type":"system","subtype":"compact_boundary","isSidechain":false,"content":"Conversation compacted","timestamp":"{ts}"}}"#
        )
    }

    #[test]
    fn a_compaction_with_no_turn_since_has_no_cache_to_lose() {
        let text = [
            assistant("2026-10-28T10:00:00.000Z", 100, 0, false),
            boundary("2026-10-28T10:20:00.000Z"),
            r#"{"type":"user","isCompactSummary":true,"timestamp":"2026-10-28T10:20:01.000Z"}"#
                .to_owned(),
        ]
        .join("\n");
        let info = parse_tail(&text).unwrap();
        assert!(info.compacted);
        assert_eq!(
            info.ttl_ms, None,
            "the pre-compaction TTL no longer applies"
        );
        assert_eq!(
            info.last_activity_ms, 1_793_182_800_000,
            "timed from the compaction"
        ); // 10:20Z
        assert_eq!(
            info.remaining_ms(info.last_activity_ms + 2 * ONE_HOUR_MS),
            None,
            "never expires"
        );
    }

    #[test]
    fn a_turn_after_a_compaction_is_tracked_as_usual() {
        let text = [
            assistant("2026-10-28T09:00:00.000Z", 100, 0, false),
            boundary("2026-10-28T10:00:00.000Z"),
            assistant("2026-10-28T10:05:00.000Z", 0, 50, false),
        ]
        .join("\n");
        let info = parse_tail(&text).unwrap();
        assert!(!info.compacted);
        assert_eq!(
            info.ttl_ms,
            Some(FIVE_MIN_MS),
            "the new cache's TTL, not the old one's"
        );

        // A pure cache read after compacting: the old TTL must not leak through.
        let text = [
            assistant("2026-10-28T09:00:00.000Z", 100, 0, false),
            boundary("2026-10-28T10:00:00.000Z"),
            assistant("2026-10-28T10:05:00.000Z", 0, 0, false),
        ]
        .join("\n");
        assert_eq!(parse_tail(&text).unwrap().ttl_ms, None);
    }

    #[test]
    fn the_words_compact_boundary_in_a_message_are_not_a_compaction() {
        let text = [
            assistant("2026-10-28T10:00:00.000Z", 100, 0, false),
            r#"{"type":"user","timestamp":"2026-10-28T10:10:00.000Z","message":{"content":"what is a \"subtype\":\"compact_boundary\" record?"}}"#.to_owned(),
        ]
        .join("\n");
        let info = parse_tail(&text).unwrap();
        assert!(!info.compacted);
        assert_eq!(info.ttl_ms, Some(ONE_HOUR_MS));
    }

    #[test]
    fn a_subagents_compaction_is_not_the_sessions() {
        let side = r#"{"type":"system","subtype":"compact_boundary","isSidechain":true,"timestamp":"2026-10-28T10:20:00.000Z"}"#;
        let text = [
            assistant("2026-10-28T10:00:00.000Z", 100, 0, false),
            side.to_owned(),
        ]
        .join("\n");
        assert!(!parse_tail(&text).unwrap().compacted);
    }

    #[test]
    fn an_unchanged_transcript_is_not_read_again_and_a_grown_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, assistant("2026-10-28T10:00:00Z", 5, 0, false) + "\n").unwrap();
        let mut reader = CacheReader::default();
        let first = reader.info(&path).unwrap();
        assert_eq!(reader.info(&path), Some(first));
        assert_eq!(reader.info(&path), Some(first));
        assert_eq!(
            reader.parses, 1,
            "two further polls of an unchanged file cost no parse"
        );
        let mut grown = std::fs::read_to_string(&path).unwrap();
        grown.push_str(&(assistant("2026-10-28T10:10:00Z", 0, 0, false) + "\n"));
        std::fs::write(&path, grown).unwrap();
        let second = reader.info(&path).unwrap();
        assert_eq!(reader.parses, 2);
        assert_eq!(
            second.last_activity_ms,
            first.last_activity_ms + 10 * 60_000
        );
    }

    #[test]
    fn closed_sessions_are_forgotten() {
        let mut reader = CacheReader::default();
        reader.paths.insert("a".into(), PathBuf::from("/x/a.jsonl"));
        reader.paths.insert("b".into(), PathBuf::from("/x/b.jsonl"));
        reader.tails.insert(
            PathBuf::from("/x/a.jsonl"),
            ((0, SystemTime::UNIX_EPOCH), None),
        );
        reader.tails.insert(
            PathBuf::from("/x/b.jsonl"),
            ((0, SystemTime::UNIX_EPOCH), None),
        );
        reader.retain(&["a"]);
        assert_eq!(reader.paths.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(reader.tails.len(), 1);
    }

    #[test]
    fn an_api_error_after_the_last_turn_does_not_restart_the_cache_clock() {
        // As Claude Code writes a failed request: zero usage, a stand-in model.
        let error = r#"{"type":"assistant","isSidechain":false,"isApiErrorMessage":true,"timestamp":"2026-10-28T10:04:00.000Z","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0}},"content":[{"type":"text","text":"API Error: 529 Overloaded."}]}}"#;
        let text = [
            assistant("2026-10-28T10:00:00.000Z", 0, 50, false),
            error.to_owned(),
        ]
        .join("\n");
        let info = parse_tail(&text).unwrap();
        assert_eq!(
            info.last_activity_ms, 1_793_181_600_000,
            "timed from 10:00, the last real turn"
        );
        assert_eq!(info.ttl_ms, Some(FIVE_MIN_MS));
    }

    #[test]
    fn five_minute_caches_and_unknown_ttls_and_subagents() {
        let five = parse_tail(&assistant("2026-10-28T10:00:00Z", 0, 50, false)).unwrap();
        assert_eq!(five.ttl_ms, Some(FIVE_MIN_MS));
        let unknown = parse_tail(&assistant("2026-10-28T10:00:00Z", 0, 0, false)).unwrap();
        assert_eq!((unknown.ttl_ms, unknown.remaining_ms(0)), (None, None));
        // A subagent's later message does not count as this session's activity.
        let text = [
            assistant("2026-10-28T10:00:00Z", 5, 0, false),
            assistant("2026-10-28T11:00:00Z", 5, 0, true),
        ]
        .join("\n");
        assert_eq!(
            parse_tail(&text).unwrap().last_activity_ms,
            1_793_181_600_000
        );
        assert_eq!(parse_tail("not json\n{}"), None);
    }
}
