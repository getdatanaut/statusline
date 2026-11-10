//! Pure aggregation views over an already-loaded [`UsageCache`]: weekly and
//! monthly rollups, a per-session summary, and 5-hour "usage block" tracking.
//!
//! Distinct from [`crate::cache::aggregate`] (`crate::cache::daily_model_aggregates`),
//! which only provides the day/model fold the cache itself needs to persist
//! nothing beyond raw records. Everything here is a further, pure
//! re-derivation of that day/model fold or of [`UsageCache::all_records`] --
//! no filesystem I/O, no live clock (the 5-hour blocks' "now" is a parameter,
//! not `SystemTime::now()`), so every function here is trivially unit
//! testable and safe to call on every tray refresh.

use std::collections::HashSet;

use chrono::{Datelike, NaiveDate};

use crate::cache::{ModelAggregate, SessionGroupCache, UsageCache};
use crate::record::UsageRecord;

// --------------------------------------------------------------------------
// Weekly / monthly buckets
// --------------------------------------------------------------------------

impl std::ops::AddAssign for ModelAggregate {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_5m_tokens = self.cache_creation_5m_tokens.saturating_add(other.cache_creation_5m_tokens);
        self.cache_creation_1h_tokens = self.cache_creation_1h_tokens.saturating_add(other.cache_creation_1h_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(other.cache_read_tokens);
        self.cost += other.cost;
        self.messages = self.messages.saturating_add(other.messages);
    }
}

/// `YYYY-MM` prefix of a `YYYY-MM-DD` date key.
///
/// Public, and re-exported from the crate root, for the same reason
/// [`crate::local_date`] and [`monday_of_week`] are: a consumer that buckets by
/// month must land on the same key [`monthly_model_aggregates`] does. Don't
/// reimplement it.
pub fn month_of_date(date: &str) -> String {
    // `date` is always an ASCII `YYYY-MM-DD` string produced by
    // `UsageRecord::date`, so byte-slicing at 7 is safe; the fallback is only
    // defensive for a malformed key that should never occur in practice.
    date.get(..7).unwrap_or(date).to_string()
}

/// `YYYY-MM-DD` of the Monday starting the ISO week that contains `date` (a
/// `YYYY-MM-DD` string). Uses `chrono::NaiveDate` for correct calendar math
/// across month/year boundaries rather than hand-rolled day-of-week
/// arithmetic.
///
/// Deliberately Monday, not Sunday: ccusage's own CLI is inconsistent between
/// the two across its own commands, and the Raycast extension this project
/// replaces uses ISO week (Monday-start) throughout.
///
/// Public, and re-exported from the crate root, for the same reason
/// [`crate::local_date`] is: a consumer that buckets by week must land on the
/// same Monday this module's `weekly_model_aggregates` does. Don't reimplement
/// it.
pub fn monday_of_week(date: &str) -> String {
    match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(parsed) => {
            let days_from_monday = i64::from(parsed.weekday().num_days_from_monday());
            let monday = parsed - chrono::Duration::days(days_from_monday);
            monday.format("%Y-%m-%d").to_string()
        }
        // Malformed date key should never occur (see `month_of_date`); fall
        // back to the input unchanged rather than panicking.
        Err(_) => date.to_string(),
    }
}

// --------------------------------------------------------------------------
// Sessions view
// --------------------------------------------------------------------------

/// One session's summary: costs, tokens and metadata folded from its
/// deduped records.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub project_path: String,
    pub cost: f64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub message_count: usize,
    pub first_seen: i64,
    pub last_activity: i64,
    /// Distinct effective models used, in first-seen order. Records with no
    /// effective model (`UsageRecord::model == None`) contribute nothing
    /// here -- they used no billable model, so there is nothing to list.
    pub models: Vec<String>,
}

/// One [`SessionSummary`] per session group, sorted descending by cost.
///
/// A session group with no records after dedup, or whose records carry only
/// zero tokens (e.g. pure tool-error replays that passed the ingest gates but
/// have no real usage), produces no summary at all.
pub fn session_summaries(cache: &UsageCache) -> Vec<SessionSummary> {
    summarize_groups(cache.groups().values())
}

fn summarize_groups<'a>(groups: impl Iterator<Item = &'a SessionGroupCache>) -> Vec<SessionSummary> {
    let mut summaries: Vec<SessionSummary> = groups.filter_map(summarize_group).collect();
    summaries.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));
    summaries
}

pub(crate) fn summarize_group(group: &SessionGroupCache) -> Option<SessionSummary> {
    // No records after dedup (every line in this group's files failed to
    // parse): nothing to summarize.
    let first = group.records.first()?;
    let project_path = first.project_path.clone();

    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_creation_5m_tokens = 0u64;
    let mut cache_creation_1h_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut models = Vec::new();
    let mut seen_models = HashSet::new();

    for record in &group.records {
        input_tokens = input_tokens.saturating_add(record.input_tokens);
        output_tokens = output_tokens.saturating_add(record.output_tokens);
        cache_creation_5m_tokens = cache_creation_5m_tokens.saturating_add(record.cache_creation_5m_tokens);
        cache_creation_1h_tokens = cache_creation_1h_tokens.saturating_add(record.cache_creation_1h_tokens);
        cache_read_tokens = cache_read_tokens.saturating_add(record.cache_read_tokens);
        if let Some(model) = &record.model
            && seen_models.insert(model.clone())
        {
            models.push(model.clone());
        }
    }

    let total_tokens =
        input_tokens.saturating_add(output_tokens).saturating_add(cache_creation_5m_tokens).saturating_add(cache_creation_1h_tokens).saturating_add(cache_read_tokens);
    // Every record parsed but carried zero usage across the board (e.g. a
    // pure tool-error replay): drop the row rather than show a zero-cost,
    // zero-token session.
    if total_tokens == 0 {
        return None;
    }

    Some(SessionSummary {
        session_id: group.session_id.clone(),
        project_path,
        cost: group.total_cost(),
        total_tokens,
        input_tokens,
        output_tokens,
        message_count: group.records.len(),
        first_seen: group.first_seen,
        last_activity: group.last_activity,
        models,
    })
}

// --------------------------------------------------------------------------
// Five-hour usage blocks
// --------------------------------------------------------------------------
//
// Ported from `identify_session_blocks`/`create_block`/`create_gap_block`/
// `floor_to_hour`/`calculate_burn_rate` in
// <https://github.com/ccusage/ccusage/blob/8841f921/rust/crates/ccusage/src/blocks.rs>. Notable deviations
// from that reference:
//   - Fixed 5-hour block duration (ccusage takes a configurable
//     `session_duration_hours`; this crate has no such setting yet).
//   - No `id`/`usage_limit_reset_time` fields (not modeled by `UsageRecord`).
//   - No `project_block_usage` (needs a live "now" at render time; that is
//     the tray task's job, using this module's `cost`/`cost_per_hour`/
//     `is_active`/`end_time_ms`).

const MILLIS_PER_MINUTE: i64 = 60_000;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;
/// Claude's account-wide rate-limit window.
pub(crate) const BLOCK_DURATION_MS: i64 = 5 * MILLIS_PER_HOUR;

/// One 5-hour usage block (or, when `is_gap` is set, the idle span between
/// two blocks).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBlock {
    /// `floor_to_hour` (UTC) of the block's first entry's timestamp; for a
    /// gap block, `last_record_in_previous_block.timestamp_ms + 5h`.
    pub start_time_ms: i64,
    /// `start_time_ms + 5h`; for a gap block, the timestamp of the record
    /// that ended the gap.
    pub end_time_ms: i64,
    /// Timestamp of the block's last entry. `None` for a gap block (which has
    /// no entries).
    pub actual_end_time_ms: Option<i64>,
    /// Timestamp of the block's first entry. `None` for a gap block. Kept
    /// separately from `start_time_ms` because burn rate is computed over the
    /// entries' own span, not the floored block window.
    pub first_entry_time_ms: Option<i64>,
    /// `actual_end_time_ms` is within the last 5h and `end_time_ms` hasn't
    /// passed yet, relative to the `now_ms` passed to [`identify_blocks`].
    /// Always `false` for a gap block.
    pub is_active: bool,
    /// A gap block represents idle time between two real blocks: no entries,
    /// no cost, no tokens.
    pub is_gap: bool,
    pub cost: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_5m_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub cache_read_tokens: u64,
    /// Distinct effective models used in the block, in first-seen order.
    pub models: Vec<String>,
    /// Number of entries folded into this block (`0` for a gap block).
    pub entry_count: usize,
}

/// Burn rate for one active (or completed) block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockBurnRate {
    pub tokens_per_minute: f64,
    /// Same as `tokens_per_minute` but counting only `input_tokens +
    /// output_tokens` (excludes cache tokens) -- the reference's "indicator"
    /// rate, meant for a burn-rate display that ignores cache noise.
    pub tokens_per_minute_for_indicator: f64,
    pub cost_per_hour: f64,
}

/// UTC-hour floor: epoch ms has no timezone, so this is inherently UTC --
/// deliberately not converted to local time first.
pub(crate) fn floor_to_hour(timestamp_ms: i64) -> i64 {
    timestamp_ms.div_euclid(MILLIS_PER_HOUR) * MILLIS_PER_HOUR
}

/// Walk every record globally (not per-session -- Claude's 5-hour rate-limit
/// window is account-wide) in timestamp order, splitting into 5-hour blocks.
///
/// A new block starts when a record is more than 5 hours (strictly) after
/// either the current block's floored start or its own immediately preceding
/// record; the latter case also emits a gap block spanning the idle time.
/// `now_ms` decides `is_active` and is supplied by the caller -- this
/// function never reads a live clock.
pub fn identify_blocks(records: &[&UsageRecord], now_ms: i64) -> Vec<SessionBlock> {
    if records.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&UsageRecord> = records.to_vec();
    sorted.sort_by_key(|record| record.timestamp_ms);

    let mut blocks = Vec::new();
    let mut current_start: Option<i64> = None;
    let mut current_entries: Vec<&UsageRecord> = Vec::new();

    for record in sorted {
        if let Some(start) = current_start {
            let last_time = current_entries.last().map_or(start, |entry| entry.timestamp_ms);
            let since_start = record.timestamp_ms - start;
            let since_last = record.timestamp_ms - last_time;
            if since_start > BLOCK_DURATION_MS || since_last > BLOCK_DURATION_MS {
                blocks.push(create_block(start, &std::mem::take(&mut current_entries), now_ms));
                if since_last > BLOCK_DURATION_MS {
                    blocks.push(create_gap_block(last_time, record.timestamp_ms));
                }
                current_start = Some(floor_to_hour(record.timestamp_ms));
            }
        } else {
            current_start = Some(floor_to_hour(record.timestamp_ms));
        }
        current_entries.push(record);
    }

    if let Some(start) = current_start
        && !current_entries.is_empty()
    {
        blocks.push(create_block(start, &current_entries, now_ms));
    }
    blocks
}

fn create_block(start_time_ms: i64, entries: &[&UsageRecord], now_ms: i64) -> SessionBlock {
    let end_time_ms = start_time_ms + BLOCK_DURATION_MS;
    let first_entry_time_ms = entries.first().map(|entry| entry.timestamp_ms);
    let actual_end_time_ms = entries.last().map(|entry| entry.timestamp_ms);
    let is_active = actual_end_time_ms.is_some_and(|last| now_ms - last < BLOCK_DURATION_MS && now_ms < end_time_ms);

    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_creation_5m_tokens = 0u64;
    let mut cache_creation_1h_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut cost = 0.0;
    let mut models = Vec::new();
    let mut seen_models = HashSet::new();
    let entry_count = entries.len();

    for entry in entries {
        input_tokens = input_tokens.saturating_add(entry.input_tokens);
        output_tokens = output_tokens.saturating_add(entry.output_tokens);
        cache_creation_5m_tokens = cache_creation_5m_tokens.saturating_add(entry.cache_creation_5m_tokens);
        cache_creation_1h_tokens = cache_creation_1h_tokens.saturating_add(entry.cache_creation_1h_tokens);
        cache_read_tokens = cache_read_tokens.saturating_add(entry.cache_read_tokens);
        cost += entry.cost;
        if let Some(model) = &entry.model
            && seen_models.insert(model.clone())
        {
            models.push(model.clone());
        }
    }

    SessionBlock {
        start_time_ms,
        end_time_ms,
        actual_end_time_ms,
        first_entry_time_ms,
        is_active,
        is_gap: false,
        cost,
        input_tokens,
        output_tokens,
        cache_creation_5m_tokens,
        cache_creation_1h_tokens,
        cache_read_tokens,
        models,
        entry_count,
    }
}

fn create_gap_block(last_time_ms: i64, next_time_ms: i64) -> SessionBlock {
    SessionBlock {
        start_time_ms: last_time_ms + BLOCK_DURATION_MS,
        end_time_ms: next_time_ms,
        actual_end_time_ms: None,
        first_entry_time_ms: None,
        is_active: false,
        is_gap: true,
        cost: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_5m_tokens: 0,
        cache_creation_1h_tokens: 0,
        cache_read_tokens: 0,
        models: Vec::new(),
        entry_count: 0,
    }
}

/// Burn rate for a block, or `None` for a gap block, an empty block, or a
/// block whose entries all share one timestamp (zero span -- rate is
/// undefined, not infinite/zero).
///
/// The denominator is the span from the block's first to last entry
/// timestamp, not elapsed time since the floored block start -- a block whose
/// entries are clustered well after `start_time_ms` should not have its rate
/// diluted by the empty time before its first entry.
pub fn block_burn_rate(block: &SessionBlock) -> Option<BlockBurnRate> {
    if block.is_gap {
        return None;
    }
    let first = block.first_entry_time_ms?;
    let last = block.actual_end_time_ms?;
    let duration_minutes = (last - first) as f64 / MILLIS_PER_MINUTE as f64;
    if duration_minutes <= 0.0 {
        return None;
    }
    let total_tokens =
        (block.input_tokens + block.output_tokens + block.cache_creation_5m_tokens + block.cache_creation_1h_tokens + block.cache_read_tokens) as f64;
    let non_cache_tokens = (block.input_tokens + block.output_tokens) as f64;
    Some(BlockBurnRate {
        tokens_per_minute: total_tokens / duration_minutes,
        tokens_per_minute_for_indicator: non_cache_tokens / duration_minutes,
        cost_per_hour: block.cost / duration_minutes * 60.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------- fixtures

    fn record(timestamp_ms: i64, session_id: &str, model: Option<&str>, input: u64, output: u64, cost: f64) -> UsageRecord {
        UsageRecord {
            timestamp_ms,
            date: "2024-01-15".to_string(),
            session_id: session_id.to_string(),
            project_path: "proj".to_string(),
            input_tokens: input,
            output_tokens: output,
            model: model.map(str::to_string),
            cost,
            ..UsageRecord::default()
        }
    }

    fn group(session_id: &str, records: Vec<UsageRecord>) -> SessionGroupCache {
        let first_seen = records.iter().map(|record| record.timestamp_ms).min().unwrap_or(0);
        let last_activity = records.iter().map(|record| record.timestamp_ms).max().unwrap_or(0);
        SessionGroupCache { session_id: session_id.to_string(), records, first_seen, last_activity }
    }

    // ---------------------------------------------------- weekly/monthly

    #[test]
    fn month_bucket_truncates_to_year_month() {
        assert_eq!(month_of_date("2026-08-31"), "2026-08");
        assert_eq!(month_of_date("2024-01-05"), "2024-01");
    }

    #[test]
    fn week_bucket_uses_monday_of_the_iso_week() {
        // 2024-01-01 is a Monday; every day through 2024-01-07 (Sunday) is
        // the same ISO week.
        assert_eq!(monday_of_week("2024-01-01"), "2024-01-01");
        assert_eq!(monday_of_week("2024-01-03"), "2024-01-01");
        assert_eq!(monday_of_week("2024-01-07"), "2024-01-01");
    }

    #[test]
    fn week_bucket_handles_a_year_boundary_week() {
        // 2025-01-01 is a Wednesday; the Monday starting that ISO week falls
        // in the previous year.
        assert_eq!(monday_of_week("2024-12-30"), "2024-12-30");
        assert_eq!(monday_of_week("2024-12-31"), "2024-12-30");
        assert_eq!(monday_of_week("2025-01-01"), "2024-12-30");
        assert_eq!(monday_of_week("2025-01-02"), "2024-12-30");
    }



    // -------------------------------------------------------------- sessions

    #[test]
    fn sessions_sort_descending_by_cost() {
        let groups = [
            group("cheap", vec![record(1, "cheap", Some("m"), 1, 1, 1.0)]),
            group("expensive", vec![record(1, "expensive", Some("m"), 1, 1, 9.0)]),
            group("mid", vec![record(1, "mid", Some("m"), 1, 1, 5.0)]),
        ];
        let summaries = summarize_groups(groups.iter());
        let costs: Vec<f64> = summaries.iter().map(|s| s.cost).collect();
        assert_eq!(costs, vec![9.0, 5.0, 1.0]);
    }

    #[test]
    fn a_session_with_only_zero_token_records_is_dropped() {
        let groups = [group("zero", vec![record(1, "zero", None, 0, 0, 0.0), record(2, "zero", None, 0, 0, 0.0)])];
        let summaries = summarize_groups(groups.iter());
        assert!(summaries.is_empty());
    }

    #[test]
    fn a_session_group_with_no_records_produces_no_summary() {
        // Distinct code path from the zero-token case: `records` itself is
        // empty (every line in the group's files failed to parse).
        let groups = [group("empty", vec![])];
        let summaries = summarize_groups(groups.iter());
        assert!(summaries.is_empty());
    }

    #[test]
    fn a_sessions_models_list_matches_its_records_distinct_effective_models() {
        let records = vec![
            record(1, "sess", Some("claude-opus-4"), 1, 1, 1.0),
            record(2, "sess", Some("claude-sonnet-4"), 1, 1, 1.0),
            record(3, "sess", Some("claude-opus-4"), 1, 1, 1.0),
            // No effective model: contributes nothing to `models`.
            record(4, "sess", None, 1, 1, 0.0),
        ];
        let groups = [group("sess", records)];
        let summaries = summarize_groups(groups.iter());
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].models, vec!["claude-opus-4".to_string(), "claude-sonnet-4".to_string()], "first-seen order, deduped");
    }

    #[test]
    fn session_summary_totals_tokens_and_message_count() {
        let mut r1 = record(1, "sess", Some("m"), 10, 20, 1.0);
        r1.cache_creation_5m_tokens = 2;
        r1.cache_creation_1h_tokens = 3;
        r1.cache_read_tokens = 4;
        let r2 = record(2, "sess", Some("m"), 5, 5, 0.5);
        let groups = [group("sess", vec![r1, r2])];
        let summaries = summarize_groups(groups.iter());
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.input_tokens, 15);
        assert_eq!(summary.output_tokens, 25);
        assert_eq!(summary.total_tokens, 15 + 25 + 2 + 3 + 4);
        assert_eq!(summary.message_count, 2);
        assert!((summary.cost - 1.5).abs() < 1e-12);
        assert_eq!(summary.project_path, "proj");
    }

    // ------------------------------------------------------------------ blocks

    const FIVE_HOURS_MS: i64 = 5 * 3_600_000;

    #[test]
    fn utc_hour_flooring_is_independent_of_local_timezone() {
        // 2024-01-15T10:47:33.250Z, asserted against the known UTC-floored
        // epoch-ms value directly (not a locally-formatted string), so this
        // test can't accidentally pass only on a UTC test machine.
        let timestamp_ms: i64 = 1_705_315_653_250; // 2024-01-15T10:47:33.250Z
        let floored = floor_to_hour(timestamp_ms);
        assert_eq!(floored, 1_705_312_800_000); // 2024-01-15T10:00:00.000Z

        let records = [record(timestamp_ms, "sess", Some("m"), 1, 1, 0.1)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, timestamp_ms);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_time_ms, 1_705_312_800_000);
        assert_eq!(blocks[0].end_time_ms, 1_705_312_800_000 + FIVE_HOURS_MS);
    }

    #[test]
    fn exactly_five_hours_apart_does_not_split() {
        let base = 1_705_312_800_000; // an exact UTC hour boundary
        let records = [record(base, "sess", Some("m"), 1, 1, 0.1), record(base + FIVE_HOURS_MS, "sess", Some("m"), 1, 1, 0.1)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, base + FIVE_HOURS_MS);
        assert_eq!(blocks.len(), 1, "exactly 5h apart must not split (> not >=)");
        assert_eq!(blocks[0].entry_count, 2);
    }

    #[test]
    fn five_hours_plus_one_ms_apart_does_split() {
        let base = 1_705_312_800_000;
        let records = [record(base, "sess", Some("m"), 1, 1, 0.1), record(base + FIVE_HOURS_MS + 1, "sess", Some("m"), 1, 1, 0.1)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, base + FIVE_HOURS_MS + 1);
        // First block (1 entry), a gap block, then the second block (1 entry).
        assert_eq!(blocks.len(), 3);
        assert!(!blocks[0].is_gap);
        assert!(blocks[1].is_gap);
        assert!(!blocks[2].is_gap);
    }

    #[test]
    fn since_last_trigger_alone_splits_and_emits_a_gap_block() {
        // Two records, individually close to their own block boundaries but
        // more than 5h apart from each other -- both `since_start` (from a
        // fresh floored start) and `since_last` would trigger here, but this
        // specifically exercises the gap-block emission tied to `since_last`.
        let base = 1_705_312_800_000;
        let far = base + FIVE_HOURS_MS + 3_600_000; // 6h later
        let records = [record(base, "sess-a", Some("m"), 1, 1, 0.1), record(far, "sess-b", Some("m"), 1, 1, 0.1)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, far);
        assert_eq!(blocks.len(), 3);
        let gap = &blocks[1];
        assert!(gap.is_gap);
        assert_eq!(gap.entry_count, 0);
        assert_eq!(gap.cost, 0.0);
        assert_eq!(gap.input_tokens, 0);
        assert_eq!(gap.start_time_ms, base + FIVE_HOURS_MS, "gap starts 5h after the last entry that closed the previous block");
        assert_eq!(gap.end_time_ms, far, "gap ends at the record that broke the idle span");
    }

    #[test]
    fn since_start_alone_can_trigger_a_split_even_when_consecutive_gaps_are_small() {
        // Many records each ~10 minutes apart (small `since_last` every
        // time), but the block's total span from its floored start exceeds
        // 5h -- `since_start` must trigger the split on its own.
        let base = 1_705_312_800_000; // exact UTC hour boundary
        let ten_minutes = 10 * 60_000;
        let mut records = Vec::new();
        let mut timestamp = base;
        // 32 records * 10 minutes = 320 minutes = 5h20m span, each gap tiny.
        for _ in 0..32 {
            records.push(record(timestamp, "sess", Some("m"), 1, 1, 0.01));
            timestamp += ten_minutes;
        }
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let now = timestamp;
        let blocks = identify_blocks(&refs, now);
        assert!(blocks.len() >= 2, "the block must have split purely from since_start, since since_last never exceeds 10 minutes");
        // No gap block: since_last never triggered, only since_start did.
        assert!(!blocks.iter().any(|b| b.is_gap));
    }

    #[test]
    fn is_active_requires_both_recent_last_entry_and_end_time_not_yet_passed() {
        let base = 1_705_312_800_000;
        let records = [record(base, "sess", Some("m"), 1, 1, 0.1)];
        let refs: Vec<&UsageRecord> = records.iter().collect();

        // now shortly after the entry, well before end_time: active.
        let active = identify_blocks(&refs, base + 60_000);
        assert!(active[0].is_active);

        // now past end_time (5h after floored start): not active even though
        // "recent" relative to the last entry doesn't even apply here.
        let past_end = identify_blocks(&refs, base + FIVE_HOURS_MS + 1);
        assert!(!past_end[0].is_active);

        // now within the window but the last entry is >5h stale relative to
        // "now" (only possible if end_time_ms is far in the future, but
        // since end_time = start + 5h and last entry <= now always for a
        // realistic "now", exercise the boundary via a far-future now that's
        // still before a synthetically distant end -- covered by past_end
        // above for the practical case; this covers now == entry time).
        let at_entry = identify_blocks(&refs, base);
        assert!(at_entry[0].is_active);
    }

    #[test]
    fn single_entry_block_has_no_burn_rate() {
        let records = [record(1_705_312_800_000, "sess", Some("m"), 10, 10, 1.0)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, 1_705_312_800_000);
        assert_eq!(blocks.len(), 1);
        assert!(block_burn_rate(&blocks[0]).is_none());
    }

    #[test]
    fn burn_rate_uses_first_to_last_entry_span_not_elapsed_since_block_start() {
        // Floored block start is on the hour; first entry is 50 minutes
        // later, last entry 10 minutes after that (60 minutes after floored
        // start). The burn rate must use the 10-minute entry span, not the
        // 60-minute elapsed-since-start span.
        let hour_start = 1_705_312_800_000;
        let first_entry = hour_start + 50 * 60_000;
        let last_entry = first_entry + 10 * 60_000;
        let records = [record(first_entry, "sess", Some("m"), 100, 100, 6.0), record(last_entry, "sess", Some("m"), 0, 0, 0.0)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, last_entry);
        assert_eq!(blocks.len(), 1);
        let rate = block_burn_rate(&blocks[0]).expect("multi-entry block has a burn rate");
        // cost 6.0 over 10 minutes => $36/hr, not $6/hr (which a 60-minute
        // elapsed-since-start denominator would wrongly produce).
        assert!((rate.cost_per_hour - 36.0).abs() < 1e-9, "got {}", rate.cost_per_hour);
    }

    #[test]
    fn gap_blocks_have_no_burn_rate() {
        let base = 1_705_312_800_000;
        let far = base + FIVE_HOURS_MS + 3_600_000;
        let records = [record(base, "sess", Some("m"), 1, 1, 0.1), record(far, "sess", Some("m"), 1, 1, 0.1)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, far);
        let gap = blocks.iter().find(|b| b.is_gap).unwrap();
        assert!(block_burn_rate(gap).is_none());
    }

    #[test]
    fn blocks_are_global_across_sessions_not_per_session() {
        let base = 1_705_312_800_000;
        let records = [
            record(base, "session-a", Some("m"), 1, 1, 0.1),
            record(base + 60_000, "session-b", Some("m"), 1, 1, 0.1),
            record(base + 120_000, "session-a", Some("m"), 1, 1, 0.1),
        ];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, base + 120_000);
        assert_eq!(blocks.len(), 1, "records from two sessions close in time land in the same block");
        assert_eq!(blocks[0].entry_count, 3);
    }

    #[test]
    fn block_folds_cost_tokens_and_distinct_models_in_first_seen_order() {
        let base = 1_705_312_800_000;
        let mut r1 = record(base, "sess", Some("claude-opus-4"), 10, 20, 1.0);
        r1.cache_creation_5m_tokens = 1;
        r1.cache_creation_1h_tokens = 2;
        r1.cache_read_tokens = 3;
        let r2 = record(base + 60_000, "sess", Some("claude-sonnet-4"), 5, 5, 0.5);
        let r3 = record(base + 120_000, "sess", Some("claude-opus-4"), 1, 1, 0.1);
        let records = [r1, r2, r3];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let blocks = identify_blocks(&refs, base + 120_000);
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.input_tokens, 16);
        assert_eq!(block.output_tokens, 26);
        assert_eq!(block.cache_creation_5m_tokens, 1);
        assert_eq!(block.cache_creation_1h_tokens, 2);
        assert_eq!(block.cache_read_tokens, 3);
        assert!((block.cost - 1.6).abs() < 1e-12);
        assert_eq!(block.models, vec!["claude-opus-4".to_string(), "claude-sonnet-4".to_string()]);
    }

    #[test]
    fn empty_records_produce_no_blocks() {
        let records: Vec<UsageRecord> = Vec::new();
        let refs: Vec<&UsageRecord> = records.iter().collect();
        assert!(identify_blocks(&refs, 0).is_empty());
    }
}
