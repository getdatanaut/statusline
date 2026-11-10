//! [`UsageTotals`]: the usage store as a long-lived process wants it --
//! rollups in memory, rows in SQLite.
//!
//! Everything a menu shows is a total: per day and model, per model, per
//! session, the active 5-hour block. The store keeps one rollup bucket per
//! session group, day and model, rebuilt from the rows whenever they change
//! (see [`super::store`]); this type folds those buckets, plus the short
//! record tail the active block needs. Nothing here reads a row.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::aggregate::{ModelAggregate, UNKNOWN_MODEL};
use super::store::{BlockEntry, RefreshStats, Store};
use super::{RefreshMode, Source, default_store_dir};
use crate::aggregate::{BLOCK_DURATION_MS, SessionSummary, floor_to_hour};
use crate::pricing::PricingMap;

/// All-time totals for one model.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelTotals {
    pub cost: f64,
    /// Every token kind summed.
    pub tokens: u64,
    pub output_tokens: u64,
    /// At least one record of this model had no price.
    pub unpriced: bool,
}

/// All-time token counts by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    /// 5-minute and 1-hour cache creation together.
    pub cache_write: u64,
    pub cache_read: u64,
}

/// The 5-hour usage block `now` falls in, if any: [`crate::identify_blocks`]'s
/// last block, when it is active.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveBlock {
    pub cost: f64,
    /// Every token kind summed.
    pub tokens: u64,
    /// The first entry's timestamp floored to the hour (UTC).
    pub start_time_ms: i64,
    pub end_time_ms: i64,
    pub first_entry_ms: i64,
    pub last_entry_ms: i64,
}

impl ActiveBlock {
    /// Dollars per hour over the span from the first to the last entry, or
    /// `None` when they share a timestamp (see [`crate::block_burn_rate`]).
    pub fn cost_per_hour(&self) -> Option<f64> {
        let minutes = (self.last_entry_ms - self.first_entry_ms) as f64 / 60_000.0;
        (minutes > 0.0).then(|| self.cost / minutes * 60.0)
    }
}

/// The usage store, as totals. See the module docs.
pub struct UsageTotals {
    store: Store,
    source: Source,
    last: RefreshStats,
    /// Kept between refreshes, so a quiet one reads no directory -- and, watched,
    /// `stat`s neither a directory nor a file.
    walk: crate::paths::Walk,
    /// What the last [`refresh`](Self::refresh) found on disk.
    discovered: Vec<PathBuf>,
}

impl std::fmt::Debug for UsageTotals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageTotals").field("records", &self.record_count()).field("last", &self.last).finish_non_exhaustive()
    }
}

impl UsageTotals {
    /// Nothing held yet, stored under the default directory. Fill it with
    /// [`refresh`](Self::refresh); the database is opened then.
    pub fn empty_default() -> Self {
        Self::empty(&default_store_dir())
    }

    /// Nothing held yet, stored in `dir` (as `usage.sqlite`).
    pub fn empty(dir: &Path) -> Self {
        Self::empty_for(dir, Source::Claude)
    }

    /// Nothing held yet of `source`'s usage, stored in `dir` (in the same
    /// `usage.sqlite` as the other source's).
    pub fn empty_for(dir: &Path, source: Source) -> Self {
        Self { store: Store::new(dir, source), source, last: RefreshStats::default(), walk: source.walk(), discovered: Vec::new() }
    }

    /// Listen for file-system events between refreshes, so a quiet one
    /// `stat`s nothing: for a process that refreshes again and again. See
    /// [`crate::watch`].
    #[must_use]
    pub fn watched(mut self) -> Self {
        self.walk = self.source.watched_walk();
        self
    }

    /// [`watched`](Self::watched), ringing `bell` whenever a transcript
    /// changes: for a process that refreshes when told rather than on a timer.
    #[must_use]
    pub fn ringing(mut self, bell: crate::watch::Bell) -> Self {
        self.walk = self.source.watched_walk().ringing(bell);
        self
    }

    /// Whose usage this holds.
    pub fn source(&self) -> Source {
        self.source
    }

    /// Refresh against the files on disk, discovering them itself. The store
    /// holds the whole tail the active block can need, so no clock is needed
    /// here: [`active_block`](Self::active_block) takes one.
    pub fn refresh(&mut self, pricing: &PricingMap, mode: RefreshMode) {
        let files = self.source.discover_with(&mut self.walk);
        self.refresh_walked(files, pricing, mode);
    }

    fn refresh_walked(&mut self, files: Vec<PathBuf>, pricing: &PricingMap, mode: RefreshMode) {
        self.last = self.store.refresh(&files, self.walk.changed(), pricing, mode);
        if self.last.error.is_some() {
            // What the walk said changed was not acted on: look at it all.
            self.walk.distrust();
        }
        self.discovered = files;
    }

    /// [`refresh`](Self::refresh) over the transcripts under `roots`, kept
    /// between calls like the walk of a refresh that discovers them itself.
    pub fn refresh_under(&mut self, roots: &[PathBuf], pricing: &PricingMap, mode: RefreshMode) {
        let files = self.walk.files(roots);
        self.refresh_walked(files, pricing, mode);
    }

    /// The files the last [`refresh`](Self::refresh) found on disk.
    pub fn discovered(&self) -> &[PathBuf] {
        &self.discovered
    }

    /// [`refresh`](Self::refresh) over an explicit file list, for tests.
    pub fn refresh_files(&mut self, files: &[PathBuf], pricing: &PricingMap, mode: RefreshMode) {
        self.last = self.store.refresh(files, None, pricing, mode);
    }

    /// What the last refresh did.
    pub fn last_refresh(&self) -> &RefreshStats {
        &self.last
    }

    /// How far a transcript has been read, or `None` if it is not stored.
    pub fn parsed_offset(&self, path: &Path) -> Option<u64> {
        self.store.state.parsed_offset(path)
    }

    /// Total number of records, after dedupe.
    pub fn record_count(&self) -> usize {
        self.store.state.buckets.values().map(|bucket| usize::try_from(bucket.aggregate.messages).unwrap_or(usize::MAX)).fold(0, usize::saturating_add)
    }

    /// Sum of every record's cost, in USD.
    pub fn total_cost(&self) -> f64 {
        self.store.state.buckets.values().map(|bucket| bucket.aggregate.cost).sum()
    }

    /// Number of session groups with a transcript on disk.
    pub fn group_count(&self) -> usize {
        self.store.state.live_groups()
    }

    fn model_name(&self, model: i64) -> &str {
        self.store.state.model_name(model).unwrap_or(UNKNOWN_MODEL)
    }

    /// Day (`YYYY-MM-DD`, local) -> model -> aggregate, as
    /// [`UsageCache::daily_model_aggregates`](super::UsageCache::daily_model_aggregates).
    pub fn daily_model_aggregates(&self) -> HashMap<String, HashMap<String, ModelAggregate>> {
        let mut by_date: HashMap<String, HashMap<String, ModelAggregate>> = HashMap::new();
        for (key, bucket) in &self.store.state.buckets {
            *by_date.entry(key.day.clone()).or_default().entry(self.model_name(key.model).to_owned()).or_default() += bucket.aggregate;
        }
        by_date
    }

    /// All-time totals per model (`unknown` for records with none).
    pub fn model_totals(&self) -> HashMap<String, ModelTotals> {
        let mut by_model: HashMap<String, ModelTotals> = HashMap::new();
        for (key, bucket) in &self.store.state.buckets {
            let totals = by_model.entry(self.model_name(key.model).to_owned()).or_default();
            totals.cost += bucket.aggregate.cost;
            totals.tokens = totals.tokens.saturating_add(bucket.aggregate.total_tokens());
            totals.output_tokens = totals.output_tokens.saturating_add(bucket.aggregate.output_tokens);
            totals.unpriced |= bucket.unpriced;
        }
        by_model
    }

    /// All-time token counts by kind.
    pub fn token_totals(&self) -> TokenTotals {
        let mut totals = TokenTotals::default();
        for bucket in self.store.state.buckets.values() {
            let a = &bucket.aggregate;
            totals.input = totals.input.saturating_add(a.input_tokens);
            totals.output = totals.output.saturating_add(a.output_tokens);
            totals.cache_write = totals.cache_write.saturating_add(a.cache_creation_5m_tokens).saturating_add(a.cache_creation_1h_tokens);
            totals.cache_read = totals.cache_read.saturating_add(a.cache_read_tokens);
        }
        totals
    }

    /// One summary per session group, most expensive first, as
    /// [`crate::session_summaries`].
    pub fn session_summaries(&self) -> Vec<SessionSummary> {
        let state = &self.store.state;
        // The buckets are ordered by group, so each group's are contiguous.
        let mut by_group: BTreeMap<i64, Vec<(i64, &super::store::Bucket)>> = BTreeMap::new();
        for (key, bucket) in &state.buckets {
            by_group.entry(key.group).or_default().push((key.model, bucket));
        }
        let mut summaries: Vec<SessionSummary> = by_group
            .into_iter()
            .filter_map(|(group, buckets)| {
                let session_id = state.group_name(group)?.to_owned();
                let first = buckets.iter().min_by_key(|(_, bucket)| bucket.first_ts)?;
                let mut summary = SessionSummary {
                    session_id,
                    project_path: first.1.project.clone(),
                    cost: 0.0,
                    total_tokens: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    message_count: 0,
                    first_seen: i64::MAX,
                    last_activity: i64::MIN,
                    models: Vec::new(),
                };
                // Models in first-seen order: each model's earliest bucket.
                let mut models: BTreeMap<i64, i64> = BTreeMap::new();
                for (model, bucket) in &buckets {
                    let a = &bucket.aggregate;
                    summary.cost += a.cost;
                    summary.total_tokens = summary.total_tokens.saturating_add(a.total_tokens());
                    summary.input_tokens = summary.input_tokens.saturating_add(a.input_tokens);
                    summary.output_tokens = summary.output_tokens.saturating_add(a.output_tokens);
                    summary.message_count = summary.message_count.saturating_add(usize::try_from(a.messages).unwrap_or(usize::MAX));
                    summary.first_seen = summary.first_seen.min(bucket.first_ts);
                    summary.last_activity = summary.last_activity.max(bucket.last_ts);
                    if state.model_name(*model).is_some() {
                        let seen = models.entry(*model).or_insert(bucket.first_ts);
                        *seen = (*seen).min(bucket.first_ts);
                    }
                }
                let mut models: Vec<(i64, String)> =
                    models.into_iter().filter_map(|(model, first)| Some((first, state.model_name(model)?.to_owned()))).collect();
                models.sort();
                summary.models = models.into_iter().map(|(_, name)| name).collect();
                // As `summarize_group`: a group whose records carry no usage at
                // all is not a session worth listing.
                (summary.total_tokens > 0).then_some(summary)
            })
            .collect();
        summaries.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));
        summaries
    }

    /// The 5-hour block `now_ms` is in, when there is one: what
    /// [`crate::identify_blocks`] reports as its last block, if that is active.
    ///
    /// Blocks chain through the history -- a block starts at the first entry
    /// more than 5 hours after the previous block's floored start or after the
    /// previous entry -- but the chain restarts at any idle gap longer than a
    /// block, so the records since the last such gap are all it needs.
    pub fn active_block(&self, now_ms: i64) -> Option<ActiveBlock> {
        active_block_of(&self.store.state.tail, now_ms)
    }

    /// Every counted record since `since_ms`, oldest first, for folding the
    /// active block over more than one store (see [`active_block_of`]). Reads
    /// the rows, unlike everything else here; empty before the first refresh.
    pub fn entries_since(&self, since_ms: i64) -> Vec<BlockEntry> {
        self.store.entries_since(since_ms).unwrap_or_default()
    }
}

/// The 5-hour block `now_ms` is in, over `entries` sorted oldest first: what
/// [`crate::identify_blocks`] reports as its last block, if that is active.
///
/// Blocks chain through the history -- a block starts at the first entry more
/// than 5 hours after the previous block's floored start or after the previous
/// entry -- and the chain restarts at any idle gap longer than a block, so
/// `entries` need only reach back past the last such gap.
pub fn active_block_of(tail: &[BlockEntry], now_ms: i64) -> Option<ActiveBlock> {
    let mut entries = tail.iter();
    let first = entries.next()?.timestamp_ms;
    let (mut start, mut first_entry, mut last) = (floor_to_hour(first), first, first);
    for entry in entries {
        let timestamp = entry.timestamp_ms;
        if timestamp - start > BLOCK_DURATION_MS || timestamp - last > BLOCK_DURATION_MS {
            start = floor_to_hour(timestamp);
            first_entry = timestamp;
        }
        last = timestamp;
    }
    let end = start + BLOCK_DURATION_MS;
    if !(now_ms - last < BLOCK_DURATION_MS && now_ms < end) {
        return None;
    }
    let (mut cost, mut tokens) = (0.0, 0u64);
    for entry in tail.iter().filter(|entry| entry.timestamp_ms >= first_entry) {
        cost += entry.cost;
        tokens = tokens.saturating_add(entry.tokens);
    }
    Some(ActiveBlock { cost, tokens, start_time_ms: start, end_time_ms: end, first_entry_ms: first_entry, last_entry_ms: last })
}
