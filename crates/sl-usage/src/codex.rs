//! OpenAI Codex CLI usage: discovery of its rollout files and parsing of their
//! usage lines into the same [`UsageRecord`] Claude's transcripts produce, so
//! one store design and one set of rollups serve both.
//!
//! ## Where it lives
//!
//! `$CODEX_HOME` (else `~/.codex`) holds one rollout per thread,
//! `sessions/YYYY/MM/DD/rollout-<timestamp>-<thread uuid>.jsonl`, moved to
//! `archived_sessions/` when archived. Codex compresses a rollout older than a
//! week in place to `.jsonl.zst`. A thread is found once: the `sessions/` copy
//! wins over an archived one, and a plain file over its compressed twin.
//!
//! ## What is counted
//!
//! Only `token_usage_record` lines, each one model response:
//! `payload.usage.input_tokens` (which *includes* `cached_input_tokens`),
//! `output_tokens` (which includes reasoning), keyed by `payload.thread_id` and
//! `payload.response_id`. `event_msg/token_count` is not counted: its running
//! total resets on compaction and it is sometimes written twice.
//!
//! A record's model is the `turn_context.model` of its turn -- which may be
//! written *after* the record -- else the thread's model from the latest
//! `thread_settings_applied` before it. The service tier comes from that too:
//! `priority` (or `fast`) bills at the model's fast multiplier, see
//! [`PricingMap::fast_multiplier`].
//!
//! ## Re-read on change
//!
//! Because a record's model can arrive later in the file, a rollout is never
//! read from the middle: any change re-reads it whole (see the store). Rollouts
//! are small -- a median of 650 KB, the largest seen 33 MB -- and only a byte
//! prefilter's handful of line kinds is parsed as JSON.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use memchr::memmem;
use serde::Deserialize;

use crate::pricing::{PricingMap, UsageTokens, cost_for_usage};
use crate::record::{Speed, UsageRecord, local_date};

/// `$CODEX_HOME`, else `~/.codex`, when it is a directory.
pub fn codex_home() -> Option<PathBuf> {
    let home = match std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => dirs::home_dir()?.join(".codex"),
    };
    home.is_dir().then_some(home)
}

/// Every rollout under `home`, one per thread, sorted by path. A missing
/// `sessions/` or `archived_sessions/` is simply empty.
pub fn codex_files(home: &Path) -> Vec<PathBuf> {
    codex_files_with(home, &mut crate::paths::Walk::new(is_rollout))
}

/// [`codex_files`], re-reading only the directories that changed since
/// `walk` (one made with [`is_rollout`]) last looked.
pub fn codex_files_with(home: &Path, walk: &mut crate::paths::Walk) -> Vec<PathBuf> {
    let archived = home.join("archived_sessions");
    // Thread -> (rank, path): lower rank wins. Live before archived, plain
    // before compressed; within a rank the first in path order.
    let mut best: HashMap<String, (u8, PathBuf)> = HashMap::new();
    for path in walk.files(&[home.join("sessions"), archived.clone()]) {
        let Some(thread) = thread_of(&path) else { continue };
        let rank = if path.starts_with(&archived) { 2 } else { 0 } + u8::from(is_compressed(&path));
        match best.get(&thread) {
            Some((held, _)) if *held <= rank => {}
            _ => {
                best.insert(thread, (rank, path));
            }
        }
    }
    let mut files: Vec<PathBuf> = best.into_values().map(|(_, path)| path).collect();
    files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
    files
}

/// A Codex rollout, by name.
pub fn is_rollout(path: &Path) -> bool {
    thread_of(path).is_some()
}

fn is_compressed(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".jsonl.zst")
}

/// The thread a rollout belongs to: the UUID ending its file name,
/// `rollout-<timestamp>-<uuid>.jsonl[.zst]`. `None` for anything else.
pub fn thread_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl.zst").or_else(|| name.strip_suffix(".jsonl"))?;
    if !stem.starts_with("rollout-") || stem.len() < 36 {
        return None;
    }
    let uuid = stem.get(stem.len() - 36..)?;
    let shaped = uuid.bytes().enumerate().all(|(i, byte)| if matches!(i, 8 | 13 | 18 | 23) { byte == b'-' } else { byte.is_ascii_hexdigit() });
    shaped.then(|| uuid.to_ascii_lowercase())
}

/// One rollout, parsed.
#[derive(Debug, Default)]
pub struct Rollout {
    /// Usage records with their line's byte offset in the (decompressed) file.
    pub records: Vec<(u64, UsageRecord)>,
    /// The working directory from the rollout's own `session_meta`: its
    /// project. A fork carries its parent's meta too, so the one whose `id` is
    /// the file's thread wins, else the first.
    pub cwd: Option<String>,
}

/// Read and parse a whole rollout, decompressing a `.jsonl.zst`. `None` when
/// it cannot be read.
pub fn read_rollout(path: &Path, pricing: &PricingMap) -> Option<Rollout> {
    let bytes = rollout_bytes(path)?;
    let thread = thread_of(path).unwrap_or_default();
    Some(parse_rollout(&bytes, &thread, pricing))
}

/// A whole rollout's bytes, a `.jsonl.zst` decompressed. `None` when it cannot
/// be read.
pub fn rollout_bytes(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    if is_compressed(path) {
        let mut reader = std::io::BufReader::new(file);
        ruzstd::decoding::StreamingDecoder::new(&mut reader).ok()?.read_to_end(&mut bytes).ok()?;
    } else {
        std::io::BufReader::new(file).read_to_end(&mut bytes).ok()?;
    }
    Some(bytes)
}

// -- Line shapes: only the fields read -----------------------------------------------------------

#[derive(Deserialize)]
struct MetaLine {
    payload: MetaPayload,
}

#[derive(Deserialize)]
struct MetaPayload {
    id: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct TurnContextLine {
    payload: TurnContextPayload,
}

#[derive(Deserialize)]
struct TurnContextPayload {
    turn_id: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct SettingsLine {
    payload: SettingsPayload,
}

#[derive(Deserialize)]
struct SettingsPayload {
    thread_settings: Option<ThreadSettings>,
}

#[derive(Deserialize, Default, Clone)]
struct ThreadSettings {
    model: Option<String>,
    service_tier: Option<String>,
}

#[derive(Deserialize)]
struct UsageLine {
    timestamp: String,
    payload: UsagePayload,
}

#[derive(Deserialize)]
struct UsagePayload {
    thread_id: Option<String>,
    turn_id: Option<String>,
    response_id: Option<String>,
    usage: CodexUsage,
}

#[derive(Deserialize)]
#[expect(clippy::struct_field_names, reason = "named as Codex's own `usage` fields are")]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// What a line is, read off its first bytes: every kind read here names its
/// `type` near the start, before any long payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Meta,
    TurnContext,
    Settings,
    Usage,
}

const HEAD_BYTES: usize = 256;

fn kind_of(line: &[u8]) -> Option<Kind> {
    let head = &line[..line.len().min(HEAD_BYTES)];
    let has = |needle: &[u8]| memmem::find(head, needle).is_some();
    if has(br#""type":"token_usage_record""#) {
        Some(Kind::Usage)
    } else if has(br#""type":"turn_context""#) {
        Some(Kind::TurnContext)
    } else if has(br#""type":"thread_settings_applied""#) {
        Some(Kind::Settings)
    } else if has(br#""type":"session_meta""#) {
        Some(Kind::Meta)
    } else {
        None
    }
}

/// A record before its model is known.
struct Pending {
    offset: u64,
    timestamp_ms: i64,
    thread: String,
    turn: Option<String>,
    response: Option<String>,
    usage: CodexUsage,
    settings: ThreadSettings,
}

/// Parse a whole rollout's bytes. `thread` is the file's own thread, which
/// picks the `session_meta` the project comes from and stands in for a record
/// without a `thread_id`.
pub fn parse_rollout(content: &[u8], thread: &str, pricing: &PricingMap) -> Rollout {
    let mut turn_models: HashMap<String, String> = HashMap::new();
    let mut settings = ThreadSettings::default();
    let mut pending: Vec<Pending> = Vec::new();
    let (mut own_cwd, mut first_cwd): (Option<String>, Option<String>) = (None, None);

    let mut offset = 0usize;
    for line in content.split(|byte| *byte == b'\n') {
        let at = offset as u64;
        offset += line.len() + 1;
        let Some(kind) = kind_of(line) else { continue };
        match kind {
            Kind::Meta => {
                let Ok(meta) = serde_json::from_slice::<MetaLine>(line) else { continue };
                if meta.payload.id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(thread)) {
                    own_cwd = own_cwd.or(meta.payload.cwd.clone());
                }
                first_cwd = first_cwd.or(meta.payload.cwd);
            }
            Kind::TurnContext => {
                let Ok(context) = serde_json::from_slice::<TurnContextLine>(line) else { continue };
                if let (Some(turn), Some(model)) = (context.payload.turn_id, context.payload.model.filter(|model| !model.is_empty())) {
                    turn_models.insert(turn, model);
                }
            }
            Kind::Settings => {
                let Ok(applied) = serde_json::from_slice::<SettingsLine>(line) else { continue };
                if let Some(applied) = applied.payload.thread_settings {
                    settings = ThreadSettings {
                        model: applied.model.filter(|model| !model.is_empty()).or(settings.model),
                        service_tier: applied.service_tier.or(settings.service_tier),
                    };
                }
            }
            Kind::Usage => {
                let Ok(usage) = serde_json::from_slice::<UsageLine>(line) else { continue };
                let Some(timestamp_ms) = sl_core::format::parse_iso8601_to_epoch_ms(&usage.timestamp) else { continue };
                pending.push(Pending {
                    offset: at,
                    timestamp_ms,
                    thread: usage.payload.thread_id.filter(|id| !id.is_empty()).unwrap_or_else(|| thread.to_owned()),
                    turn: usage.payload.turn_id,
                    response: usage.payload.response_id.filter(|id| !id.is_empty()),
                    usage: usage.payload.usage,
                    settings: settings.clone(),
                });
            }
        }
    }

    let cwd = own_cwd.or(first_cwd);
    let project = cwd.clone().unwrap_or_else(|| "unknown".to_owned());
    let records = pending
        .into_iter()
        .map(|record| {
            let model = record.turn.as_ref().and_then(|turn| turn_models.get(turn)).cloned().or(record.settings.model.clone());
            let speed = tier_speed(record.settings.service_tier.as_deref());
            let cached = record.usage.cached_input_tokens.min(record.usage.input_tokens);
            let tokens = UsageTokens {
                input: record.usage.input_tokens - cached,
                output: record.usage.output_tokens,
                cache_create_5m: 0,
                cache_create_1h: 0,
                cache_read: cached,
            };
            let (cost, missing_pricing) = codex_cost(pricing, model.as_deref(), &tokens, speed, record.timestamp_ms);
            (
                record.offset,
                UsageRecord {
                    timestamp_ms: record.timestamp_ms,
                    date: local_date(record.timestamp_ms),
                    session_id: record.thread,
                    project_path: project.clone(),
                    input_tokens: tokens.input,
                    output_tokens: tokens.output,
                    cache_creation_5m_tokens: 0,
                    cache_creation_1h_tokens: 0,
                    cache_read_tokens: tokens.cache_read,
                    model,
                    message_id: record.response,
                    request_id: None,
                    is_sidechain: false,
                    speed,
                    cost,
                    missing_pricing,
                },
            )
        })
        .collect();
    Rollout { records, cwd }
}

/// The tier a thread's settings name, as the [`Speed`] stored with a record:
/// `priority` (and `fast`, its older name) is [`Speed::Fast`]; any other named
/// tier is standard; none named is unknown.
fn tier_speed(tier: Option<&str>) -> Option<Speed> {
    tier.map(|tier| if matches!(tier, "priority" | "fast") { Speed::Fast } else { Speed::Standard })
}

/// A Codex record's cost: its model's price, times the model's fast
/// multiplier on the priority tier. Shared with the store's reprice, so a new
/// price list charges a priority record the same way.
/// `codex-auto-review` is priced as a model it stands for at `timestamp_ms`
/// (see [`PricingMap::priced_model`]); the record keeps its own name.
pub fn codex_cost(pricing: &PricingMap, model: Option<&str>, tokens: &UsageTokens, speed: Option<Speed>, timestamp_ms: i64) -> (f64, bool) {
    let model = model.map(|model| pricing.priced_model(model, timestamp_ms));
    let (cost, missing) = cost_for_usage(pricing, model, tokens);
    match (speed, model) {
        (Some(Speed::Fast), Some(model)) => (cost * pricing.fast_multiplier(model), missing),
        _ => (cost, missing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "01a1c25a-a06b-7471-8a01-1a5dd574e6e4";

    fn usage(ts: &str, turn: &str, response: &str, input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","ordinal":1,"type":"token_usage_record","payload":{{"thread_id":"{THREAD}","turn_id":"{turn}","session_id":"{THREAD}","response_id":"{response}","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":0}}}}}}"#
        )
    }

    fn meta(id: &str, cwd: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-09T22:10:32.945Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}","originator":"codex-tui","source":"cli"}}}}"#)
    }

    fn turn_context(turn: &str, model: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-09T22:10:33.260Z","type":"turn_context","payload":{{"turn_id":"{turn}","cwd":"/x","model":"{model}"}}}}"#)
    }

    fn settings(model: &str, tier: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-11-09T22:10:33.260Z","type":"event_msg","payload":{{"type":"thread_settings_applied","thread_id":"{THREAD}","thread_settings":{{"model":"{model}","service_tier":"{tier}"}}}}}}"#
        )
    }

    fn token_count() -> String {
        r#"{"timestamp":"2026-11-09T22:10:36.025Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":99999,"cached_input_tokens":0,"output_tokens":99999},"last_token_usage":{"input_tokens":99999,"cached_input_tokens":0,"output_tokens":99999}},"rate_limits":null}}"#.to_owned()
    }

    fn parse(lines: &[String]) -> Rollout {
        parse_rollout(lines.join("\n").as_bytes(), THREAD, &PricingMap::new())
    }

    #[test]
    fn a_record_counts_uncached_input_and_cache_reads_apart() {
        let rollout = parse(&[meta(THREAD, "/repo"), turn_context("t1", "gpt-6.1-sol"), usage("2026-11-09T22:10:36.025Z", "t1", "resp_1", 17119, 6912, 11)]);
        let (_, record) = &rollout.records[0];
        assert_eq!((record.input_tokens, record.cache_read_tokens, record.output_tokens), (17119 - 6912, 6912, 11));
        assert_eq!(record.cache_creation_tokens(), 0);
        assert_eq!(record.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(record.session_id, THREAD);
        assert_eq!(record.message_id.as_deref(), Some("resp_1"));
        assert_eq!(record.project_path, "/repo");
        let expected = (10207.0 * 2.0 + 6912.0 * 0.1 + 11.0 * 10.0) / 1e6;
        assert!((record.cost - expected).abs() < 1e-12, "{} vs {expected}", record.cost);
        assert!(!record.missing_pricing);
    }

    #[test]
    fn token_count_events_are_not_counted() {
        let rollout = parse(&[meta(THREAD, "/repo"), token_count(), token_count()]);
        assert!(rollout.records.is_empty());
    }

    #[test]
    fn a_turn_context_written_after_its_records_still_names_their_model() {
        let rollout = parse(&[
            settings("gpt-6-astra", "default"),
            usage("2026-11-09T22:10:36.025Z", "t1", "resp_1", 100, 0, 1),
            turn_context("t1", "gpt-6.1-sol"),
            usage("2026-11-09T22:11:00.000Z", "t2", "resp_2", 100, 0, 1),
        ]);
        let models: Vec<_> = rollout.records.iter().map(|(_, record)| record.model.clone()).collect();
        assert_eq!(models, vec![Some("gpt-6.1-sol".to_owned()), Some("gpt-6-astra".to_owned())], "t2 has no context: the thread's model");
    }

    #[test]
    fn the_priority_tier_bills_at_the_fast_multiplier() {
        let standard = parse(&[settings("gpt-6-astra", "default"), usage("2026-11-09T22:10:36.025Z", "t1", "r", 1000, 0, 10)]);
        let priority = parse(&[settings("gpt-6-astra", "priority"), usage("2026-11-09T22:10:36.025Z", "t1", "r", 1000, 0, 10)]);
        let (standard, priority) = (&standard.records[0].1, &priority.records[0].1);
        assert_eq!(standard.speed, Some(Speed::Standard));
        assert_eq!(priority.speed, Some(Speed::Fast));
        assert!((priority.cost - standard.cost * 2.0).abs() < 1e-12);
    }

    #[test]
    fn a_fork_takes_its_project_from_its_own_meta() {
        let rollout = parse(&[meta("01a0aaaa-0000-7000-8000-000000000000", "/parent"), meta(THREAD, "/child"), usage("2026-11-09T22:10:36.025Z", "t", "r", 1, 0, 1)]);
        assert_eq!(rollout.cwd.as_deref(), Some("/child"));
        assert_eq!(rollout.records[0].1.project_path, "/child");
    }

    #[test]
    fn an_imported_session_without_usage_records_yields_nothing() {
        let imported = r#"{"timestamp":"2026-09-14T22:35:51.914Z","type":"event_msg","payload":{"type":"agent_message","message":"<EXTERNAL SESSION IMPORTED>"}}"#.to_owned();
        assert!(parse(&[meta(THREAD, "/repo"), imported, token_count()]).records.is_empty());
    }

    #[test]
    fn offsets_point_at_each_records_line() {
        let lines = [meta(THREAD, "/repo"), usage("2026-11-09T22:10:36.025Z", "t", "r", 1, 0, 1)];
        let rollout = parse(&lines);
        assert_eq!(rollout.records[0].0, lines[0].len() as u64 + 1);
    }

    #[test]
    fn thread_of_reads_the_uuid_ending_the_file_name() {
        let plain = Path::new("/h/sessions/2026/11/09/rollout-2026-11-09T23-08-01-01a1c25a-a06b-7471-8a01-1a5dd574e6e4.jsonl");
        assert_eq!(thread_of(plain).as_deref(), Some(THREAD));
        assert_eq!(thread_of(&PathBuf::from(format!("{}.zst", plain.display()))).as_deref(), Some(THREAD));
        assert_eq!(thread_of(Path::new("/h/sessions/notes.jsonl")), None);
        assert_eq!(thread_of(Path::new("/h/sessions/rollout-2026-11-09T23-08-01-not-a-uuid-at-all-really-not.jsonl")), None);
    }

    #[test]
    fn discovery_prefers_live_over_archived_and_plain_over_compressed() {
        let home_guard = tempfile::tempdir().unwrap();
        let home = home_guard.path().to_path_buf();
        let day = home.join("sessions/2026/11/09");
        let archived = home.join("archived_sessions");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(&archived).unwrap();
        let name = |uuid: &str| format!("rollout-2026-11-09T23-08-01-{uuid}.jsonl");
        let a = "01a1c25a-a06b-7471-8a01-1a5dd574e6e4";
        let b = "01a1c25a-a0b2-7083-bf71-eb249595094c";
        let c = "01a1c25a-d146-75a0-83dd-b1822c83cdf9";
        for path in [
            day.join(name(a)),
            archived.join(name(a)),
            day.join(name(b)),
            day.join(format!("{}.zst", name(b))),
            archived.join(format!("{}.zst", name(c))),
            day.join("notes.txt"),
        ] {
            std::fs::write(path, b"").unwrap();
        }
        let found = codex_files(&home);
        assert_eq!(found, vec![archived.join(format!("{}.zst", name(c))), day.join(name(a)), day.join(name(b))]);
        assert!(codex_files(&home.join("nowhere")).is_empty(), "a missing home is empty, not an error");
    }

    #[test]
    fn a_compressed_rollout_reads_like_its_plain_twin() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let path = dir.join(format!("rollout-2026-11-09T23-08-01-{THREAD}.jsonl.zst"));
        // A single raw (uncompressed) zstd block: magic, frame header with a
        // single-segment 2-byte content size (stored as size - 256), then a
        // last raw block.
        let content = [meta(THREAD, "/repo"), usage("2026-11-09T22:10:36.025Z", "t", "r", 10, 0, 1)].join("\n").into_bytes();
        assert!((256..65_536).contains(&content.len()));
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x60];
        frame.extend_from_slice(&u16::try_from(content.len() - 256).unwrap().to_le_bytes());
        let header = (u32::try_from(content.len()).unwrap() << 3) | 1; // last block, raw
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(&content);
        std::fs::write(&path, frame).unwrap();
        let rollout = read_rollout(&path, &PricingMap::new()).expect("decodes");
        assert_eq!(rollout.records.len(), 1);
        assert_eq!(rollout.cwd.as_deref(), Some("/repo"));
    }
}
