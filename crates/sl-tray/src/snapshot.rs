//! The tray's pure-Rust view of the world.
//!
//! Everything the menu bar renders is derived here, on the background refresh
//! thread, into a plain owned [`Snapshot`] -- no `objc2` types, no borrows of
//! the [`UsageTotals`], no clock reads (`now_ms` is a parameter). That is what
//! makes the threading split in `main.rs` sound: the background thread owns
//! the only code that touches `sl_usage`, hands the main thread a `Snapshot`
//! through a mutex, and the main thread only ever formats it.
//!
//! It is also the only part of this crate that can be meaningfully unit
//! tested -- the AppKit glue needs a real display and a real menu bar.
//!
//! Two things deliberately stay *out* of here. Anything needing a live clock
//! (the monthly projection, the current block's countdown) is render-time
//! math and lives in [`crate::predictions`], because a snapshot built 29
//! seconds ago must not be the source of a countdown. And nothing here is
//! truncated to "what a menu row shows": the folds keep whole lists (every
//! model, every token category) and the controller decides how many rows fit,
//! so a layout change never needs a new field here.
//!
//! Sizing note: a `Snapshot` is cloned out of the shared mutex on every
//! repaint tick (once a second), so the folds below deliberately reduce
//! thousands of sessions to a handful of numbers plus the five latest rather
//! than parking a `Vec<SessionSummary>` in here to be re-cloned every second.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sl_core::types::{LimitsCapture, RateLimitWindow};
use sl_usage::aggregate::SessionSummary;
use sl_usage::cache::{ActiveBlock, ModelAggregate, TokenTotals, UNKNOWN_MODEL, UsageTotals};
#[cfg(test)]
use sl_usage::{aggregate::{block_burn_rate, identify_blocks, session_summaries}, cache::UsageCache, record::UsageRecord};
// `local_date`, `monday_of_week` and `month_of_date` are `sl_usage`'s own,
// deliberately not reimplemented here: the tray has to land on exactly the
// same day, week and month boundaries as `UsageRecord::date`,
// `weekly_model_aggregates` and `monthly_model_aggregates`, and a second copy
// of that bucketing would drift silently the first time either was edited.
use sl_usage::{local_date, monday_of_week, month_of_date};

use crate::predictions::{cost_per_mtok, output_input_ratio};

/// How many of the latest sessions the Stats window's Sessions table lists. Kept here
/// rather than in the controller so the fold does the truncation once, on the
/// background thread, instead of the repaint doing it every second.
pub const LATEST_SESSIONS: usize = 10;

/// Days of per-day history the Stats window can chart, ending today.
pub const HISTORY_DAYS: usize = 90;

/// One period's folded totals (a day, a week, a month, ...).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub cost: f64,
    /// Every token bucket summed (input, output, cache creation, cache read).
    pub tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Totals {
    fn add(&mut self, aggregate: &ModelAggregate) {
        self.cost += aggregate.cost;
        self.tokens = self.tokens.saturating_add(aggregate.total_tokens());
        self.input_tokens = self.input_tokens.saturating_add(aggregate.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(aggregate.output_tokens);
    }
}

/// All-time usage of one model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    /// The effective model name, or [`UNKNOWN_MODEL`] for records that used
    /// no billable model -- the same key `daily_model_aggregates` buckets on,
    /// so the two views never disagree about what "unknown" covers.
    pub model: String,
    pub cost: f64,
    pub tokens: u64,
    pub output_tokens: u64,
    /// True if *any* record for this model priced at zero because no catalog
    /// entry matched it. Rolled up here because nothing in `ModelAggregate`
    /// carries it, and the plan is explicit that an unpriced future model must
    /// never silently under-report: the Stats window's Models table marks these rows.
    pub missing_pricing: bool,
}

/// Coarse cost tier, used only to group the Stats window's Models table.
///
/// Substring matching on the model name, not a catalog lookup: the point is a
/// stable four-bucket grouping that keeps working when a model nobody has
/// heard of yet appears, not an authoritative classification. Anything
/// unmatched lands in [`ModelTier::Unknown`] rather than being hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    Frontier,
    Premium,
    Standard,
    Fast,
    /// Any OpenAI model (Codex's), by family rather than by price.
    OpenAi,
    Unknown,
}

impl ModelTier {

    /// The tier of a model name. Case-insensitive substring match, checked
    /// most-capable-first so a hypothetical `claude-fable-5-haiku` reads as
    /// Frontier rather than Fast.
    pub fn of(model: &str) -> Self {
        let name = model.to_ascii_lowercase();
        let has = |needle: &str| name.contains(needle);
        if has("fable") || has("mythos") {
            Self::Frontier
        } else if has("opus") {
            Self::Premium
        } else if has("sonnet") {
            Self::Standard
        } else if has("haiku") {
            Self::Fast
        } else if has("gpt") || has("codex") {
            Self::OpenAi
        } else {
            Self::Unknown
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Frontier => "Frontier",
            Self::Premium => "Premium",
            Self::Standard => "Standard",
            Self::Fast => "Fast",
            Self::OpenAi => "OpenAI",
            Self::Unknown => "Unknown",
        }
    }
}

/// The cheapest model per unit of output, and what that costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EfficientModel {
    pub model: String,
    /// Total cost (input, output and cache alike) divided by output tokens
    /// alone, scaled to a million -- the unit model pricing is quoted in.
    pub cost_per_output_mtok: f64,
}

/// One row of the Stats window's "Latest" sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestSession {
    pub session_id: String,
    /// The transcript directory's project, with the home directory taken off
    /// the front: `Developer-statusline` for `-Users-<you>-Developer-statusline`.
    pub project: String,
    pub cost: f64,
    pub total_tokens: u64,
    pub messages: usize,
    pub first_seen_ms: i64,
    pub last_activity_ms: i64,
}

/// The Stats window's Sessions section, folded from every session summary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub count: usize,
    pub avg_cost: f64,
    pub avg_tokens: u64,
    /// Output tokens over input tokens, across every session at once (not the
    /// mean of each session's own ratio, which a single tiny session with two
    /// input tokens could dominate). `None` when nothing recorded any input.
    pub output_input_ratio: Option<f64>,
    /// Every session's cost over every session's output tokens, per million
    /// output tokens. `None` when no session produced output.
    pub cost_per_output_mtok: Option<f64>,
    pub most_efficient: Option<EfficientModel>,
    /// The [`LATEST_SESSIONS`] most recently active sessions, newest first.
    pub latest: Vec<LatestSession>,
    /// The [`LATEST_SESSIONS`] most expensive sessions, costliest first.
    pub top: Vec<LatestSession>,
}

/// The active 5-hour block, as of the snapshot's `now_ms`.
///
/// Only the numbers a projection needs: the countdown and the projected total
/// are computed at render time from the *live* clock ([`crate::predictions`]),
/// not baked in here where they would be up to a refresh interval stale.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentBlock {
    pub cost_so_far: f64,
    /// `None` for a block that cannot have a rate yet -- a single entry, or
    /// every entry sharing one timestamp (see `sl_usage`'s `block_burn_rate`).
    pub cost_per_hour: Option<f64>,
    /// When the 5-hour window closes (`start_time_ms + 5h`).
    pub end_time_ms: i64,
    /// The block's first entry's timestamp (not the floored window start).
    pub started_at_ms: i64,
    pub tokens: u64,
}

/// One rate-limit window, as the status-line hook captured it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    /// Percent of the window used, 0-100. Claude Code 2.1.258 sends this as
    /// `used_percentage`; older builds sent `utilization`, and the fold reads
    /// them in the same order `statusline`'s own renderer does so the menu bar
    /// and the status line can never disagree about the number.
    pub used_percent: f64,
    /// When the window rolls over, epoch ms. `None` when the payload carried
    /// no reset time or one that would not parse -- which makes the projection
    /// unavailable rather than wrong, since the window start is derived from
    /// this and nothing else.
    pub resets_at_ms: Option<i64>,
    /// How long the window is, when its capture says (Codex's does; Claude
    /// Code's windows are the fixed 5 hours and 7 days).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
}

/// The status-line hook's rate-limit capture, folded for the title.
///
/// Present only when there *is* a capture: the tray can perfectly well run
/// before Claude Code has ever written one, and an all-zero window would read
/// as "0% used" rather than "no data", which is a lie the menu bar would tell
/// once per second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsSnapshot {
    /// Epoch ms the hook wrote the capture.
    ///
    /// A different clock from [`Snapshot::generated_at_ms`], and the two must
    /// not be conflated: the snapshot time says when the tray last folded the
    /// usage corpus (every 30s, forever), this says when Claude Code last told
    /// anybody about the rate limits (only while it is running).
    pub captured_at_ms: i64,
    pub five_hour: Option<LimitWindow>,
    pub seven_day: Option<LimitWindow>,
}

/// Everything the menu bar item and its menu draw from, as of
/// [`generated_at_ms`](Snapshot::generated_at_ms).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Epoch-ms the snapshot was built at. The menu renders "updated Ns ago"
    /// from this, which doubles as the visible proof that the timer is still
    /// firing while the menu is open.
    pub generated_at_ms: i64,
    /// Local calendar date (`YYYY-MM-DD`) `today` refers to.
    pub today_date: String,
    /// Monday (`YYYY-MM-DD`) starting the ISO week `week` refers to.
    pub week_start_date: String,
    /// `YYYY-MM` `month` refers to.
    pub month_date: String,
    /// The calendar day before [`today_date`](Snapshot::today_date), which
    /// `prev_day` refers to. The day-over-day baseline is the previous
    /// *calendar* day, not the previous day that had usage: "nothing
    /// yesterday" is a real, informative comparison.
    pub prev_day_date: String,
    /// The Monday before [`week_start_date`](Snapshot::week_start_date).
    pub prev_week_start_date: String,
    pub today: Totals,
    pub week: Totals,
    pub month: Totals,
    pub prev_day: Totals,
    pub prev_week: Totals,
    /// All-time cost across the whole corpus.
    pub total_cost: f64,
    /// Total records the cache held, across all time. Cheap sanity signal for
    /// "did the background thread actually load anything".
    pub record_count: usize,
    /// Every model ever used, descending by cost (ties broken by name, so the
    /// order is stable between refreshes and the menu does not shuffle).
    pub models: Vec<ModelUsage>,
    /// All-time tokens by category, descending. Categories with no tokens at
    /// all are omitted rather than rendered as zero rows.
    pub token_breakdown: Vec<(String, u64)>,
    pub sessions: SessionStats,
    /// `None` when no block is active -- idle for over 5 hours, or no usage
    /// at all.
    pub current_block: Option<CurrentBlock>,
    /// The status-line hook's rate-limit capture, or `None` when there is no
    /// usable one. Unlike everything else here this does not come from the
    /// usage corpus at all -- see [`crate::limits`].
    pub rate_limits: Option<LimitsSnapshot>,
    /// Codex's rate limits as last seen in its rollouts (see
    /// [`crate::limits::CodexLimits`]), on the Codex and All snapshots.
    /// `rate_limits` stays Claude Code's: under All both are shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_limits: Option<LimitsSnapshot>,
    /// The last [`HISTORY_DAYS`] calendar days, oldest first, ending today.
    /// Days with no usage are present with zeroes: a chart's x axis is time,
    /// and a missing day would close the gap it should show.
    pub history: Vec<DayUsage>,
}

/// One calendar day's usage, for the Stats window's charts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayUsage {
    /// `YYYY-MM-DD`, local.
    pub date: String,
    pub cost: f64,
    pub tokens: u64,
    pub messages: u64,
    /// Per model, costliest first.
    pub models: Vec<DayModel>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayModel {
    pub model: String,
    pub cost: f64,
    pub tokens: u64,
}

impl Snapshot {
    /// An all-zero snapshot for `now_ms`, used before the first background
    /// refresh completes.
    pub fn empty(now_ms: i64) -> Self {
        let today_date = local_date(now_ms);
        let week_start_date = monday_of_week(&today_date);
        Self {
            generated_at_ms: now_ms,
            month_date: month_of_date(&today_date),
            prev_day_date: shift_days(&today_date, -1),
            prev_week_start_date: shift_days(&week_start_date, -7),
            history: fold_history(&HashMap::new(), &today_date),
            today_date,
            week_start_date,
            ..Self::default()
        }
    }
}

/// `date` shifted by `delta` calendar days, as `YYYY-MM-DD`.
///
/// Calendar arithmetic via `chrono`, not `+/- 86_400_000` on a timestamp: the
/// day before a DST spring-forward is 23 hours earlier, and the previous
/// *date* is what the day-over-day comparison is keyed on either way.
fn shift_days(date: &str, delta: i64) -> String {
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(parsed) => (parsed + chrono::Duration::days(delta)).format("%Y-%m-%d").to_string(),
        Err(_) => date.to_string(),
    }
}

/// Fold a loaded cache and Claude Code's session metadata into a [`Snapshot`]
/// as of `now_ms`.
///
/// Buckets by the record's own local calendar date (`UsageRecord::date`), the
/// same key `sl_usage`'s daily/weekly aggregations use, so the tray and the
/// statusline agree on where a day boundary falls.
///
/// `capture` is passed in rather than loaded here (it reads the
/// filesystem) so this function stays a pure fold over data the caller
/// supplies -- which is what makes it testable against a fixture corpus.
#[cfg(test)]
pub fn build_snapshot(totals: &UsageTotals, capture: Option<&LimitsCapture>, now_ms: i64) -> Snapshot {
    assemble(fold_totals(totals, now_ms).folded, capture, now_ms)
}

/// What a snapshot is folded from, whichever cache it came out of: the
/// record-level [`UsageCache`] (the oracle the tests compare against) or the
/// totals [`UsageTotals`] the tray runs on.
struct Folded {
    daily: HashMap<String, HashMap<String, ModelAggregate>>,
    models: Vec<ModelUsage>,
    token_breakdown: Vec<(String, u64)>,
    current_block: Option<CurrentBlock>,
    sessions: SessionStats,
    total_cost: f64,
    record_count: usize,
}

/// The same snapshot out of the record-level cache. Test-only: it is what the
/// totals path must agree with.
#[cfg(test)]
fn build_snapshot_from_records(cache: &UsageCache, capture: Option<&LimitsCapture>, now_ms: i64) -> Snapshot {
    let records: Vec<&UsageRecord> = cache.all_records().collect();
    let models = fold_models(&records);
    let sessions = fold_sessions(&session_summaries(cache), &models);
    assemble(
        Folded {
            daily: cache.daily_model_aggregates(),
            token_breakdown: fold_token_breakdown(&records),
            current_block: fold_current_block(&records, now_ms),
            total_cost: cache.total_cost(),
            record_count: cache.record_count(),
            models,
            sessions,
        },
        capture,
        now_ms,
    )
}

fn assemble(folded: Folded, capture: Option<&LimitsCapture>, now_ms: i64) -> Snapshot {
    let today_date = local_date(now_ms);
    let week_start_date = monday_of_week(&today_date);
    let month_date = month_of_date(&today_date);
    let prev_day_date = shift_days(&today_date, -1);
    let prev_week_start_date = shift_days(&week_start_date, -7);

    let history = fold_history(&folded.daily, &today_date);
    // Today, this week, this month, yesterday, last week. A single date lands
    // in several at once -- today is always also in this week and in this
    // month -- so each is tested on its own, not as an `else` chain.
    let mut buckets = [Totals::default(), Totals::default(), Totals::default(), Totals::default(), Totals::default()];
    for (date, by_model) in folded.daily {
        let week_of_date = monday_of_week(&date);
        let holds = [date == today_date, week_of_date == week_start_date, month_of_date(&date) == month_date, date == prev_day_date, week_of_date == prev_week_start_date];
        for (bucket, _) in buckets.iter_mut().zip(holds).filter(|(_, holds)| *holds) {
            for aggregate in by_model.values() {
                bucket.add(aggregate);
            }
        }
    }
    let [today, week, month, prev_day, prev_week] = buckets;

    Snapshot {
        generated_at_ms: now_ms,
        today_date,
        week_start_date,
        month_date,
        prev_day_date,
        prev_week_start_date,
        today,
        week,
        month,
        prev_day,
        prev_week,
        total_cost: folded.total_cost,
        record_count: folded.record_count,
        models: folded.models,
        token_breakdown: folded.token_breakdown,
        sessions: folded.sessions,
        current_block: folded.current_block,
        rate_limits: fold_rate_limits(capture),
        codex_limits: None,
        history,
    }
}

/// The three views the provider choice picks between, built together each
/// refresh so a switch shows at once. `codex` and `all` are `None` when no
/// Codex home exists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshots {
    pub claude: Snapshot,
    pub codex: Option<Snapshot>,
    pub all: Option<Snapshot>,
}

impl Snapshots {
    pub fn empty(now_ms: i64) -> Self {
        Self { claude: Snapshot::empty(now_ms), codex: None, all: None }
    }

    /// The view for `provider`; Claude Code's when that one was not built.
    pub fn select(&self, provider: crate::view_prefs::Provider) -> &Snapshot {
        use crate::view_prefs::Provider;
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => self.codex.as_ref().unwrap_or(&self.claude),
            Provider::All => self.all.as_ref().unwrap_or(&self.claude),
        }
    }
}

/// How far back All's active block looks for both stores' records. The block
/// chain restarts at any idle gap over five hours, so this is exact unless the
/// two together ran without one for this long.
const MERGED_BLOCK_LOOKBACK_MS: i64 = 48 * 60 * 60 * 1000;

/// Claude Code's snapshot, and when Codex's store is given, Codex's and both
/// merged. Each folds its own store; All merges the stores' rollups and full
/// session summaries *before* folding, so its averages and lists are over every
/// session rather than averaged from the two snapshots.
pub fn build_snapshots(
    claude: &UsageTotals,
    codex: Option<&UsageTotals>,
    capture: Option<&LimitsCapture>,
    codex_limits: Option<LimitsSnapshot>,
    now_ms: i64,
) -> Snapshots {
    let claude_folded = fold_totals(claude, now_ms);
    let Some(codex) = codex else {
        return Snapshots { claude: assemble(claude_folded.folded, capture, now_ms), codex: None, all: None };
    };
    let codex_folded = fold_totals(codex, now_ms);

    let merged = merge_folded(&claude_folded, &codex_folded, {
        let since = now_ms - MERGED_BLOCK_LOOKBACK_MS;
        let mut entries = claude.entries_since(since);
        entries.extend(codex.entries_since(since));
        entries.sort_by_key(|entry| entry.timestamp_ms);
        sl_usage::cache::active_block_of(&entries, now_ms).map(current_block_of)
    });
    let mut all = assemble(merged, capture, now_ms);
    all.codex_limits = codex_limits;
    let mut codex_snapshot = assemble(codex_folded.folded, None, now_ms);
    codex_snapshot.codex_limits = codex_limits;
    Snapshots { claude: assemble(claude_folded.folded, capture, now_ms), codex: Some(codex_snapshot), all: Some(all) }
}

/// One store's fold, with the full session summaries kept for a merge.
struct StoreFold {
    folded: Folded,
    summaries: Vec<SessionSummary>,
    tokens: TokenTotals,
}

fn fold_totals(totals: &UsageTotals, now_ms: i64) -> StoreFold {
    let models = rank_models(
        totals
            .model_totals()
            .into_iter()
            .map(|(model, totals)| ModelUsage {
                model,
                cost: totals.cost,
                tokens: totals.tokens,
                output_tokens: totals.output_tokens,
                missing_pricing: totals.unpriced,
            })
            .collect(),
    );
    let summaries = totals.session_summaries();
    let tokens = totals.token_totals();
    StoreFold {
        folded: Folded {
            daily: totals.daily_model_aggregates(),
            token_breakdown: rank_token_breakdown(tokens),
            current_block: totals.active_block(now_ms).map(current_block_of),
            total_cost: totals.total_cost(),
            record_count: totals.record_count(),
            sessions: fold_sessions(&summaries, &models),
            models,
        },
        summaries,
        tokens,
    }
}

/// Two stores' folds as one: day and model rollups summed (the two never share
/// a model name, but summing is right if they ever did), every session summary
/// folded together, and `current_block` as given.
fn merge_folded(a: &StoreFold, b: &StoreFold, current_block: Option<CurrentBlock>) -> Folded {
    let mut daily = a.folded.daily.clone();
    for (date, by_model) in &b.folded.daily {
        let day = daily.entry(date.clone()).or_default();
        for (model, aggregate) in by_model {
            *day.entry(model.clone()).or_default() += *aggregate;
        }
    }
    let mut models: HashMap<String, ModelUsage> = HashMap::new();
    for usage in a.folded.models.iter().chain(&b.folded.models) {
        let merged = models.entry(usage.model.clone()).or_insert_with(|| ModelUsage { model: usage.model.clone(), ..ModelUsage::default() });
        merged.cost += usage.cost;
        merged.tokens = merged.tokens.saturating_add(usage.tokens);
        merged.output_tokens = merged.output_tokens.saturating_add(usage.output_tokens);
        merged.missing_pricing |= usage.missing_pricing;
    }
    let models = rank_models(models.into_values().collect());
    let mut summaries: Vec<SessionSummary> = a.summaries.iter().chain(&b.summaries).cloned().collect();
    // `fold_sessions` takes them costliest first, as one store hands them over.
    summaries.sort_by(|x, y| y.cost.partial_cmp(&x.cost).unwrap_or(std::cmp::Ordering::Equal));
    let tokens = TokenTotals {
        input: a.tokens.input.saturating_add(b.tokens.input),
        output: a.tokens.output.saturating_add(b.tokens.output),
        cache_write: a.tokens.cache_write.saturating_add(b.tokens.cache_write),
        cache_read: a.tokens.cache_read.saturating_add(b.tokens.cache_read),
    };
    Folded {
        daily,
        token_breakdown: rank_token_breakdown(tokens),
        current_block,
        total_cost: a.folded.total_cost + b.folded.total_cost,
        record_count: a.folded.record_count + b.folded.record_count,
        sessions: fold_sessions(&summaries, &models),
        models,
    }
}

/// [`HISTORY_DAYS`] of per-day usage ending at `today`, zero-filled.
fn fold_history(daily: &HashMap<String, HashMap<String, ModelAggregate>>, today: &str) -> Vec<DayUsage> {
    (0..i64::try_from(HISTORY_DAYS).unwrap_or(i64::MAX))
        .rev()
        .map(|back| {
            let date = shift_days(today, -back);
            let mut day = DayUsage { date, ..DayUsage::default() };
            if let Some(by_model) = daily.get(&day.date) {
                for (model, aggregate) in by_model {
                    day.cost += aggregate.cost;
                    day.tokens = day.tokens.saturating_add(aggregate.total_tokens());
                    day.messages = day.messages.saturating_add(aggregate.messages);
                    day.models.push(DayModel { model: model.clone(), cost: aggregate.cost, tokens: aggregate.total_tokens() });
                }
                day.models.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.model.cmp(&b.model)));
            }
            day
        })
        .collect()
}

/// Fold a capture into the two windows the menu draws.
///
/// Everything is required: a capture with no timestamp cannot be aged, and one
/// with no `rate_limits` has nothing to show, and in both cases the honest
/// answer is `None` rather than a window full of defaults.
pub(crate) fn fold_rate_limits(capture: Option<&LimitsCapture>) -> Option<LimitsSnapshot> {
    let capture = capture?;
    let captured_at_ms = capture.captured_at_ms?;
    let rate_limits = capture.rate_limits.as_ref()?;
    Some(LimitsSnapshot {
        captured_at_ms,
        five_hour: rate_limits.five_hour.as_ref().and_then(fold_limit_window),
        seven_day: rate_limits.seven_day.as_ref().and_then(fold_limit_window),
    })
}

/// One captured window, or `None` when it carries no percentage at all --
/// which is the only field the title cannot do without.
fn fold_limit_window(window: &RateLimitWindow) -> Option<LimitWindow> {
    // `used_percentage` first, then `utilization`: the same precedence
    // `statusline::render::rate_limit_segment` applies to the same payload.
    let used_percent = window.used_percent()?;
    Some(LimitWindow { used_percent, resets_at_ms: window.resets_at.as_ref().and_then(sl_core::format::resets_at_ms), window_minutes: None })
}

/// Order a model list: cost descending, name ascending on a tie. Two models
/// with identical cost (very common at $0.00 on an unpriced corpus) must not
/// swap places between refreshes, or the menu reorders itself under the cursor.
///
/// The models are keyed the same way `daily_model_aggregates` keys its inner
/// map (effective model name, `UNKNOWN_MODEL` when there is none), and carry a
/// rolled-up `missing_pricing`, which no existing aggregate does.
fn rank_models(mut models: Vec<ModelUsage>) -> Vec<ModelUsage> {
    models.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.model.cmp(&b.model)));
    models
}

/// All-time per-model usage out of the records themselves. Test-only: the
/// oracle for what [`UsageTotals::model_totals`] must add up to.
#[cfg(test)]
fn fold_models(records: &[&UsageRecord]) -> Vec<ModelUsage> {
    let mut by_model: HashMap<&str, ModelUsage> = HashMap::new();
    for record in records {
        let name = record.model.as_deref().unwrap_or(UNKNOWN_MODEL);
        let usage = by_model.entry(name).or_insert_with(|| ModelUsage { model: name.to_owned(), ..ModelUsage::default() });
        usage.cost += record.cost;
        usage.tokens = usage.tokens.saturating_add(record.total_tokens());
        usage.output_tokens = usage.output_tokens.saturating_add(record.output_tokens);
        usage.missing_pricing |= record.missing_pricing;
    }
    rank_models(by_model.into_values().collect())
}

/// All-time tokens by category, descending, omitting empty categories.
///
/// The two cache-creation buckets (`5m`/`1h` ephemeral) are summed into one
/// "Cache Write" category: they are priced differently but are the same thing
/// to a reader looking at where their tokens went, and four categories is
/// already one more than the three rows the Costs submenu shows.
fn rank_token_breakdown(totals: TokenTotals) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> =
        [("Input", totals.input), ("Output", totals.output), ("Cache Write", totals.cache_write), ("Cache Read", totals.cache_read)]
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(name, count)| (name.to_owned(), count))
            .collect();
    // Same stable-order rule as `rank_models`.
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// [`rank_token_breakdown`] over the records themselves. Test-only oracle.
#[cfg(test)]
fn fold_token_breakdown(records: &[&UsageRecord]) -> Vec<(String, u64)> {
    let mut totals = TokenTotals::default();
    for record in records {
        totals.input = totals.input.saturating_add(record.input_tokens);
        totals.output = totals.output.saturating_add(record.output_tokens);
        totals.cache_write = totals.cache_write.saturating_add(record.cache_creation_tokens());
        totals.cache_read = totals.cache_read.saturating_add(record.cache_read_tokens);
    }
    rank_token_breakdown(totals)
}

/// The active block as the snapshot carries it. `started_at_ms` is the first
/// entry's timestamp, not the floored window start.
fn current_block_of(block: ActiveBlock) -> CurrentBlock {
    CurrentBlock {
        cost_so_far: block.cost,
        cost_per_hour: block.cost_per_hour(),
        end_time_ms: block.end_time_ms,
        started_at_ms: block.first_entry_ms,
        tokens: block.tokens,
    }
}

/// The active block out of the records themselves. Test-only oracle for
/// [`UsageTotals::active_block`].
///
/// `identify_blocks` returns blocks in chronological order and at most the
/// last one can be active (activity is "within 5 hours of `now_ms`"), but the
/// search is written as a reverse scan for exactly that reason rather than
/// assuming the invariant.
#[cfg(test)]
fn fold_current_block(records: &[&UsageRecord], now_ms: i64) -> Option<CurrentBlock> {
    let block = identify_blocks(records, now_ms).into_iter().rev().find(|block| block.is_active && !block.is_gap)?;
    let tokens = block
        .input_tokens
        .saturating_add(block.output_tokens)
        .saturating_add(block.cache_creation_5m_tokens)
        .saturating_add(block.cache_creation_1h_tokens)
        .saturating_add(block.cache_read_tokens);
    Some(CurrentBlock {
        cost_so_far: block.cost,
        cost_per_hour: block_burn_rate(&block).map(|rate| rate.cost_per_hour),
        end_time_ms: block.end_time_ms,
        started_at_ms: block.first_entry_time_ms.unwrap_or(block.start_time_ms),
        tokens,
    })
}

/// Fold every session summary into the Stats window's session numbers.
fn fold_sessions(summaries: &[SessionSummary], models: &[ModelUsage]) -> SessionStats {
    let count = summaries.len();
    let (mut cost, mut tokens, mut input_tokens, mut output_tokens) = (0.0, 0u64, 0u64, 0u64);
    for summary in summaries {
        cost += summary.cost;
        tokens = tokens.saturating_add(summary.total_tokens);
        input_tokens = input_tokens.saturating_add(summary.input_tokens);
        output_tokens = output_tokens.saturating_add(summary.output_tokens);
    }

    let mut latest: Vec<&SessionSummary> = summaries.iter().collect();
    // Newest first; session id ascending on a tie so the list is stable.
    latest.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then_with(|| a.session_id.cmp(&b.session_id)));

    SessionStats {
        count,
        avg_cost: if count == 0 { 0.0 } else { cost / count as f64 },
        avg_tokens: if count == 0 { 0 } else { tokens / count as u64 },
        output_input_ratio: output_input_ratio(output_tokens, input_tokens),
        cost_per_output_mtok: cost_per_mtok(cost, output_tokens),
        most_efficient: most_efficient_model(models),
        latest: latest.into_iter().take(LATEST_SESSIONS).map(listed_session).collect(),
        // `summaries` arrive costliest first.
        top: summaries.iter().take(LATEST_SESSIONS).map(listed_session).collect(),
    }
}

fn listed_session(summary: &SessionSummary) -> LatestSession {
    LatestSession {
        session_id: summary.session_id.clone(),
        project: short_project(&summary.project_path),
        cost: summary.cost,
        total_tokens: summary.total_tokens,
        messages: summary.message_count,
        first_seen_ms: summary.first_seen,
        last_activity_ms: summary.last_activity,
    }
}

/// Claude Code names a project's transcript directory after its path with
/// every `/` turned into `-`; the home directory's share of that is noise.
/// A Codex session's project is the real path, which is shortened the same
/// way so one project reads alike under both.
fn short_project(project_path: &str) -> String {
    let encoded;
    let project_path = if project_path.starts_with('/') {
        encoded = project_path.replace(['/', '.'], "-");
        encoded.as_str()
    } else {
        project_path
    };
    let home = dirs::home_dir().map(|home| home.to_string_lossy().replace(['/', '.'], "-")).unwrap_or_default();
    match project_path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && !rest.is_empty() => rest.trim_start_matches('-').to_owned(),
        _ => project_path.to_owned(),
    }
}

/// The model with the lowest all-time cost per million output tokens.
///
/// Definition, since the plan only says "most efficient model": cost is
/// attributed at the *record* level (which is what `models` already is), not
/// by splitting a session's cost across the models it used -- most real
/// sessions mix an Opus-tier model with a Fast one for subagents, and any
/// per-session attribution would either double-count that cost or discard the
/// session entirely. "Across sessions" therefore means "across the whole
/// corpus", which is the same set of records with none thrown away.
///
/// Three exclusions, each of which would otherwise produce a nonsense winner:
///   * [`UNKNOWN_MODEL`] -- not a model, and not something to recommend.
///   * anything flagged `missing_pricing` -- its cost is $0 because nothing
///     priced it, so it would win every comparison by default. This is the
///     load-bearing one.
///   * zero output tokens, or zero cost -- the ratio is undefined or degenerate.
///
/// Tie-break: cheaper first, then more output tokens (the better-evidenced
/// number wins), then name ascending for a stable result.
fn most_efficient_model(models: &[ModelUsage]) -> Option<EfficientModel> {
    models
        .iter()
        .filter(|usage| usage.model != UNKNOWN_MODEL && !usage.missing_pricing && usage.cost > 0.0)
        .filter_map(|usage| cost_per_mtok(usage.cost, usage.output_tokens).map(|rate| (usage, rate)))
        .min_by(|(a, a_rate), (b, b_rate)| {
            a_rate
                .partial_cmp(b_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.output_tokens.cmp(&a.output_tokens))
                .then_with(|| a.model.cmp(&b.model))
        })
        .map(|(usage, rate)| EfficientModel { model: usage.model.clone(), cost_per_output_mtok: rate })
}

/// The environment variable that pins [`now_ms`] to one instant, so the
/// self-test and the view dumps come out the same on every run.
pub(crate) const NOW_ENV: &str = "SL_TRAY_NOW_MS";

/// Epoch-ms now. The one clock read in the crate, kept out of every function
/// that has anything to compute. Fixed under [`NOW_ENV`].
pub fn now_ms() -> i64 {
    static FIXED: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    if let Some(fixed) = *FIXED.get_or_init(|| std::env::var(NOW_ENV).ok().and_then(|value| value.parse().ok())) {
        return fixed;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use super::*;

    use std::fs;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    use chrono::{Local, NaiveDate, TimeZone, Utc};
    use sl_usage::cache::RefreshMode;
    use sl_usage::{PricingMap, usage_files};

    fn pricing() -> &'static PricingMap {
        static MAP: OnceLock<PricingMap> = OnceLock::new();
        MAP.get_or_init(PricingMap::new)
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A throwaway `projects/` corpus plus a throwaway store dir, so nothing
    /// here touches the real `~/.claude` tree or the real store. Same shape as
    /// `sl-usage`'s own `tests/cache.rs` fixture.
    struct Fixture {
        root: PathBuf,
        cache_dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let base = std::env::temp_dir().join(format!("sl-tray-snapshot-{}-{name}-{n}", std::process::id()));
            fs::remove_dir_all(&base).ok();
            let root = base.join("corpus");
            let cache_dir = base.join("cache");
            fs::create_dir_all(root.join("projects")).unwrap();
            fs::create_dir_all(&cache_dir).unwrap();
            Self { root, cache_dir }
        }

        fn write_session(&self, session: &str, lines: &str) {
            let dir = self.root.join("projects").join("proj");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{session}.jsonl")), lines).unwrap();
        }

        /// The record-level oracle over the same files.
        fn cache(&self) -> UsageCache {
            UsageCache::from_files(&usage_files(std::slice::from_ref(&self.root)), pricing())
        }

        /// The totals the tray runs on, over the same files.
        fn totals(&self) -> UsageTotals {
            let files = usage_files(std::slice::from_ref(&self.root));
            let mut totals = UsageTotals::empty(&self.cache_dir);
            totals.refresh_files(&files, pricing(), RefreshMode::Incremental);
            totals
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(base) = self.root.parent() {
                fs::remove_dir_all(base).ok();
            }
        }
    }

    /// Local noon on `date`, as epoch-ms. Noon (rather than midnight) so the
    /// timestamp lands unambiguously inside the local day regardless of DST
    /// transitions, which is what `UsageRecord::date` buckets on.
    fn local_noon_ms(date: NaiveDate) -> i64 {
        Local
            .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .expect("noon exists in every timezone")
            .timestamp_millis()
    }

    /// One JSONL usage line at `ts_ms`, with `input`/`output` tokens and a
    /// model with no pricing entry (so `cost` stays 0 and the tests assert on
    /// token counts, which are exact, rather than on drifting price data).
    fn line(ts_ms: i64, message_id: &str, input: u64, output: u64) -> String {
        let ts = Utc.timestamp_millis_opt(ts_ms).single().unwrap().to_rfc3339();
        format!(
            r#"{{"timestamp":"{ts}","sessionId":"sess","requestId":"{message_id}-req","message":{{"id":"{message_id}","model":"test-model","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
        )
    }

    fn capture(json: &str) -> LimitsCapture {
        serde_json::from_str(json).expect("fixture capture parses")
    }

    #[test]
    fn no_capture_folds_to_no_rate_limits() {
        assert_eq!(fold_rate_limits(None), None);
        // A capture that parsed but says nothing is still nothing: an all-zero
        // window would render as "0% used", which is a different claim.
        assert_eq!(fold_rate_limits(Some(&capture(r#"{"captured_at_ms": 42}"#))), None);
        assert_eq!(fold_rate_limits(Some(&capture(r#"{"rate_limits": {"five_hour": {"used_percentage": 7}}}"#))), None, "undateable");
    }

    #[test]
    fn a_real_capture_folds_both_windows() {
        // The shape Claude Code 2.1.258 writes: `used_percentage` and epoch
        // *seconds*.
        let folded = fold_rate_limits(Some(&capture(
            r#"{"captured_at_ms": 1788200392112,
                "rate_limits": {"five_hour": {"used_percentage": 12.5, "resets_at": 1787580599},
                                "seven_day": {"used_percentage": 15.0, "resets_at": 1787889599}}}"#,
        )))
        .expect("folds");

        assert_eq!(folded.captured_at_ms, 1_788_200_392_112);
        assert_eq!(folded.five_hour, Some(LimitWindow { used_percent: 12.5, resets_at_ms: Some(1_787_580_599_000), window_minutes: None }));
        assert_eq!(folded.seven_day, Some(LimitWindow { used_percent: 15.0, resets_at_ms: Some(1_787_889_599_000), window_minutes: None }));
    }

    #[test]
    fn the_older_utilization_and_iso_shape_folds_the_same_way() {
        let folded = fold_rate_limits(Some(&capture(
            r#"{"captured_at_ms": 1, "rate_limits": {"five_hour": {"utilization": 12,
                "resets_at": "2026-08-24T14:09:59.911461+00:00"}}}"#,
        )))
        .expect("folds");
        assert_eq!(folded.five_hour, Some(LimitWindow { used_percent: 12.0, resets_at_ms: Some(1_787_580_599_911), window_minutes: None }));
        assert_eq!(folded.seven_day, None, "a window the payload omits stays omitted");
    }

    #[test]
    fn a_window_with_no_percentage_or_an_unparseable_reset_degrades_rather_than_lies() {
        let folded = fold_rate_limits(Some(&capture(
            r#"{"captured_at_ms": 1, "rate_limits": {"five_hour": {"resets_at": 1787580599},
                                                     "seven_day": {"used_percentage": 15.0, "resets_at": "not a date"}}}"#,
        )))
        .expect("folds");
        assert_eq!(folded.five_hour, None, "no percentage means no window, not 0%");
        // The percentage survives; only the projection is lost with the reset.
        assert_eq!(folded.seven_day, Some(LimitWindow { used_percent: 15.0, resets_at_ms: None, window_minutes: None }));
    }

    #[test]
    fn build_snapshot_carries_the_capture_through() {
        let fixture = Fixture::new("limits");
        let capture = capture(r#"{"captured_at_ms": 7, "rate_limits": {"five_hour": {"used_percentage": 3}}}"#);
        let snapshot = build_snapshot(&fixture.totals(), Some(&capture), now_ms());
        assert_eq!(snapshot.rate_limits.expect("carried through").captured_at_ms, 7);
        assert_eq!(build_snapshot(&fixture.totals(), None, now_ms()).rate_limits, None);
    }

    #[test]
    fn empty_cache_yields_a_zeroed_snapshot() {
        let fixture = Fixture::new("empty");
        let now = now_ms();
        let snapshot = build_snapshot(&fixture.totals(), None, now);

        assert_eq!(snapshot.today, Totals::default());
        assert_eq!(snapshot.week, Totals::default());
        assert_eq!(snapshot.record_count, 0);
        assert_eq!(snapshot.generated_at_ms, now);
        assert_eq!(snapshot.today_date, local_date(now));
    }

    #[test]
    fn today_and_week_totals_fold_the_right_days() {
        // Anchor on a Wednesday so "earlier this week" and "last week" are
        // both expressible without crossing the week boundary by accident.
        let today = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(); // Wednesday
        let monday = NaiveDate::from_ymd_opt(2026, 8, 24).unwrap();
        let last_week = NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(); // previous Thursday
        let now = local_noon_ms(today);

        let fixture = Fixture::new("totals");
        fixture.write_session(
            "sess",
            &format!(
                "{}\n{}\n{}\n{}\n",
                line(now, "msg-today-a", 10, 5),
                line(now + 60_000, "msg-today-b", 1, 4),
                line(local_noon_ms(monday), "msg-monday", 100, 0),
                line(local_noon_ms(last_week), "msg-lastweek", 1_000, 0),
            ),
        );

        let snapshot = build_snapshot(&fixture.totals(), None, now);

        assert_eq!(snapshot.today_date, "2026-08-26");
        assert_eq!(snapshot.week_start_date, "2026-08-24");
        assert_eq!(snapshot.record_count, 4, "all four records are cached, only some are in-period");
        assert_eq!(snapshot.today.tokens, 20, "10+5 and 1+4 from today only");
        assert_eq!(snapshot.week.tokens, 120, "today's 20 plus Monday's 100, excluding last week's 1000");
    }

    #[test]
    fn a_day_that_is_today_is_also_counted_in_the_week() {
        // Guards the `if !is_today && !is_this_week { continue }` fold: today
        // must land in both buckets, never be double-counted in either.
        let today = NaiveDate::from_ymd_opt(2026, 8, 24).unwrap(); // a Monday: today == week start
        let now = local_noon_ms(today);

        let fixture = Fixture::new("monday");
        fixture.write_session("sess", &format!("{}\n", line(now, "msg-1", 7, 3)));

        let snapshot = build_snapshot(&fixture.totals(), None, now);
        assert_eq!(snapshot.today_date, snapshot.week_start_date);
        assert_eq!(snapshot.today.tokens, 10);
        assert_eq!(snapshot.week.tokens, 10);
    }

    /// Not a test of `sl_usage::monday_of_week` (that crate tests its own
    /// bucketing); a test that `build_snapshot` inherits Monday-start weeks
    /// from it. If someone reintroduces a local copy with Sunday-start or
    /// naive day arithmetic, `week_start_date` moves and this fails.
    #[test]
    fn week_bucketing_is_monday_start_across_a_month_boundary() {
        let week_start_for = |date: &str| {
            let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
            let fixture = Fixture::new("weekstart");
            build_snapshot(&fixture.totals(), None, local_noon_ms(day)).week_start_date
        };
        assert_eq!(week_start_for("2026-08-24"), "2026-08-24", "a Monday is its own week start");
        assert_eq!(week_start_for("2026-08-30"), "2026-08-24", "Sunday belongs to the week that started Monday");
        assert_eq!(week_start_for("2026-09-01"), "2026-08-31", "week start may fall in the previous month");
    }

    #[test]
    fn empty_snapshot_dates_agree_with_build_snapshot() {
        let now = local_noon_ms(NaiveDate::from_ymd_opt(2026, 8, 26).unwrap());
        let fixture = Fixture::new("agree");
        let built = build_snapshot(&fixture.totals(), None, now);
        let empty = Snapshot::empty(now);
        assert_eq!(empty.today_date, built.today_date);
        assert_eq!(empty.week_start_date, built.week_start_date);
    }

    // ----------------------------------------------------------------------
    // The folds
    // ----------------------------------------------------------------------

    /// A `UsageRecord` built directly rather than round-tripped through the
    /// JSONL fixture. The folds below take `&[&UsageRecord]`, and the fields
    /// they care about (cost, `missing_pricing`, the five token buckets) are
    /// exactly the ones a fixture cannot set independently -- a fixture's cost
    /// comes from the live pricing catalogs, which is real data these tests
    /// must not assert against.
    #[test]
    fn history_is_every_day_up_to_today_with_quiet_days_as_zeroes() {
        let aggregate = |cost: f64, input: u64| ModelAggregate { input_tokens: input, cost, messages: 1, ..ModelAggregate::default() };
        let mut daily: HashMap<String, HashMap<String, ModelAggregate>> = HashMap::new();
        daily.entry("2026-11-05".into()).or_default().insert("claude-opus-5".into(), aggregate(3.0, 100));
        daily.entry("2026-11-05".into()).or_default().insert("claude-sonnet-5".into(), aggregate(5.0, 50));
        daily.entry("2026-11-03".into()).or_default().insert("claude-opus-5".into(), aggregate(1.0, 10));
        // Before the window: not charted.
        daily.entry("2026-01-01".into()).or_default().insert("claude-opus-5".into(), aggregate(9.0, 10));

        let history = fold_history(&daily, "2026-11-05");
        assert_eq!(history.len(), HISTORY_DAYS);
        assert_eq!(history.first().map(|d| d.date.as_str()), Some("2026-08-08"), "90 days, today included");
        let today = history.last().unwrap();
        assert_eq!((today.date.as_str(), today.cost, today.tokens, today.messages), ("2026-11-05", 8.0, 150, 2));
        assert_eq!(today.models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(), ["claude-sonnet-5", "claude-opus-5"], "costliest first");
        let quiet = &history[HISTORY_DAYS - 2];
        assert_eq!((quiet.date.as_str(), quiet.cost, quiet.models.len()), ("2026-11-04", 0.0, 0), "a quiet day is a zero, not a gap");
        assert_eq!(history[HISTORY_DAYS - 3].cost, 1.0);
        assert!(history.iter().all(|d| d.date.as_str() > "2026-01-01"));
    }

    fn record(ts_ms: i64, model: Option<&str>, input: u64, output: u64, cost: f64) -> UsageRecord {
        UsageRecord {
            timestamp_ms: ts_ms,
            date: local_date(ts_ms),
            session_id: "sess".to_owned(),
            project_path: "proj".to_owned(),
            input_tokens: input,
            output_tokens: output,
            model: model.map(str::to_owned),
            cost,
            ..UsageRecord::default()
        }
    }

    fn summary(session_id: &str, cost: f64, input: u64, output: u64, last_activity: i64) -> SessionSummary {
        SessionSummary {
            session_id: session_id.to_owned(),
            project_path: "proj".to_owned(),
            cost,
            total_tokens: input + output,
            input_tokens: input,
            output_tokens: output,
            message_count: 1,
            first_seen: 0,
            last_activity,
            models: vec!["claude-sonnet-4".to_owned()],
        }
    }

    fn model_usage(model: &str, cost: f64, output_tokens: u64, missing_pricing: bool) -> ModelUsage {
        ModelUsage { model: model.to_owned(), cost, tokens: output_tokens, output_tokens, missing_pricing }
    }

    #[test]
    fn shift_days_crosses_month_and_year_boundaries() {
        assert_eq!(shift_days("2026-08-26", -1), "2026-08-25");
        assert_eq!(shift_days("2026-09-01", -1), "2026-08-31", "the day before the first is last month's last");
        assert_eq!(shift_days("2026-01-01", -1), "2025-12-31");
        assert_eq!(shift_days("2028-03-01", -1), "2028-02-29", "2028 is a leap year");
        assert_eq!(shift_days("2026-08-24", -7), "2026-08-17", "the previous week's Monday");
        assert_eq!(shift_days("nonsense", -1), "nonsense", "a malformed date is passed through, not panicked on");
    }

    #[test]
    fn month_of_date_is_the_yyyy_mm_prefix() {
        assert_eq!(month_of_date("2026-08-26"), "2026-08");
        assert_eq!(month_of_date("bad"), "bad");
    }

    #[test]
    fn previous_day_and_week_and_month_fold_the_right_days() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(); // Wednesday
        let yesterday = NaiveDate::from_ymd_opt(2026, 8, 25).unwrap();
        let last_week = NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(); // previous Thursday
        let last_month = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let now = local_noon_ms(today);

        let fixture = Fixture::new("prev");
        fixture.write_session(
            "sess",
            &format!(
                "{}\n{}\n{}\n{}\n",
                line(now, "msg-today", 10, 5),
                line(local_noon_ms(yesterday), "msg-yesterday", 100, 0),
                line(local_noon_ms(last_week), "msg-lastweek", 1_000, 0),
                line(local_noon_ms(last_month), "msg-lastmonth", 10_000, 0),
            ),
        );

        let snapshot = build_snapshot(&fixture.totals(), None, now);

        assert_eq!(snapshot.month_date, "2026-08");
        assert_eq!(snapshot.prev_day_date, "2026-08-25");
        assert_eq!(snapshot.prev_week_start_date, "2026-08-17");
        assert_eq!(snapshot.today.tokens, 15);
        assert_eq!(snapshot.prev_day.tokens, 100);
        assert_eq!(snapshot.prev_week.tokens, 1_000, "the previous ISO week, not the previous seven days");
        assert_eq!(snapshot.month.tokens, 1_115, "August only: 15 + 100 + 1000, excluding July's 10000");
        assert_eq!(snapshot.today.input_tokens, 10);
        assert_eq!(snapshot.today.output_tokens, 5);
    }

    #[test]
    fn model_tiers_match_case_insensitive_substrings() {
        assert_eq!(ModelTier::of("claude-fable-5"), ModelTier::Frontier);
        assert_eq!(ModelTier::of("Claude-Mythos-Preview"), ModelTier::Frontier);
        assert_eq!(ModelTier::of("claude-opus-5-20260101"), ModelTier::Premium);
        assert_eq!(ModelTier::of("claude-sonnet-4-5"), ModelTier::Standard);
        assert_eq!(ModelTier::of("claude-haiku-4-5-fast"), ModelTier::Fast);
        assert_eq!(ModelTier::of("gpt-6-astra"), ModelTier::OpenAi);
        assert_eq!(ModelTier::of("codex-auto-review"), ModelTier::OpenAi);
        assert_eq!(ModelTier::of("llama-9"), ModelTier::Unknown);
        assert_eq!(ModelTier::of(UNKNOWN_MODEL), ModelTier::Unknown);
        assert_eq!(ModelTier::of(""), ModelTier::Unknown, "an empty name is Unknown, never a panic");
        assert_eq!(
            ModelTier::of("claude-fable-5-haiku"),
            ModelTier::Frontier,
            "the most capable match wins, so a future compound name does not read as Fast"
        );
    }

    #[test]
    fn fold_models_rolls_up_cost_tokens_and_missing_pricing() {
        let mut unpriced = record(0, Some("future-model"), 10, 10, 0.0);
        unpriced.missing_pricing = true;
        let records = [record(0, Some("claude-opus-5"), 100, 50, 2.0),
            record(1, Some("claude-opus-5"), 100, 50, 1.0),
            record(2, Some("claude-haiku-4"), 10, 5, 0.5),
            record(3, None, 7, 3, 0.0),
            unpriced];
        let borrowed: Vec<&UsageRecord> = records.iter().collect();
        let models = fold_models(&borrowed);

        assert_eq!(models.len(), 4, "opus merged, haiku, the unpriced model, and the no-model bucket");
        assert_eq!(models[0].model, "claude-opus-5", "sorted by cost descending");
        assert_eq!(models[0].cost, 3.0);
        assert_eq!(models[0].tokens, 300);
        assert_eq!(models[0].output_tokens, 100);
        assert!(!models[0].missing_pricing);

        let unknown = models.iter().find(|usage| usage.model == UNKNOWN_MODEL).expect("records with no model bucket as unknown");
        assert_eq!(unknown.tokens, 10);

        let future = models.iter().find(|usage| usage.model == "future-model").expect("an unpriced model is still listed");
        assert!(future.missing_pricing, "missing pricing is rolled up so the menu can mark it");
    }

    #[test]
    fn fold_models_and_token_breakdown_survive_an_empty_corpus() {
        assert!(fold_models(&[]).is_empty());
        assert!(fold_token_breakdown(&[]).is_empty());
        assert_eq!(fold_current_block(&[], 0), None);
        assert_eq!(most_efficient_model(&[]), None);
    }

    #[test]
    fn fold_token_breakdown_sorts_desc_and_drops_empty_categories() {
        let mut cached = record(0, Some("claude-opus-5"), 0, 0, 0.0);
        cached.cache_read_tokens = 5_000;
        cached.cache_creation_5m_tokens = 300;
        cached.cache_creation_1h_tokens = 200;
        let records = [record(0, Some("claude-opus-5"), 100, 20, 1.0), cached];
        let borrowed: Vec<&UsageRecord> = records.iter().collect();

        let breakdown = fold_token_breakdown(&borrowed);
        assert_eq!(
            breakdown,
            vec![
                ("Cache Read".to_owned(), 5_000),
                ("Cache Write".to_owned(), 500),
                ("Input".to_owned(), 100),
                ("Output".to_owned(), 20),
            ],
            "descending by count, with the two cache-creation buckets summed"
        );
    }

    #[test]
    fn fold_token_breakdown_omits_categories_with_no_tokens() {
        let records = [record(0, Some("claude-opus-5"), 100, 20, 1.0)];
        let borrowed: Vec<&UsageRecord> = records.iter().collect();
        let breakdown = fold_token_breakdown(&borrowed);
        assert_eq!(breakdown.len(), 2, "no cache tokens at all, so no cache rows");
        assert_eq!(breakdown[0].0, "Input");
    }

    #[test]
    fn fold_current_block_reports_the_active_block_and_its_burn_rate() {
        let now = 1_800_000_000_000; // an arbitrary fixed instant; no clock read
        let hour = 3_600_000i64;
        let records = [record(now - 90 * 60_000, Some("claude-opus-5"), 100, 50, 1.0),
            record(now - 30 * 60_000, Some("claude-opus-5"), 100, 50, 1.0)];
        let borrowed: Vec<&UsageRecord> = records.iter().collect();

        let block = fold_current_block(&borrowed, now).expect("usage 30 minutes ago is an active block");
        assert_eq!(block.cost_so_far, 2.0);
        assert_eq!(block.started_at_ms, now - 90 * 60_000, "the first entry, not the floored window start");
        assert!(block.end_time_ms > now, "an active block's window has not closed");
        assert!(block.end_time_ms <= now + 5 * hour);
        assert_eq!(block.tokens, 300);
        // $2 over a 60-minute first-to-last span.
        assert_eq!(block.cost_per_hour, Some(2.0));
    }

    #[test]
    fn fold_current_block_has_no_burn_rate_for_a_single_entry() {
        let now = 1_800_000_000_000;
        let records = [record(now - 60_000, Some("claude-opus-5"), 100, 50, 1.0)];
        let borrowed: Vec<&UsageRecord> = records.iter().collect();

        let block = fold_current_block(&borrowed, now).expect("one recent entry is still an active block");
        assert_eq!(block.cost_per_hour, None, "a zero-length span has no rate, and must not divide by zero");
        assert_eq!(block.cost_so_far, 1.0);
    }

    #[test]
    fn fold_current_block_is_none_after_five_idle_hours() {
        let now = 1_800_000_000_000;
        let records = [record(now - 6 * 3_600_000, Some("claude-opus-5"), 100, 50, 1.0)];
        let borrowed: Vec<&UsageRecord> = records.iter().collect();
        assert_eq!(fold_current_block(&borrowed, now), None, "idle longer than the 5-hour window: no active block");
    }

    #[test]
    fn fold_sessions_averages_over_every_session() {
        let summaries = vec![summary("aaa", 3.0, 100, 200, 3_000), summary("bbb", 1.0, 100, 200, 1_000)];
        let stats = fold_sessions(&summaries, &[]);

        assert_eq!(stats.count, 2);
        assert_eq!(stats.avg_cost, 2.0);
        assert_eq!(stats.avg_tokens, 300, "(300 + 300) / 2");
        assert_eq!(stats.output_input_ratio, Some(2.0), "400 output over 200 input");
        // $4 over 400 output tokens is $10,000 per million.
        assert_eq!(stats.cost_per_output_mtok, Some(10_000.0));
        assert_eq!(stats.latest.len(), 2);
        assert_eq!(stats.latest[0].session_id, "aaa", "newest first, by last activity");
    }

    #[test]
    fn fold_sessions_truncates_the_latest_list() {
        let summaries: Vec<SessionSummary> =
            (0..LATEST_SESSIONS + 3).map(|n| summary(&format!("s{n}"), 1.0, 10, 10, i64::try_from(n).unwrap_or(i64::MAX))).collect();
        let stats = fold_sessions(&summaries, &[]);
        assert_eq!(stats.latest.len(), LATEST_SESSIONS);
        assert_eq!(stats.latest[0].session_id, format!("s{}", LATEST_SESSIONS + 2), "the most recent session leads");
    }

    #[test]
    fn fold_sessions_on_no_sessions_is_all_zeroes_not_nan() {
        let stats = fold_sessions(&[], &[]);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.avg_cost, 0.0);
        assert_eq!(stats.avg_tokens, 0);
        assert_eq!(stats.output_input_ratio, None);
        assert_eq!(stats.cost_per_output_mtok, None);
        assert!(stats.latest.is_empty());
    }

    #[test]
    fn most_efficient_model_picks_the_cheapest_per_output_token() {
        let models = vec![model_usage("claude-opus-5", 100.0, 1_000_000, false), model_usage("claude-haiku-4", 1.0, 1_000_000, false)];
        let winner = most_efficient_model(&models).expect("two priced models, one is cheaper");
        assert_eq!(winner.model, "claude-haiku-4");
        assert_eq!(winner.cost_per_output_mtok, 1.0);
    }

    #[test]
    fn most_efficient_model_ignores_unpriced_unknown_and_output_free_models() {
        let models = vec![
            // Would win on cost-per-token ($0) if it were not excluded -- this
            // is the case the exclusion exists for.
            model_usage("future-model", 0.0, 1_000_000, true),
            model_usage(UNKNOWN_MODEL, 0.0, 1_000_000, false),
            // No output tokens at all: the ratio is undefined.
            model_usage("claude-opus-5", 5.0, 0, false),
            model_usage("claude-sonnet-4", 4.0, 1_000_000, false),
        ];
        let winner = most_efficient_model(&models).expect("one model qualifies");
        assert_eq!(winner.model, "claude-sonnet-4");
        assert_eq!(winner.cost_per_output_mtok, 4.0);
    }

    #[test]
    fn most_efficient_model_breaks_ties_by_evidence_then_name() {
        let models = vec![
            model_usage("model-b", 2.0, 2_000_000, false),
            model_usage("model-a", 1.0, 1_000_000, false),
            model_usage("model-c", 1.0, 1_000_000, false),
        ];
        // All three price at $1 per million output tokens; `model-b` has twice
        // the output tokens behind that number.
        let winner = most_efficient_model(&models).expect("all three qualify");
        assert_eq!(winner.model, "model-b");

        let two = vec![model_usage("model-c", 1.0, 1_000_000, false), model_usage("model-a", 1.0, 1_000_000, false)];
        assert_eq!(most_efficient_model(&two).unwrap().model, "model-a", "an exact tie falls back to the name");
    }

    #[test]
    fn empty_snapshot_has_every_section_blank_but_dated() {
        let now = local_noon_ms(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()); // a Tuesday
        let snapshot = Snapshot::empty(now);

        assert_eq!(snapshot.today_date, "2026-09-01");
        assert_eq!(snapshot.week_start_date, "2026-08-31");
        assert_eq!(snapshot.month_date, "2026-09");
        assert_eq!(snapshot.prev_day_date, "2026-08-31");
        assert_eq!(snapshot.prev_week_start_date, "2026-08-24");
        assert_eq!(snapshot.current_block, None);
        assert_eq!(snapshot.sessions, SessionStats::default());
        assert!(snapshot.models.is_empty());
        assert!(snapshot.token_breakdown.is_empty());
        assert_eq!(snapshot.total_cost, 0.0);
    }

    #[test]
    fn build_snapshot_on_an_empty_corpus_matches_the_empty_snapshot_shape() {
        let fixture = Fixture::new("empty-sections");
        let now = local_noon_ms(NaiveDate::from_ymd_opt(2026, 8, 26).unwrap());
        let built = build_snapshot(&fixture.totals(), None, now);
        let empty = Snapshot::empty(now);
        assert_eq!(built, empty, "no records and no session metadata is exactly the pre-refresh snapshot");
    }

    // ----------------------------------------------------------------------
    // The totals the tray runs on must say what the records say
    // ----------------------------------------------------------------------

    /// JSON values equal, floats to a tolerance: the two paths sum in
    /// different orders.
    fn approx_eq(a: &serde_json::Value, b: &serde_json::Value, path: &str) {
        use serde_json::Value::*;
        match (a, b) {
            (Number(x), Number(y)) if x.is_f64() || y.is_f64() => {
                let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
                assert!((x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0), "{path}: {x} vs {y}");
            }
            (Array(x), Array(y)) => {
                assert_eq!(x.len(), y.len(), "{path}: length");
                x.iter().zip(y).enumerate().for_each(|(i, (x, y))| approx_eq(x, y, &format!("{path}[{i}]")));
            }
            (Object(x), Object(y)) => {
                assert_eq!(x.keys().collect::<Vec<_>>(), y.keys().collect::<Vec<_>>(), "{path}: keys");
                x.iter().for_each(|(k, x)| approx_eq(x, &y[k], &format!("{path}.{k}")));
            }
            _ => assert_eq!(a, b, "{path}"),
        }
    }

    /// A usage line for `session` on `model` (priced models make the costs
    /// real; an unpriced one makes `missing_pricing` real).
    fn priced_line(ts_ms: i64, session: &str, id: &str, model: &str, input: u64, output: u64, cache_read: u64) -> String {
        let ts = Utc.timestamp_millis_opt(ts_ms).single().unwrap().to_rfc3339();
        format!(
            r#"{{"timestamp":"{ts}","sessionId":"{session}","requestId":"{id}-req","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read}}}}}}}"#
        )
    }

    #[test]
    fn the_totals_snapshot_matches_the_snapshot_built_from_the_records() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(); // a Wednesday
        let now = local_noon_ms(today);
        let day = 86_400_000;
        let fixture = Fixture::new("totals-vs-records");

        // Several sessions over several weeks and models, an unpriced model, a
        // message in two groups, and recent activity (an active block).
        let shared = priced_line(now - 40 * 60_000, "sess-a", "shared", "claude-opus-5", 1_000, 200, 50_000);
        let mut a = vec![shared.clone()];
        let mut b = vec![shared];
        for n in 0..30 {
            a.push(priced_line(now - (n + 1) * day, "sess-a", &format!("a{n}"), "claude-opus-5", 100 + u64::try_from(n).unwrap_or(0), 50, 10_000));
            b.push(priced_line(now - (n + 1) * day - 60_000, "sess-b", &format!("b{n}"), "claude-sonnet-5", 20, 10 + u64::try_from(n).unwrap_or(0), 4_000));
        }
        a.push(priced_line(now - 20 * 60_000, "sess-a", "a-recent", "claude-opus-5", 300, 90, 20_000));
        b.push(priced_line(now - 10 * 60_000, "sess-b", "b-recent", "claude-sonnet-5", 40, 30, 8_000));
        b.push(priced_line(now - 5 * 60_000, "sess-b", "unpriced", "future-model-9", 7, 3, 0));
        fixture.write_session("sess-a", &(a.join("\n") + "\n"));
        fixture.write_session("sess-b", &(b.join("\n") + "\n"));

        let capture = capture(r#"{"captured_at_ms": 7, "rate_limits": {"five_hour": {"used_percentage": 3}}}"#);
        let from_totals = build_snapshot(&fixture.totals(), Some(&capture), now);
        let from_records = build_snapshot_from_records(&fixture.cache(), Some(&capture), now);

        assert!(from_totals.current_block.is_some(), "the fixture has an active block");
        assert!(from_totals.models.iter().any(|m| m.missing_pricing), "and an unpriced model");
        assert_eq!(from_totals.record_count, 64, "32 + 33 records, the shared message counted once");
        approx_eq(&serde_json::to_value(&from_totals).unwrap(), &serde_json::to_value(&from_records).unwrap(), "snapshot");
    }

    // ----------------------------------------------------------------------
    // `--dump-snapshot`'s JSON round-trip
    // ----------------------------------------------------------------------

    #[test]
    fn empty_snapshot_round_trips_through_json() {
        let empty = Snapshot::empty(now_ms());
        let json = serde_json::to_string(&empty).expect("an empty snapshot always serializes");
        let back: Snapshot = serde_json::from_str(&json).expect("what was just serialized must deserialize");
        assert_eq!(empty, back);
    }

    /// A populated snapshot -- every section non-empty (a model, a session,
    /// an active block, a token breakdown, working time) -- round-trips too,
    /// not just the all-zero shape `Snapshot::empty` produces.
    #[test]
    fn populated_snapshot_round_trips_through_json() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(); // Wednesday
        let now = local_noon_ms(today);

        let fixture = Fixture::new("round-trip");
        fixture.write_session(
            "sess",
            &format!("{}\n{}\n", line(now - 30 * 60_000, "msg-a", 100, 50), line(now - 10 * 60_000, "msg-b", 10, 5)),
        );
        let built = build_snapshot(&fixture.totals(), None, now);
        assert!(!built.models.is_empty(), "fixture data should produce at least one model row");
        assert!(built.current_block.is_some(), "recent activity should leave a block active");

        let json = serde_json::to_string_pretty(&built).expect("a populated snapshot serializes");
        assert!(json.contains("\"generatedAtMs\""), "field naming is camelCase");
        let back: Snapshot = serde_json::from_str(&json).expect("what was just serialized must deserialize");
        assert_eq!(built, back);
    }

    /// A Codex rollout in a scratch `CODEX_HOME`-shaped tree, as the Codex
    /// store reads it: never the real `~/.codex`.
    fn codex_store(fixture: &Fixture, records: &[(i64, &str, u64, u64)]) -> UsageTotals {
        let thread = "01a1c25a-a06b-7471-8a01-1a5dd574e6e4";
        let dir = fixture.root.join("codex-home/sessions/2026/11/09");
        fs::create_dir_all(&dir).unwrap();
        let mut body = format!(r#"{{"timestamp":"2026-11-09T22:10:32.945Z","type":"session_meta","payload":{{"id":"{thread}","cwd":"/repo/codex"}}}}"#);
        body.push('\n');
        body.push_str(r#"{"timestamp":"2026-11-09T22:10:33.000Z","type":"turn_context","payload":{"turn_id":"t","model":"test-codex-model"}}"#);
        body.push('\n');
        for (ts_ms, response, input, output) in records {
            let ts = Utc.timestamp_millis_opt(*ts_ms).single().unwrap().to_rfc3339();
            write!(
                body,
                r#"{{"timestamp":"{ts}","type":"token_usage_record","payload":{{"thread_id":"{thread}","turn_id":"t","response_id":"{response}","usage":{{"input_tokens":{input},"cached_input_tokens":0,"output_tokens":{output}}}}}}}"#
            )
            .ok();
            body.push('\n');
        }
        let file = dir.join(format!("rollout-2026-11-09T23-08-01-{thread}.jsonl"));
        fs::write(&file, body).unwrap();
        let mut totals = UsageTotals::empty_for(&fixture.cache_dir, sl_usage::Source::Codex);
        totals.refresh_files(&[file], pricing(), RefreshMode::Incremental);
        totals
    }

    #[test]
    fn all_merges_both_stores_before_folding_sessions() {
        let fixture = Fixture::new("merge-all");
        let now = now_ms();
        // Recent enough for an active block; the day totals below compare
        // the views with each other, so the date they land on does not matter.
        let today = now - 10 * 60_000;
        // Two Claude sessions' worth in one, and one Codex session.
        fixture.write_session("sess", &format!("{}\n{}\n", line(today, "a", 100, 10), line(today + 1000, "b", 300, 30)));
        let claude = fixture.totals();
        let codex = codex_store(&fixture, &[(today + 2000, "r1", 50, 5)]);
        let limits = LimitsSnapshot { captured_at_ms: today, five_hour: None, seven_day: Some(LimitWindow { used_percent: 9.0, resets_at_ms: None, window_minutes: Some(10_080) }) };
        let views = build_snapshots(&claude, Some(&codex), None, Some(limits), now);

        let (claude_view, codex_view, all) = (&views.claude, views.codex.as_ref().unwrap(), views.all.as_ref().unwrap());
        assert_eq!((claude_view.record_count, codex_view.record_count, all.record_count), (2, 1, 3));
        assert_eq!(all.today.tokens, claude_view.today.tokens + codex_view.today.tokens);
        assert_eq!(all.sessions.count, 2, "one Claude session and one Codex thread");
        // Over every session, not the mean of the two views' averages.
        assert_eq!(all.sessions.avg_tokens, u64::midpoint(440, 55));
        assert_eq!(all.models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>().len(), 2);
        // Outside the home directory, as Claude Code's own name for it would be.
        assert_eq!(codex_view.sessions.latest[0].project, "-repo-codex", "a Codex project is its cwd, encoded like Claude's");
        // Limits: Claude's stay Claude's, Codex's ride along where it is shown.
        assert_eq!(claude_view.codex_limits, None);
        assert_eq!(codex_view.codex_limits, Some(limits));
        assert_eq!(all.codex_limits, Some(limits));
        assert_eq!(codex_view.rate_limits, None);
        // The All block covers both stores' records.
        let block = all.current_block.expect("records within the last five hours");
        assert_eq!(block.tokens, 440 + 55);
    }

    #[test]
    fn without_codex_only_claude_is_built_and_every_choice_falls_back_to_it() {
        let fixture = Fixture::new("no-codex");
        let now = now_ms();
        fixture.write_session("sess", &format!("{}\n", line(now - 60_000, "a", 1, 1)));
        let views = build_snapshots(&fixture.totals(), None, None, None, now);
        assert!(views.codex.is_none() && views.all.is_none());
        for provider in crate::view_prefs::Provider::ALL {
            assert_eq!(views.select(provider).record_count, 1, "{provider:?}");
        }
    }
}
