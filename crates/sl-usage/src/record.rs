//! Deserialization of a single raw usage line, the line-level gates that
//! decide whether a line is worth parsing/keeping, and single-file parsing.
//!
//! Ported from <https://github.com/ccusage/ccusage/blob/8841f921/rust/crates/ccusage-core/src/types.rs>
//! (`UsageEntry`/`UsageMessage`/`TokenUsageRaw`/`CacheCreationRaw`/`Speed`) and
//! <https://github.com/ccusage/ccusage/blob/8841f921/rust/adapters/claude/src/lib.rs> (`read_usage_file` and its
//! gates), minus the advisor/iterations and cost-mode/pricing plumbing (out of
//! scope for this crate).

use std::path::Path;

use memchr::{memchr, memmem};
use serde::{Deserialize, Serialize};

use crate::paths::extract_session_parts;
use crate::pricing::{PricingMap, UsageTokens, cost_for_usage};

/// Raw per-line JSONL record, as written by Claude Code.
///
/// Only `timestamp`, `message`, `message.usage`, `message.usage.input_tokens`
/// and `message.usage.output_tokens` are required for a line to deserialize at
/// all -- anything else missing is fine (`Option`). A line missing a required
/// field fails `serde_json::from_slice` and is skipped, not treated as an
/// error.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEntry {
    pub session_id: Option<String>,
    pub timestamp: String,
    pub version: Option<String>,
    pub message: UsageMessage,
    #[serde(rename = "costUSD")]
    pub cost_usd: Option<f64>,
    pub request_id: Option<String>,
    pub is_api_error_message: Option<bool>,
    pub is_sidechain: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageMessage {
    pub usage: TokenUsageRaw,
    pub model: Option<String>,
    pub id: Option<String>,
}

/// Token counts stay snake_case as declared in the JSONL, unlike the outer
/// `UsageEntry`'s camelCase.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct TokenUsageRaw {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub speed: Option<Speed>,
    #[serde(default)]
    pub cache_creation: Option<CacheCreationRaw>,
}

impl TokenUsageRaw {
    /// The `cache_creation` breakdown (5m + 1h ephemeral fields) overrides the
    /// flat `cache_creation_input_tokens` entirely when present -- it is not
    /// summed with it. When absent, the flat field is treated as all-5m (the
    /// 1h cache-creation tier billed at `input * 2.0` didn't exist before
    /// this breakdown shape did).
    pub fn cache_creation_split(&self) -> (u64, u64) {
        if let Some(breakdown) = &self.cache_creation {
            (breakdown.ephemeral_5m_input_tokens, breakdown.ephemeral_1h_input_tokens)
        } else {
            (self.cache_creation_input_tokens, 0)
        }
    }

    /// Sum of the 5m + 1h cache-creation split.
    pub fn cache_creation_token_count(&self) -> u64 {
        let (five_minute, one_hour) = self.cache_creation_split();
        five_minute.saturating_add(one_hour)
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct CacheCreationRaw {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    Standard,
    Fast,
}

/// A single deduped, parsed usage record: token counts plus enough identity
/// and grouping metadata for a later cache/aggregation layer. No cost/pricing
/// yet (that's a later task).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct UsageRecord {
    /// Epoch milliseconds (UTC), parsed from the JSONL `timestamp` field with
    /// full sub-second precision.
    pub timestamp_ms: i64,
    /// Local calendar date (`YYYY-MM-DD`) the entry buckets into, computed
    /// once here from `timestamp_ms` in the system's local timezone.
    pub date: String,
    /// Effective session id: the JSONL `sessionId` if present, else the
    /// path-derived fallback from [`extract_session_parts`]. This is the
    /// dedupe key's session component, and it is *not* necessarily the session
    /// group a record's file belongs to -- see [`crate::cache`].
    pub session_id: String,
    /// Path-derived project path/name (see [`extract_session_parts`]).
    /// Consumed by [`crate::aggregate::session_summaries`].
    pub project_path: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 5-minute and 1-hour cache-creation token counts, kept split (rather
    /// than merged into one total) because the pricing formula bills them at
    /// different rates -- see [`crate::pricing::calculate_cost`]. Resolved
    /// via [`TokenUsageRaw::cache_creation_split`].
    pub cache_creation_5m_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub cache_read_tokens: u64,
    /// Effective model: `None` when `message.model == "<synthetic>"`,
    /// otherwise the raw model with a `-fast` suffix appended when
    /// `usage.speed == Speed::Fast`.
    pub model: Option<String>,
    pub message_id: Option<String>,
    /// Part of the exact dedupe key, so it has to survive into the cache: the
    /// incremental refresh rebuilds `seen_keys` and re-runs the authoritative
    /// dedupe from cached records alone, never from the transcripts.
    pub request_id: Option<String>,
    /// Defaults to `false` when the JSONL `isSidechain` field is absent.
    pub is_sidechain: bool,
    /// Kept for the dedupe tie-break (`speed.is_some()` vs `speed.is_none()`
    /// -- not specifically `Fast`; see `should_replace_deduped_entry` in the
    /// reference implementation).
    pub speed: Option<Speed>,
    /// Computed at parse time from the effective `model` and raw token
    /// counts via [`crate::pricing::PricingMap`]. `0.0` when `model` has no
    /// resolvable pricing or is `None` and there are nonzero tokens -- see
    /// `missing_pricing`.
    pub cost: f64,
    /// Set when this record has nonzero total tokens but no resolvable
    /// pricing (unknown model, or no effective model at all). Never set for
    /// a zero-token record.
    pub missing_pricing: bool,
}

impl UsageRecord {
    /// Sum of the 5m + 1h cache-creation split.
    pub fn cache_creation_tokens(&self) -> u64 {
        self.cache_creation_5m_tokens.saturating_add(self.cache_creation_1h_tokens)
    }

    /// Sum of all raw token counts, used by the dedupe tie-break.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens).saturating_add(self.cache_creation_tokens()).saturating_add(self.cache_read_tokens)
    }
}

/// Parse one JSONL transcript file into raw (not yet deduped) [`UsageRecord`]s.
/// A read failure returns an empty vec; a malformed/invalid line is silently
/// skipped -- never a panic or a propagated error, so one bad file or line
/// never stops ingest of the rest of the corpus. `pricing` supplies per-model
/// rates so each record's `cost`/`missing_pricing` is computed here.
pub fn parse_file(path: &Path, pricing: &PricingMap) -> Vec<UsageRecord> {
    let Ok(content) = std::fs::read(path) else {
        return Vec::new();
    };
    let (fallback_session_id, project_path) = extract_session_parts(path);
    parse_bytes(&content, &fallback_session_id, &project_path, pricing)
}

/// Parse the lines of already-read raw bytes into [`UsageRecord`]s (not yet
/// deduped). `fallback_session_id`/`project_path` are the path-derived values
/// used when a line has no `sessionId` of its own, normally obtained via
/// [`extract_session_parts`].
///
/// `content` must start and end on line boundaries: every `\n`-separated
/// chunk is parsed as one candidate line, and a trailing chunk with no
/// terminating `\n` is parsed too (a truncated final line simply fails to
/// deserialize and is skipped). The incremental cache relies on this to parse
/// a byte range read from the middle of a growing file, and is responsible for
/// trimming that range to the last complete line first -- see
/// [`crate::cache`].
pub fn parse_bytes(content: &[u8], fallback_session_id: &str, project_path: &str, pricing: &PricingMap) -> Vec<UsageRecord> {
    parse_lines(content, fallback_session_id, project_path, pricing).into_iter().map(|(_, record)| record).collect()
}

/// [`parse_bytes`], with each record's line start as a byte offset into
/// `content`. A line yields at most one record, so the offset identifies the
/// record within its file: the usage store keys its rows on it.
pub fn parse_lines(content: &[u8], fallback_session_id: &str, project_path: &str, pricing: &PricingMap) -> Vec<(u64, UsageRecord)> {
    let mut records = Vec::new();
    let usage_marker = memmem::Finder::new(br#""usage":{"#);
    for line in byte_lines(content) {
        // `byte_lines` hands out subslices of `content`, so the distance
        // between the two starts is the line's offset.
        let offset = line.as_ptr() as usize - content.as_ptr() as usize;
        // Byte-prefilter before any JSON parsing: this is the dominant cost
        // saving, since most lines are not usage records.
        if usage_marker.find(line).is_none() {
            continue;
        }
        if has_unsupported_null_field(line) {
            continue;
        }
        let Ok(data) = serde_json::from_slice::<UsageEntry>(line) else {
            continue;
        };
        let Some(timestamp_ms) = sl_core::format::parse_iso8601_to_epoch_ms(&data.timestamp) else {
            continue;
        };
        if !is_valid_usage_entry(&data) {
            continue;
        }

        let session_id = data.session_id.clone().unwrap_or_else(|| fallback_session_id.to_string());
        let model = data.message.model.as_deref().and_then(|model| {
            if model == "<synthetic>" {
                None
            } else if matches!(data.message.usage.speed, Some(Speed::Fast)) {
                Some(format!("{model}-fast"))
            } else {
                Some(model.to_string())
            }
        });

        let (cache_creation_5m_tokens, cache_creation_1h_tokens) = data.message.usage.cache_creation_split();
        let tokens = UsageTokens {
            input: data.message.usage.input_tokens,
            output: data.message.usage.output_tokens,
            cache_create_5m: cache_creation_5m_tokens,
            cache_create_1h: cache_creation_1h_tokens,
            cache_read: data.message.usage.cache_read_input_tokens,
        };
        let (cost, missing_pricing) = cost_for_usage(pricing, model.as_deref(), &tokens);

        records.push((offset as u64, UsageRecord {
            timestamp_ms,
            date: local_date(timestamp_ms),
            session_id,
            project_path: project_path.to_string(),
            input_tokens: data.message.usage.input_tokens,
            output_tokens: data.message.usage.output_tokens,
            cache_creation_5m_tokens,
            cache_creation_1h_tokens,
            cache_read_tokens: data.message.usage.cache_read_input_tokens,
            model,
            message_id: data.message.id.clone(),
            request_id: data.request_id.clone(),
            is_sidechain: data.is_sidechain == Some(true),
            speed: data.message.usage.speed,
            cost,
            missing_pricing,
        }));
    }
    records
}

/// Convert an epoch-ms timestamp to the system's local calendar date,
/// formatted `YYYY-MM-DD`. Uses `chrono::Local` (real timezone/DST data)
/// rather than a hand-rolled fixed-offset calculation, since local
/// calendar-day boundaries need it.
///
/// Public, and re-exported from the crate root, because every consumer that
/// buckets by day has to agree with [`UsageRecord::date`] on exactly where a
/// day boundary falls -- the statusline, `working_time`, and the tray alike.
/// A second copy of this logic anywhere is a silent divergence waiting to
/// happen; call this one.
pub fn local_date(timestamp_ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(timestamp_ms).single().unwrap_or_else(chrono::Local::now).format("%Y-%m-%d").to_string()
}

/// memchr-based line splitting with no fixed-capacity buffer -- this corpus
/// has lines up to megabytes long.
fn byte_lines(bytes: &[u8]) -> ByteLines<'_> {
    ByteLines { bytes }
}

struct ByteLines<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for ByteLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        if let Some(newline) = memchr(b'\n', self.bytes) {
            let (line, rest) = self.bytes.split_at(newline);
            self.bytes = &rest[1..];
            Some(line)
        } else {
            let line = self.bytes;
            self.bytes = &[];
            Some(line)
        }
    }
}

/// Skip a line unless every `:null`-valued field it contains is in the
/// supported-nullable set. Ported verbatim (byte-for-byte logic) from
/// ccusage's `has_unsupported_null_field`.
fn has_unsupported_null_field(line: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(relative_index) = memmem::find(&line[offset..], b":null") {
        let null_index = offset + relative_index;
        let mut field_end = null_index.saturating_sub(1);
        if line.get(field_end) != Some(&b'"') {
            while field_end > 0 && line[field_end] != b'"' {
                field_end -= 1;
            }
        }
        if line.get(field_end) == Some(&b'"') {
            let mut field_start = field_end.saturating_sub(1);
            while field_start > 0 && line[field_start] != b'"' {
                field_start -= 1;
            }
            if line.get(field_start) == Some(&b'"') && is_unsupported_nullable_field(&line[field_start + 1..field_end]) {
                return true;
            }
        }
        offset = null_index + b":null".len();
    }
    false
}

/// The exact field-name set ccusage treats as *not* allowed to be `null`.
fn is_unsupported_nullable_field(field: &[u8]) -> bool {
    matches!(
        field,
        b"id" | b"cwd"
            | b"model"
            | b"speed"
            | b"costUSD"
            | b"version"
            | b"sessionId"
            | b"requestId"
            | b"isApiErrorMessage"
            | b"cache_read_input_tokens"
            | b"cache_creation_input_tokens"
    )
}

/// `version` (if present) must satisfy [`is_semver_prefix`]; `session_id`,
/// `request_id`, `message.id`, `message.model` -- if present, must not be
/// empty strings. Ported verbatim from ccusage's `is_valid_usage_entry`.
fn is_valid_usage_entry(data: &UsageEntry) -> bool {
    if data.version.as_deref().is_some_and(|version| !is_semver_prefix(version)) {
        return false;
    }
    if data.session_id.as_deref().is_some_and(str::is_empty) {
        return false;
    }
    if data.request_id.as_deref().is_some_and(str::is_empty) {
        return false;
    }
    if data.message.id.as_deref().is_some_and(str::is_empty) {
        return false;
    }
    if data.message.model.as_deref().is_some_and(str::is_empty) {
        return false;
    }
    true
}

/// `^\d+\.\d+\.\d+` prefix match (e.g. `"1.2.3"`, `"1.2.3-beta"` are valid;
/// `"1.2"`, `"abc"` are not). Ported verbatim from ccusage's `is_semver_prefix`.
fn is_semver_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    if !consume_ascii_digits(bytes, &mut index) || bytes.get(index) != Some(&b'.') {
        return false;
    }
    index += 1;
    if !consume_ascii_digits(bytes, &mut index) || bytes.get(index) != Some(&b'.') {
        return false;
    }
    index += 1;
    bytes.get(index).is_some_and(u8::is_ascii_digit)
}

fn consume_ascii_digits(bytes: &[u8], index: &mut usize) -> bool {
    let start = *index;
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        *index += 1;
    }
    *index > start
}

#[cfg(test)]
#[expect(clippy::float_cmp, reason = "each comparison is of a value exact by construction: zero, a small binary fraction, or the same expression on both sides")]
mod tests {
    use super::*;

    fn test_pricing_map() -> &'static PricingMap {
        static MAP: std::sync::OnceLock<PricingMap> = std::sync::OnceLock::new();
        MAP.get_or_init(PricingMap::new)
    }

    fn parse(line: &str) -> Vec<UsageRecord> {
        parse_bytes(line.as_bytes(), "fallback-session", "fallback-project", test_pricing_map())
    }

    /// `extra_usage_fields` must include its own leading comma, e.g.
    /// `,"cache_read_input_tokens":5"`.
    fn usage_line(extra_usage_fields: &str) -> String {
        format!(
            r#"{{"timestamp":"2024-01-15T10:30:00.000Z","sessionId":"sess-1","message":{{"id":"msg-1","model":"claude-sonnet-4","usage":{{"input_tokens":10,"output_tokens":20{extra_usage_fields}}}}}}}"#
        )
    }

    #[test]
    fn cache_creation_breakdown_overrides_flat_field() {
        let usage = TokenUsageRaw {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 999,
            cache_read_input_tokens: 0,
            speed: None,
            cache_creation: Some(CacheCreationRaw { ephemeral_5m_input_tokens: 100, ephemeral_1h_input_tokens: 200 }),
        };
        assert_eq!(usage.cache_creation_token_count(), 300);
    }

    #[test]
    fn cache_creation_falls_back_to_flat_field_when_no_breakdown() {
        let usage = TokenUsageRaw { cache_creation_input_tokens: 42, ..TokenUsageRaw::default() };
        assert_eq!(usage.cache_creation_token_count(), 42);
    }

    #[test]
    fn null_model_field_skips_line() {
        let line = usage_line(r#","cache_read_input_tokens":5"#);
        assert_eq!(parse(&line).len(), 1);

        let bad = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","model":null,"message":{"id":"msg-1","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        assert!(parse(bad).is_empty());
    }

    #[test]
    fn unrelated_null_field_does_not_skip() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","someOtherField":null,"message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        assert_eq!(parse(line).len(), 1);
    }

    #[test]
    fn semver_prefix_validity() {
        assert!(is_semver_prefix("1.2.3"));
        assert!(is_semver_prefix("1.2.3-beta"));
        assert!(!is_semver_prefix("1.2"));
        assert!(!is_semver_prefix("abc"));
    }

    #[test]
    fn invalid_version_skips_line() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","version":"1.2","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn synthetic_model_drops_to_none() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","message":{"id":"msg-1","model":"<synthetic>","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        let records = parse(line);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, None, "a <synthetic> model resolves to no effective model at all");
    }

    #[test]
    fn fast_speed_appends_suffix_to_effective_model_only() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20,"speed":"fast"}}}"#;
        let records = parse(line);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model.as_deref(), Some("claude-sonnet-4-fast"));
        assert_eq!(records[0].speed, Some(Speed::Fast));
    }

    #[test]
    fn missing_required_fields_skip_the_line() {
        // No `message` at all.
        assert!(parse(r#"{"timestamp":"2024-01-15T10:30:00Z"}"#).is_empty());
        // `message.usage` missing.
        assert!(parse(r#"{"timestamp":"2024-01-15T10:30:00Z","message":{"id":"msg-1"}}"#).is_empty());
        // Line has no `"usage":{` at all -- filtered before JSON parsing.
        assert!(parse(r#"{"type":"summary"}"#).is_empty());
    }

    #[test]
    fn unparseable_timestamp_skips_the_line() {
        let line = r#"{"timestamp":"not-a-date","sessionId":"sess-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn session_id_falls_back_to_path_derived_value() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        let records = parse(line);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "fallback-session");
        assert_eq!(records[0].project_path, "fallback-project");
    }

    #[test]
    fn is_sidechain_defaults_to_false() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}"#;
        assert!(!parse(line)[0].is_sidechain);
    }

    #[test]
    fn multiple_lines_and_malformed_lines_are_isolated() {
        let content = format!(
            "{}\nnot even json\n{}\n",
            usage_line(""),
            r#"{"timestamp":"2024-01-16T10:30:00Z","sessionId":"sess-2","message":{"id":"msg-2","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}"#
        );
        let records = parse_bytes(content.as_bytes(), "fallback", "fallback", test_pricing_map());
        assert_eq!(records.len(), 2);
    }

    /// End-to-end: the record's `cost` is computed at parse time from the
    /// real embedded catalogs (`claude-sonnet-4` is a literal, non-exact-only
    /// models.dev entry: $3/$15 per MTok input/output, $3.75/$0.3 per MTok
    /// cache write/read), split cache-creation tokens billed at their own
    /// rates.
    #[test]
    fn cost_is_computed_at_parse_time_from_the_effective_model_and_split_cache_tokens() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":100,"output_tokens":200,"cache_read_input_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}}}}"#;
        let records = parse(line);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.cache_creation_5m_tokens, 10);
        assert_eq!(record.cache_creation_1h_tokens, 20);
        assert!(!record.missing_pricing);

        let input_rate = 3.0 / 1_000_000.0;
        let output_rate = 15.0 / 1_000_000.0;
        let cache_write_rate = 3.75 / 1_000_000.0;
        let cache_read_rate = 0.3 / 1_000_000.0;
        let cache_1h_rate = input_rate * 2.0;
        let expected = 100.0 * input_rate + 200.0 * output_rate + 50.0 * cache_read_rate + 10.0 * cache_write_rate + 20.0 * cache_1h_rate;
        assert!((record.cost - expected).abs() < 1e-12, "expected {expected}, got {}", record.cost);
    }

    #[test]
    fn unknown_model_with_nonzero_tokens_is_flagged_missing_pricing_in_the_record() {
        let line = r#"{"timestamp":"2024-01-15T10:30:00Z","sessionId":"sess-1","message":{"id":"msg-1","model":"totally-unknown-model-xyz","usage":{"input_tokens":10,"output_tokens":10}}}"#;
        let records = parse(line);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cost, 0.0);
        assert!(records[0].missing_pricing);
    }
}
