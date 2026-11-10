//! What the Stats window shows, worked out from a [`Snapshot`] with no AppKit
//! in sight: every figure, label and colour slot the view draws. The view in
//! `mod.rs` only lays this out and paints it.

use chrono::NaiveDate;

use crate::predictions;
use crate::snapshot::{DayUsage, ModelTier, Snapshot};

/// The ranges the toolbar offers, in days.
pub const RANGES: [u32; 3] = [7, 30, 90];
pub const DEFAULT_RANGE: u32 = 30;

/// How many models get a colour of their own; the rest share "Other".
pub const NAMED_SERIES: usize = 4;

/// Colour follows the model, never its rank in the visible range: the four
/// costliest models of the last 30 days -- what is in use now -- keep slots
/// 1-4 whichever range is shown, so switching ranges never repaints a model.
const COLOR_DAYS: usize = 30;

/// At most this many models in the range's cost-by-model list.
const BY_MODEL_ROWS: usize = 8;

/// Which sessions the table lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Latest,
    Top,
}

/// A colour identity: one of the named slots, or the shared "Other".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Series {
    Slot(usize),
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub name: String,
    pub series: Series,
}

/// One day of the chart: its total, and its cost split over the legend's
/// entries, in legend order.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// `Oct 10`, for the axis.
    pub short: String,
    /// `Fri, Oct 10`, for the hover readout.
    pub long: String,
    pub total: f64,
    pub parts: Vec<f64>,
}

/// One row of a horizontal bar list.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    pub name: String,
    /// A quieter note after the name (a share, say).
    pub note: Option<String>,
    pub value: f64,
    pub label: String,
    /// `None` draws the bar in the neutral colour.
    pub series: Option<Series>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub project: String,
    pub session: String,
    pub cost: String,
    pub tokens: String,
    pub messages: String,
    pub last_active: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub model: String,
    pub series: Series,
    pub tier: &'static str,
    pub cost: String,
    pub tokens: String,
    pub output: String,
    /// No catalog price matched, so its cost reads $0.
    pub missing_pricing: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatsModel {
    pub tiles: Vec<Tile>,
    pub days: u32,
    /// `$3,448.05 over 30 days · $114.93 a day`.
    pub range_summary: String,
    pub legend: Vec<LegendEntry>,
    pub columns: Vec<Column>,
    /// The chart's y-axis: a nice step, and the top of the scale.
    pub step: f64,
    pub top: f64,
    pub by_model: Vec<Bar>,
    pub by_kind: Vec<Bar>,
    pub session_stats: Vec<(&'static str, String)>,
    pub tab: Tab,
    pub sessions: Vec<SessionRow>,
    pub models: Vec<ModelRow>,
}

impl StatsModel {
    pub fn build(snapshot: &Snapshot, days: u32, tab: Tab, now_ms: i64) -> Self {
        let slots = color_slots(snapshot);
        let range = &snapshot.history[snapshot.history.len().saturating_sub(days as usize)..];
        let cost: f64 = range.iter().map(|day| day.cost).sum();
        let count = range.len().max(1);
        let (legend, columns) = chart(range, &slots);
        let max = columns.iter().map(|column| column.total).fold(0.0, f64::max);
        let step = nice_step(max / 4.0);
        let top = (step * (max / step).ceil()).max(step);
        Self {
            tiles: tiles(snapshot),
            days,
            range_summary: format!("{} over {} days · {} a day", money(cost), range.len(), money(cost / count as f64)),
            legend,
            columns,
            step,
            top,
            by_model: by_model(range, &slots),
            by_kind: by_kind(snapshot),
            session_stats: session_stats(snapshot),
            tab,
            sessions: sessions(snapshot, tab, now_ms),
            models: snapshot
                .models
                .iter()
                .map(|usage| ModelRow {
                    model: usage.model.clone(),
                    series: slots.of(&usage.model),
                    tier: ModelTier::of(&usage.model).label(),
                    cost: money(usage.cost),
                    tokens: compact(usage.tokens as f64),
                    output: compact(usage.output_tokens as f64),
                    missing_pricing: usage.missing_pricing,
                })
                .collect(),
        }
    }
}

/// The named models, in slot order, and whether any model is left over.
struct Slots {
    named: Vec<String>,
    more: bool,
}

impl Slots {
    fn of(&self, model: &str) -> Series {
        self.named.iter().position(|named| named == model).map_or(Series::Other, Series::Slot)
    }
}

fn color_slots(snapshot: &Snapshot) -> Slots {
    let recent = &snapshot.history[snapshot.history.len().saturating_sub(COLOR_DAYS)..];
    let mut ranked = costs_by_model(recent);
    if ranked.is_empty() {
        ranked = snapshot.models.iter().map(|usage| (usage.model.clone(), usage.cost)).collect();
    }
    let more = ranked.len() > NAMED_SERIES || snapshot.models.len() > ranked.len().min(NAMED_SERIES);
    Slots { named: ranked.into_iter().take(NAMED_SERIES).map(|(model, _)| model).collect(), more }
}

/// Each model's cost over `days`, costliest first, models with no cost left
/// out. Ties go by name, so the order is stable.
fn costs_by_model(days: &[DayUsage]) -> Vec<(String, f64)> {
    let mut totals: Vec<(String, f64)> = Vec::new();
    for model in days.iter().flat_map(|day| &day.models) {
        match totals.iter_mut().find(|(name, _)| *name == model.model) {
            Some((_, cost)) => *cost += model.cost,
            None => totals.push((model.model.clone(), model.cost)),
        }
    }
    totals.retain(|(_, cost)| *cost > 0.0);
    totals.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    totals
}

fn tiles(snapshot: &Snapshot) -> Vec<Tile> {
    // Today and this week are still running, so a percentage against the whole
    // of yesterday or last week would read "down" nearly every time. Name the
    // earlier period's amount instead.
    let projection = predictions::monthly_projection(snapshot.month.cost, &snapshot.today_date);
    vec![
        Tile { label: "Today", value: money(snapshot.today.cost), detail: format!("yesterday {}", money(snapshot.prev_day.cost)) },
        Tile { label: "This week", value: money(snapshot.week.cost), detail: format!("last week {}", money(snapshot.prev_week.cost)) },
        Tile { label: "This month", value: money(snapshot.month.cost), detail: projection.map(|p| format!("on track for {}", money(p))).unwrap_or_default() },
        Tile { label: "All time", value: money(snapshot.total_cost), detail: format!("{} records", whole(snapshot.record_count as u64)) },
    ]
}

fn chart(range: &[DayUsage], slots: &Slots) -> (Vec<LegendEntry>, Vec<Column>) {
    let mut legend: Vec<LegendEntry> = slots.named.iter().enumerate().map(|(i, name)| LegendEntry { name: name.clone(), series: Series::Slot(i) }).collect();
    if slots.more {
        legend.push(LegendEntry { name: "Other".to_owned(), series: Series::Other });
    }
    let columns = range
        .iter()
        .map(|day| {
            let mut parts = vec![0.0; legend.len()];
            for model in &day.models {
                let index = match slots.of(&model.model) {
                    Series::Slot(i) => Some(i),
                    Series::Other => slots.more.then_some(legend.len() - 1),
                };
                if let Some(index) = index {
                    parts[index] += model.cost;
                }
            }
            let date = NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok();
            Column {
                short: date.map_or_else(|| day.date.clone(), |d| d.format("%b %-d").to_string()),
                long: date.map_or_else(|| day.date.clone(), |d| d.format("%a, %b %-d").to_string()),
                total: day.cost,
                parts,
            }
        })
        .collect();
    (legend, columns)
}

fn by_model(range: &[DayUsage], slots: &Slots) -> Vec<Bar> {
    costs_by_model(range)
        .into_iter()
        .take(BY_MODEL_ROWS)
        .map(|(model, cost)| Bar { series: Some(slots.of(&model)), name: model, note: None, value: cost, label: money(cost) })
        .collect()
}

fn by_kind(snapshot: &Snapshot) -> Vec<Bar> {
    let total: u64 = snapshot.token_breakdown.iter().map(|(_, n)| n).sum();
    snapshot
        .token_breakdown
        .iter()
        .map(|(name, n)| Bar {
            name: name.clone(),
            note: (total > 0).then(|| format!("{:.1}%", *n as f64 / total as f64 * 100.0)),
            value: *n as f64,
            label: compact(*n as f64),
            series: None,
        })
        .collect()
}

fn session_stats(snapshot: &Snapshot) -> Vec<(&'static str, String)> {
    let stats = &snapshot.sessions;
    let dash = || "—".to_owned();
    vec![
        ("Sessions", whole(stats.count as u64)),
        ("Avg cost / session", money(stats.avg_cost)),
        ("Avg tokens / session", compact(stats.avg_tokens as f64)),
        ("Output / input", stats.output_input_ratio.map_or_else(dash, |r| format!("{r:.2}"))),
        ("Cost per output MTok", stats.cost_per_output_mtok.map_or_else(dash, money)),
        ("Most efficient", stats.most_efficient.as_ref().map_or_else(dash, |m| format!("{} · {}/MTok", m.model, money(m.cost_per_output_mtok)))),
    ]
}

fn sessions(snapshot: &Snapshot, tab: Tab, now_ms: i64) -> Vec<SessionRow> {
    let list = match tab {
        Tab::Latest => &snapshot.sessions.latest,
        Tab::Top => &snapshot.sessions.top,
    };
    list.iter()
        .map(|session| SessionRow {
            project: session.project.clone(),
            session: session.session_id.chars().take(8).collect(),
            cost: money(session.cost),
            tokens: compact(session.total_tokens as f64),
            messages: whole(session.messages as u64),
            last_active: ago(now_ms.saturating_sub(session.last_activity_ms)),
        })
        .collect()
}

/// A round step for an axis whose ticks should be about `raw` apart.
pub fn nice_step(raw: f64) -> f64 {
    if raw.is_nan() || raw <= 0.0 {
        return 1.0;
    }
    let power = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].into_iter().map(|f| f * power).find(|step| raw <= *step).unwrap_or(10.0 * power)
}

pub use sl_core::format::money;
use sl_core::format::{compact_number as compact, trim_decimals};

/// An axis label: `$0`, `$250`, `$1.5K`.
pub fn axis_money(value: f64) -> String {
    if value.abs() >= 1_000.0 { format!("${}", compact(value)) } else { format!("${}", trim_decimals(value, 2)) }
}

/// `429,344`.
pub fn whole(n: u64) -> String {
    sl_core::format::group_thousands(n, ',')
}

/// How long ago, to the minute: a table that ticked every second would be
/// redrawn every second for nothing.
pub fn ago(ms: i64) -> String {
    if ms < 60_000 { "just now".to_owned() } else { format!("{} ago", sl_core::format::ago_short(ms)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{DayModel, LatestSession, ModelUsage};

    const NOW: i64 = 1_788_200_000_000;

    fn day(date: &str, models: &[(&str, f64)]) -> DayUsage {
        DayUsage {
            date: date.to_owned(),
            cost: models.iter().map(|(_, cost)| cost).sum(),
            tokens: 0,
            messages: 0,
            models: models.iter().map(|(model, cost)| DayModel { model: (*model).to_owned(), cost: *cost, tokens: 0 }).collect(),
        }
    }

    fn snapshot(history: Vec<DayUsage>) -> Snapshot {
        let mut snapshot = Snapshot::empty(NOW);
        snapshot.history = history;
        snapshot
    }



    #[test]
    fn axis_labels_are_short() {
        assert_eq!(axis_money(0.0), "$0");
        assert_eq!(axis_money(250.0), "$250");
        assert_eq!(axis_money(2.5), "$2.5");
        assert_eq!(axis_money(1_500.0), "$1.5K");
    }

    #[test]
    fn nice_steps_are_round() {
        assert_eq!(nice_step(77.0), 100.0);
        assert_eq!(nice_step(30.0), 50.0);
        assert_eq!(nice_step(21.0), 25.0);
        assert_eq!(nice_step(0.0), 1.0);
    }

    #[test]
    fn ago_counts_minutes_hours_and_days() {
        assert_eq!(ago(30_000), "just now");
        assert_eq!(ago(5 * 60_000), "5m ago");
        assert_eq!(ago(3 * 3_600_000 + 60_000), "3h ago");
        assert_eq!(ago(2 * 86_400_000), "2d ago");
        assert_eq!(ago(-5), "just now");
    }

    #[test]
    fn colours_go_to_the_costliest_recent_models_and_stay_put_across_ranges() {
        let mut history: Vec<DayUsage> = (0..60).map(|i| day(&format!("2026-09-{:02}", 1 + i % 28), &[("old-model", 100.0)])).collect();
        history.extend((0..30).map(|i| day(&format!("2026-10-{:02}", 1 + i), &[("a", 5.0), ("b", 4.0), ("c", 3.0), ("d", 2.0), ("e", 1.0)])));
        let snapshot = snapshot(history);

        for days in RANGES {
            let model = StatsModel::build(&snapshot, days, Tab::Latest, NOW);
            let names: Vec<_> = model.legend.iter().map(|entry| entry.name.as_str()).collect();
            assert_eq!(names, ["a", "b", "c", "d", "Other"], "{days} days");
        }
        let model = StatsModel::build(&snapshot, 90, Tab::Latest, NOW);
        let old = model.by_model.iter().find(|bar| bar.name == "old-model").expect("in range");
        assert_eq!(old.series, Some(Series::Other), "a model out of use shares Other even when it tops the range");
    }

    #[test]
    fn a_column_splits_its_cost_over_the_legend() {
        let snapshot = snapshot(vec![day("2026-10-10", &[("a", 5.0), ("b", 4.0), ("c", 3.0), ("d", 2.0), ("e", 1.0), ("f", 0.5)])]);
        let model = StatsModel::build(&snapshot, 7, Tab::Latest, NOW);
        let column = &model.columns[0];
        assert_eq!(column.parts, [5.0, 4.0, 3.0, 2.0, 1.5]);
        assert_eq!(column.total, 15.5);
        assert_eq!((column.short.as_str(), column.long.as_str()), ("Oct 10", "Sat, Oct 10"));
        assert_eq!((model.step, model.top), (5.0, 20.0));
    }

    #[test]
    fn with_four_models_or_fewer_there_is_no_other() {
        let mut snapshot = snapshot(vec![day("2026-10-10", &[("a", 5.0), ("b", 4.0)])]);
        snapshot.models = vec![ModelUsage { model: "a".into(), ..ModelUsage::default() }, ModelUsage { model: "b".into(), ..ModelUsage::default() }];
        let model = StatsModel::build(&snapshot, 7, Tab::Latest, NOW);
        assert_eq!(model.legend.len(), 2);
        assert_eq!(model.columns[0].parts, [5.0, 4.0]);
    }

    #[test]
    fn the_range_is_the_newest_days_and_its_summary_averages_over_them() {
        let snapshot = snapshot((1..=10).map(|d| day(&format!("2026-10-{d:02}"), &[("a", f64::from(d))])).collect());
        let model = StatsModel::build(&snapshot, 7, Tab::Latest, NOW);
        assert_eq!(model.columns.len(), 7);
        assert_eq!(model.columns[0].short, "Oct 4");
        assert_eq!(model.range_summary, "$49.00 over 7 days · $7.00 a day");
    }

    #[test]
    fn the_session_table_follows_the_tab() {
        let session = |id: &str, cost: f64| LatestSession {
            session_id: id.to_owned(),
            project: "Developer-statusline".to_owned(),
            cost,
            total_tokens: 1_500_000,
            messages: 1_234,
            first_seen_ms: 0,
            last_activity_ms: NOW - 2 * 3_600_000,
        };
        let mut snapshot = snapshot(Vec::new());
        snapshot.sessions.latest = vec![session("0123456789abcdef", 1.0)];
        snapshot.sessions.top = vec![session("fedcba9876543210", 99.0)];
        let latest = StatsModel::build(&snapshot, 7, Tab::Latest, NOW).sessions;
        assert_eq!(latest[0], SessionRow {
            project: "Developer-statusline".into(),
            session: "01234567".into(),
            cost: "$1.00".into(),
            tokens: "1.5M".into(),
            messages: "1,234".into(),
            last_active: "2h ago".into(),
        });
        assert_eq!(StatsModel::build(&snapshot, 7, Tab::Top, NOW).sessions[0].session, "fedcba98");
    }

    #[test]
    fn token_kinds_carry_their_share() {
        let mut snapshot = snapshot(Vec::new());
        snapshot.token_breakdown = vec![("Cache Read".into(), 979), ("Output".into(), 21)];
        let bars = StatsModel::build(&snapshot, 7, Tab::Latest, NOW).by_kind;
        assert_eq!(bars[0].note.as_deref(), Some("97.9%"));
        assert_eq!(bars[1].label, "21");
    }
}
