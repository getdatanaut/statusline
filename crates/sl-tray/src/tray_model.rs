//! What the tray shows, as plain values: the limit meters and the line under
//! them, the menu bar glyph's meter, the footer, and the rows of the Closed
//! and Stacks groups. Pure: built from a snapshot and the clock, drawn by the
//! views in `controller`, `compact` and `meters`, and tested here without
//! AppKit.

use std::fmt::Write as _;

use crate::menu_rows::{HeaderContent, RowContent};
use crate::meters::{MeterSlot, WindowModel};
use crate::predictions;
use crate::sessions::{self, Icon, OpenSession, Seg, Tone};
use crate::snapshot::{CurrentBlock, LimitWindow, Snapshot};
use crate::stacks::{EntryState, StackView};
use crate::sticky::Remembered;
use crate::view_prefs::{self, BarWindow, Provider, Source};

/// A stack's row: `Stack #746 · 6/10 merged`, and the PR the work is at.
pub(crate) fn stack_row(stack: &StackView) -> RowContent {
    let detail = match stack.next_open() {
        Some(entry) => {
            let mut text = format!("next #{}", entry.number);
            if let Some(branch) = &entry.branch {
                write!(text, " · {}", sessions::truncate_middle(branch, 40)).ok();
            }
            text
        }
        None => "all merged".to_owned(),
    };
    RowContent {
        icon: Some(Icon::Stack),
        ring: None,
        title: vec![sessions::seg(stack.title(), Tone::Plain)],
        detail: vec![sessions::seg(detail, Tone::Dim)],
        right: None,
        mark: None,
        chevron: Some(false),
        tinted: false,
        dimmed: false,
    }
}

/// The mark for a PR's state in a stack: an SF Symbol and its colour.
pub(crate) fn stack_mark(state: EntryState) -> (&'static str, Tone) {
    match state {
        EntryState::Merged => ("checkmark.circle.fill", Tone::Green),
        EntryState::Open => ("circle", Tone::Plain),
        EntryState::Draft => ("circle.dashed", Tone::Dim),
        EntryState::Closed => ("xmark.circle", Tone::Dim),
        EntryState::Queued => ("smallcircle.filled.circle", Tone::Yellow),
        EntryState::NeedsRebase => ("exclamationmark.triangle.fill", Tone::Orange),
        EntryState::Unknown => ("questionmark.circle", Tone::Dim),
    }
}

/// A closed session's row: its name, dimmed; where it was; how long ago.
pub(crate) fn closed_row(entry: &Remembered, mark: bool, now: i64) -> RowContent {
    RowContent {
        icon: Some(Icon::Closed),
        ring: None,
        title: vec![sessions::seg(&entry.name, Tone::Plain)],
        detail: closed_detail(entry),
        right: Some((crate::sticky::closed_ago(entry.last_seen_ms, now), Tone::Dim)),
        mark: mark.then_some(entry.provider),
        chevron: Some(false),
        tinted: false,
        dimmed: true,
    }
}

/// `statusline · jakub/dn-479-… · #781`.
pub(crate) fn closed_detail(entry: &Remembered) -> Vec<Seg> {
    let mut parts = vec![entry.project.clone()];
    parts.extend(entry.branch.as_deref().map(|branch| sessions::truncate_middle(branch, 34)));
    parts.extend(entry.pr_number.map(|pr| format!("#{pr}")));
    vec![sessions::seg(parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · "), Tone::Dim)]
}

/// The card at the top of a closed session's submenu.
pub(crate) fn closed_header(entry: &Remembered, now: i64) -> HeaderContent {
    let mut lines = vec![vec![sessions::seg(format!("{} · {}", entry.project, crate::sticky::closed_ago(entry.last_seen_ms, now)), Tone::Dim)]];
    lines.extend(entry.branch.iter().map(|branch| vec![sessions::seg(branch, Tone::Dim)]));
    lines.push(vec![sessions::seg(&entry.session_id, Tone::Dim)]);
    HeaderContent {
        symbol: Some(("clock.arrow.circlepath", Tone::Dim)),
        title: entry.pr_number.map_or_else(|| entry.name.clone(), |pr| format!("{} · #{pr}", entry.name)),
        lines,
        ..HeaderContent::default()
    }
}

/// The Closed group's own row: `Closed · 3`, nothing more.
pub(crate) fn closed_fold(entries: &[Remembered], open: bool) -> RowContent {
    RowContent {
        icon: Some(Icon::Closed),
        ring: None,
        title: vec![sessions::seg(format!("Closed · {}", entries.len()), Tone::Plain)],
        detail: Vec::new(),
        right: None,
        mark: None,
        chevron: Some(open),
        tinted: true,
        dimmed: false,
    }
}

/// `Updated 29s ago · 433,498 records`: the footer of both layouts, and the
/// Stats window's subtitle.
pub(crate) fn footer_text(snapshot: &Snapshot, now: i64) -> String {
    format!(
        "Updated {} ago · {} records",
        sl_core::format::fmt_elapsed(now.saturating_sub(snapshot.generated_at_ms)),
        sl_core::format::group_thousands(snapshot.record_count as u64, ',')
    )
}

pub(crate) fn window_model(window: &LimitWindow, window_ms: i64, spend: Option<String>, now: i64) -> WindowModel {
    let used = window.used_percent;
    let resets_at = window.resets_at_ms;
    let pace = resets_at.and_then(|at| predictions::window_pace(used, window_ms, at, now));
    WindowModel {
        used,
        projected: resets_at.and_then(|at| predictions::window_projection(used, window_ms, at, now)),
        elapsed: resets_at.and_then(|at| predictions::window_elapsed_percent(window_ms, at, now)).map(|elapsed| elapsed.min(100.0)),
        pace,
        tone: sessions::window_tone(used, pace),
        resets: reset_text(resets_at, now),
        spend,
        expired: false,
    }
}

/// A window's length: its own when the capture says, else `fallback`.
pub(crate) fn window_length(window: &LimitWindow, fallback: i64) -> i64 {
    window.window_minutes.map_or(fallback, |minutes| i64::from(minutes) * 60_000)
}

/// `5h`, `7d`, `90m`: a window length as its meter names it.
pub(crate) fn span_label(window: &LimitWindow, fallback: &str) -> String {
    match window.window_minutes {
        None => fallback.to_owned(),
        Some(minutes) if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
    }
}

/// A Codex window's meter. Once its reset has passed, the reading is about a
/// window that is over and nothing has been seen of the next one: shown as
/// unknown, not as the old percentage and not as 0%.
pub(crate) fn codex_window_model(window: &LimitWindow, fallback_ms: i64, now: i64) -> WindowModel {
    if window.resets_at_ms.is_some_and(|at| at <= now) {
        return WindowModel {
            used: 0.0,
            projected: None,
            elapsed: None,
            pace: None,
            tone: Tone::Dim,
            resets: Some("reset — no reading since".to_owned()),
            spend: None,
            expired: true,
        };
    }
    window_model(window, window_length(window, fallback_ms), None, now)
}

/// The tools the meters' page dots switch between, in order.
pub(crate) const METER_PAGES: [Provider; 2] = [Provider::Claude, Provider::Codex];

/// The page dots under the meters, `(pages, current)`: only when Codex is
/// installed, so there is another page to turn to.
pub(crate) fn meter_dots(provider: Provider) -> Option<(usize, usize)> {
    view_prefs::codex_available().then(|| (METER_PAGES.len(), METER_PAGES.iter().position(|p| *p == provider).unwrap_or(0)))
}

/// The meters `provider` shows, and a line to show under them when there is
/// something to say about the readings themselves.
///
/// Claude Code: its session and weekly windows, the spend under the session.
/// Codex: its windows as last seen in its rollouts. All: Claude Code's two and
/// Codex's, named `Codex` -- the spend, over both, under Claude's session.
pub(crate) fn limit_models(snapshot: &Snapshot, provider: Provider, show_spend: bool, now: i64) -> (Vec<MeterSlot>, Option<String>) {
    let mut slots = Vec::new();
    let mut notes = Vec::new();
    let block = snapshot.current_block.as_ref();
    if provider.shows(Source::Claude) {
        match snapshot.rate_limits.as_ref() {
            // The ordinary first-run state, not an error: nothing has captured
            // anything yet because Claude Code has not run a turn.
            None => notes.push("Usage limits: no capture yet — run a Claude Code turn".to_owned()),
            Some(limits) => {
                // A stale capture's meters are dimmed rather than hidden: they
                // are the last thing we were told, and the note says how long
                // ago that was.
                let stale = predictions::capture_is_stale(limits.captured_at_ms, now);
                let spend = spend_line(block, now).filter(|_| show_spend);
                if let Some(window) = limits.five_hour {
                    slots.push(MeterSlot { name: "Session".into(), span: "5h".into(), model: window_model(&window, predictions::SESSION_WINDOW_MS, spend, now), dimmed: stale });
                }
                if let Some(window) = limits.seven_day {
                    // Without a session window (a plan with no session limit)
                    // the spend goes under the weekly one.
                    let spend = if limits.five_hour.is_none() { spend_line(block, now).filter(|_| show_spend) } else { None };
                    slots.push(MeterSlot { name: "Weekly".into(), span: "7d".into(), model: window_model(&window, predictions::WEEKLY_WINDOW_MS, spend, now), dimmed: stale });
                }
                // Only worth a line once the capture has fallen behind: while
                // it is fresh it says the same thing as the "Updated" footer.
                if stale {
                    notes.push(format!("Limits data {} old — stale", sl_core::format::fmt_elapsed(now.saturating_sub(limits.captured_at_ms))));
                } else if limits.five_hour.is_none() && limits.seven_day.is_none() {
                    notes.push("Usage limits: none in the last capture".to_owned());
                }
            }
        }
    }
    if provider.shows(Source::Codex) {
        let all = provider == Provider::All;
        match snapshot.codex_limits.as_ref() {
            None => notes.push("Codex limits: none seen yet — run a Codex turn".to_owned()),
            Some(limits) => {
                // Not dimmed for age: Codex reports its limits only during its
                // own turns, so its newest reading is usually minutes or hours
                // old and still the latest there is.
                let first = slots.len();
                let mut push = |window: &LimitWindow, name: &str, fallback_span: &str, fallback_ms: i64| {
                    let name = if all { "Codex" } else { name };
                    let model = codex_window_model(window, fallback_ms, now);
                    slots.push(MeterSlot { name: name.into(), span: span_label(window, fallback_span), model, dimmed: false });
                };
                if let Some(window) = limits.five_hour {
                    push(&window, "Session", "5h", predictions::SESSION_WINDOW_MS);
                }
                if let Some(window) = limits.seven_day {
                    push(&window, "Weekly", "7d", predictions::WEEKLY_WINDOW_MS);
                }
                // Codex's spend in its current 5-hour block goes under its
                // first meter: the 5-hour one when it reports one, else the
                // weekly (all it sends on the plans seen). Under All the spend
                // line is Claude's, over both.
                if !all && let Some(slot) = slots.get_mut(first) {
                    slot.model.spend = spend_line(block, now).filter(|_| show_spend);
                }
            }
        }
    }
    let note = (!notes.is_empty()).then(|| notes.join(" · "));
    (slots, note)
}

/// A day, for choosing how to say when a window resets.
pub(crate) const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `resets in 3h 21m`, or for a reset more than a day out the moment itself,
/// `resets Wed 08:59`: "in 4d 3h" leaves the calendar arithmetic to the
/// reader. `None` for a reset time that is missing or already behind us -- a
/// countdown to a moment that has passed is noise, and the capture's own age
/// already says the number is old.
pub(crate) fn reset_text(resets_at_ms: Option<i64>, now: i64) -> Option<String> {
    let resets_at_ms = resets_at_ms.filter(|resets_at_ms| *resets_at_ms > now)?;
    if resets_at_ms - now >= DAY_MS {
        let local = chrono::DateTime::from_timestamp_millis(resets_at_ms)?.with_timezone(&chrono::Local);
        Some(format!("resets {}", local.format("%a %H:%M")))
    } else {
        Some(format!("resets in {}", sl_core::format::fmt_elapsed(resets_at_ms - now)))
    }
}

/// `$15.41 so far · $7.27/hr · $35.88 projected`; `None` when no block is
/// active (idle for over five hours, or no usage at all -- ordinary states, and
/// neither is "$0.00 projected").
pub(crate) fn spend_line(block: Option<&CurrentBlock>, now: i64) -> Option<String> {
    let block = block?;
    let projected = predictions::block_projection(block.cost_so_far, block.cost_per_hour, block.end_time_ms, now);
    let mut parts = vec![format!("{} so far", sl_core::format::money(block.cost_so_far))];
    // A block with one entry has no first-to-last span to divide by, so it has
    // no rate yet -- leave it out rather than print "$0.00/hr".
    if let Some(per_hour) = block.cost_per_hour {
        parts.push(format!("{}/hr", sl_core::format::money(per_hour)));
    }
    parts.push(format!("{} projected", sl_core::format::money(projected)));
    Some(parts.join(" · "))
}

/// The glyph's meter, showing the 5-hour session window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BarMeter {
    /// Percent of the window used, 0-100.
    pub used: u8,
    /// Whether the window is on pace to end past 100%.
    pub over_pace: bool,
}

/// The glyph's meter for `provider`: how much of its window is used, and green
/// or yellow for whether that ends the window within 100% at the current pace
/// -- the same projection as the menu's "16% → 82%".
///
/// Claude Code: its 5-hour window. Codex: its shortest window present (the
/// weekly one, on the plans seen). All: whichever of those is projected to end
/// furthest over -- the one most worth a glance.
pub(crate) fn bar_meter(snapshot: &Snapshot, provider: Provider, window: BarWindow, now: i64) -> Option<BarMeter> {
    // `Auto`: a plan with no session limit sends only the weekly window, so
    // then that. The others show their window or nothing.
    let session = |w: Option<LimitWindow>, length: i64| w.map(|w| (w, length));
    let claude = provider
        .shows(Source::Claude)
        .then(|| {
            let limits = snapshot.rate_limits?;
            let five = session(limits.five_hour, predictions::SESSION_WINDOW_MS);
            let seven = session(limits.seven_day, predictions::WEEKLY_WINDOW_MS);
            match window {
                BarWindow::Auto => five.or(seven),
                BarWindow::Session => five,
                BarWindow::Weekly => seven,
            }
        })
        .flatten();
    let codex = provider
        .shows(Source::Codex)
        .then(|| {
            let limits = snapshot.codex_limits?;
            let five = limits.five_hour.map(|w| (w, window_length(&w, predictions::SESSION_WINDOW_MS)));
            let seven = limits.seven_day.map(|w| (w, window_length(&w, predictions::WEEKLY_WINDOW_MS)));
            match window {
                BarWindow::Auto => five.or(seven),
                BarWindow::Session => five,
                BarWindow::Weekly => seven,
            }
        })
        .flatten();
    [claude, codex]
        .into_iter()
        .flatten()
        .map(|(window, length)| window_meter(window, length, now))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(meter, _)| meter)
}

/// One window as the glyph shows it, with its projected end for choosing
/// between windows.
pub(crate) fn window_meter(window: LimitWindow, length_ms: i64, now: i64) -> (BarMeter, f64) {
    if window.resets_at_ms.is_some_and(|at| at <= now) {
        // The window it describes is over; nothing of the next one is used
        // until a capture says so.
        return (BarMeter { used: 0, over_pace: false }, 0.0);
    }
    let projected = window
        .resets_at_ms
        .and_then(|at| predictions::window_projection(window.used_percent, length_ms, at, now))
        .unwrap_or(window.used_percent);
    (BarMeter { used: sl_core::num::trunc_u8(window.used_percent.clamp(0.0, 100.0).round()), over_pace: projected.round() > 100.0 }, projected)
}

/// A pill: short text on a tinted capsule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pill {
    pub text: String,
    pub tone: Tone,
}

pub(crate) fn pill(text: impl Into<String>, tone: Tone) -> Pill {
    Pill { text: text.into(), tone }
}

/// Where a PR's pills go, which says how much each says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PillsFor {
    /// A compact card: `Ready`, and the CI with what its count is of.
    Card,
    /// A session's submenu header: `Ready for review`, the CI as a count.
    Header,
}

/// A PR's draft state and verdict as pills, and its CI as one more: the same
/// facts in both layouts, worded for `place`.
pub(crate) fn pr_pills(info: &sl_core::gh::GhInfo, place: PillsFor) -> (Vec<Pill>, Option<Pill>) {
    let card = place == PillsFor::Card;
    let ready = if card { "Ready" } else { "Ready for review" };
    let mut pills = vec![if info.is_draft == Some(true) { pill("Draft", Tone::Yellow) } else { pill(ready, Tone::Green) }];
    if let Some((text, tone)) = sessions::verdict_text(info) {
        pills.push(pill(text, tone));
    }
    let ci = info.ci.as_ref().map(|ci| match ci.state {
        sl_core::gh::CiState::Passing => pill("CI ✓", Tone::Green),
        sl_core::gh::CiState::Pending if card => pill(format!("CI … {} running", ci.pending), Tone::Yellow),
        sl_core::gh::CiState::Pending => pill(format!("CI … {}", ci.pending), Tone::Yellow),
        sl_core::gh::CiState::Failing if card => pill(format!("CI ✗ {} failed", ci.failed.len()), Tone::Red),
        sl_core::gh::CiState::Failing => pill(format!("CI ✗ {}", ci.failed.len()), Tone::Red),
    });
    (pills, ci)
}

/// The card at the top of a session's submenu: the PR, its state and CI,
/// what it is based on; or, with no PR, the branch.
pub(crate) fn header_content(session: &OpenSession) -> HeaderContent {
    let Some((info, number)) = session.gh.as_ref().and_then(|info| info.pr_number.map(|n| (info, n))) else {
        let title = sessions::no_pr(session).unwrap_or(sessions::NoPr::NoneYet).title().to_owned();
        let mut lines: Vec<Vec<Seg>> = session.branch.iter().map(|branch| vec![sessions::seg(branch, Tone::Dim)]).collect();
        lines.extend(session.linear.iter().map(sessions::linear_segments));
        return HeaderContent { symbol: Some(("arrow.triangle.branch", Tone::Dim)), title, lines, ..HeaderContent::default() };
    };
    let draft = info.is_draft == Some(true);
    let (pills, end_pill) = pr_pills(info, PillsFor::Header);
    let mut lines: Vec<Vec<Seg>> = session.linear.iter().map(sessions::linear_segments).collect();
    // The base and its tip share a line: two short facts, not two lines.
    let mut onto: Vec<String> = Vec::new();
    if let Some(base) = &info.base {
        // Shorter when the tip follows on the same line, so it is not pushed out.
        let room = if info.base_sha.is_some() { 24 } else { 38 };
        onto.push(format!("onto {}", sessions::truncate_middle(base, room)));
    }
    if let Some(sha) = &info.base_sha {
        onto.push(sha.chars().take(7).collect());
    }
    if !onto.is_empty() {
        lines.push(vec![sessions::seg(onto.join(" · "), Tone::Dim)]);
    }
    let mut below: Vec<String> = Vec::new();
    if let Some(parent) = info.base_pr {
        below.push(format!("stacked on #{parent}"));
    }
    if info.reviews.is_empty() && info.assessment.is_none() && info.requested.is_empty() {
        below.push("no reviews yet".to_owned());
    }
    if !below.is_empty() {
        lines.push(vec![sessions::seg(below.join(" · "), Tone::Dim)]);
    }
    HeaderContent { symbol: Some(("arrow.triangle.pull", if draft { Tone::Yellow } else { Tone::Green })), title: format!("#{number}"), pills, end_pill, lines }
}

/// The stack `target` names among `stacks`, and the PR in it when one is
/// named. `None` when the stack has gone, or the PR is no longer in it.
pub(crate) fn stack_for(stacks: &[StackView], target: &crate::act::Target) -> Option<(StackView, Option<i64>)> {
    let (key, pr) = match target {
        crate::act::Target::Stack(key) => (key, None),
        crate::act::Target::StackPr { stack, pr } => (stack, Some(*pr)),
        _ => return None,
    };
    let stack = stacks.iter().find(|stack| stack.key() == *key)?.clone();
    if pr.is_some_and(|pr| !stack.entries.iter().any(|entry| entry.number == pr)) {
        return None;
    }
    Some((stack, pr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictions::{SESSION_WINDOW_MS, WEEKLY_WINDOW_MS};
    use crate::snapshot::LimitsSnapshot;

    const NOW: i64 = 1_788_200_000_000;

    /// A window of `window_ms` that is `fraction` of the way through, with
    /// `used_percent` of it spent.
    fn window(used_percent: f64, window_ms: i64, fraction: f64) -> LimitWindow {
        LimitWindow { used_percent, resets_at_ms: Some(NOW + sl_core::num::trunc_i64(window_ms as f64 * (1.0 - fraction))), window_minutes: None }
    }

    fn limits(five_hour: Option<LimitWindow>) -> LimitsSnapshot {
        LimitsSnapshot { captured_at_ms: NOW, five_hour, seven_day: None }
    }

    /// Claude Code's snapshot with `limits`.
    fn claude(limits: Option<LimitsSnapshot>) -> Snapshot {
        Snapshot { rate_limits: limits, ..Snapshot::default() }
    }

    /// Codex's weekly window, `used` percent `fraction` of the way through.
    fn codex_weekly(used_percent: f64, fraction: f64) -> LimitsSnapshot {
        let window = LimitWindow { window_minutes: Some(10_080), ..window(used_percent, WEEKLY_WINDOW_MS, fraction) };
        LimitsSnapshot { captured_at_ms: NOW, five_hour: None, seven_day: Some(window) }
    }

    fn glyph(limits: Option<LimitsSnapshot>) -> Option<BarMeter> {
        bar_meter(&claude(limits), Provider::Claude, BarWindow::Auto, NOW)
    }

    #[test]
    fn the_bar_meter_is_green_while_the_session_ends_within_its_limit() {
        // 40% used halfway through: on pace for 80%.
        let meter = glyph(Some(limits(Some(window(40.0, SESSION_WINDOW_MS, 0.5)))));
        assert_eq!(meter, Some(BarMeter { used: 40, over_pace: false }));
        // Exactly on pace for 100% is still within it.
        let meter = glyph(Some(limits(Some(window(50.0, SESSION_WINDOW_MS, 0.5)))));
        assert_eq!(meter, Some(BarMeter { used: 50, over_pace: false }));
    }

    #[test]
    fn the_bar_meter_is_yellow_once_the_session_is_on_pace_past_its_limit() {
        // 60% used halfway through: on pace for 120%.
        let meter = glyph(Some(limits(Some(window(60.0, SESSION_WINDOW_MS, 0.5)))));
        assert_eq!(meter, Some(BarMeter { used: 60, over_pace: true }));
    }

    #[test]
    fn the_bar_meter_is_empty_once_its_window_has_reset_and_absent_without_a_capture() {
        let mut ended = window(90.0, SESSION_WINDOW_MS, 0.5);
        ended.resets_at_ms = Some(NOW - 1);
        assert_eq!(glyph(Some(limits(Some(ended)))), Some(BarMeter { used: 0, over_pace: false }));
        assert_eq!(glyph(Some(limits(None))), None);
        assert_eq!(glyph(None), None);
    }

    #[test]
    fn the_bar_meter_without_a_reset_time_judges_pace_by_usage_alone() {
        let meter = glyph(Some(limits(Some(LimitWindow { used_percent: 120.0, resets_at_ms: None, window_minutes: None }))));
        assert_eq!(meter, Some(BarMeter { used: 100, over_pace: true }));
    }

    #[test]
    fn without_a_session_window_the_glyph_and_the_spend_use_the_weekly_one() {
        let weekly_only = LimitsSnapshot { captured_at_ms: NOW, five_hour: None, seven_day: Some(window(60.0, WEEKLY_WINDOW_MS, 0.5)) };
        assert_eq!(glyph(Some(weekly_only)), Some(BarMeter { used: 60, over_pace: true }));
        let block = CurrentBlock { cost_so_far: 1.0, cost_per_hour: None, end_time_ms: NOW + 3_600_000, started_at_ms: NOW - 60_000, tokens: 1 };
        let snapshot = Snapshot { rate_limits: Some(weekly_only), current_block: Some(block), ..Snapshot::default() };
        let (slots, _) = limit_models(&snapshot, Provider::Claude, true, NOW);
        assert_eq!(slots.len(), 1);
        assert!(slots[0].model.spend.is_some(), "the spend line still shows");
    }

    #[test]
    fn codex_spend_shows_under_its_weekly_meter_when_that_is_all_it_reports() {
        let block = CurrentBlock { cost_so_far: 2.5, cost_per_hour: None, end_time_ms: NOW + 3_600_000, started_at_ms: NOW - 60_000, tokens: 1 };
        let snapshot = Snapshot { codex_limits: Some(codex_weekly(9.0, 0.1)), current_block: Some(block), ..Snapshot::default() };
        let (slots, _) = limit_models(&snapshot, Provider::Codex, true, NOW);
        assert_eq!(slots[0].model.spend.as_deref(), Some("$2.50 so far · $2.50 projected"));
        let (slots, _) = limit_models(&snapshot, Provider::Codex, false, NOW);
        assert_eq!(slots[0].model.spend, None, "unless the spend line is switched off");
    }

    #[test]
    fn the_glyph_can_be_pinned_to_one_window() {
        let both = LimitsSnapshot { captured_at_ms: NOW, five_hour: Some(window(40.0, SESSION_WINDOW_MS, 0.5)), seven_day: Some(window(20.0, WEEKLY_WINDOW_MS, 0.5)) };
        let snapshot = claude(Some(both));
        let meter = |window| bar_meter(&snapshot, Provider::Claude, window, NOW).map(|m| m.used);
        assert_eq!(meter(BarWindow::Auto), Some(40), "the session window, as before");
        assert_eq!(meter(BarWindow::Session), Some(40));
        assert_eq!(meter(BarWindow::Weekly), Some(20));
        let weekly_only = claude(Some(LimitsSnapshot { five_hour: None, ..both }));
        assert_eq!(bar_meter(&weekly_only, Provider::Claude, BarWindow::Auto, NOW).map(|m| m.used), Some(20), "no session limit: the weekly one");
        assert_eq!(bar_meter(&weekly_only, Provider::Claude, BarWindow::Session, NOW), None, "pinned to a window it does not have: none");
    }

    #[test]
    fn the_glyph_follows_the_provider() {
        let snapshot = Snapshot {
            rate_limits: Some(limits(Some(window(40.0, SESSION_WINDOW_MS, 0.5)))),
            codex_limits: Some(codex_weekly(60.0, 0.5)),
            ..Snapshot::default()
        };
        assert_eq!(bar_meter(&snapshot, Provider::Claude, BarWindow::Auto, NOW), Some(BarMeter { used: 40, over_pace: false }), "Claude: its 5-hour window");
        assert_eq!(bar_meter(&snapshot, Provider::Codex, BarWindow::Auto, NOW), Some(BarMeter { used: 60, over_pace: true }), "Codex: its shortest window, the weekly");
        assert_eq!(bar_meter(&snapshot, Provider::All, BarWindow::Auto, NOW), Some(BarMeter { used: 60, over_pace: true }), "All: the one projected furthest over");
        let calm = Snapshot { codex_limits: Some(codex_weekly(10.0, 0.5)), ..snapshot };
        assert_eq!(bar_meter(&calm, Provider::All, BarWindow::Auto, NOW), Some(BarMeter { used: 40, over_pace: false }));
    }

    #[test]
    fn the_meters_follow_the_provider() {
        let snapshot = Snapshot {
            rate_limits: Some(LimitsSnapshot { seven_day: Some(window(27.0, WEEKLY_WINDOW_MS, 0.5)), ..limits(Some(window(7.0, SESSION_WINDOW_MS, 0.2))) }),
            codex_limits: Some(codex_weekly(9.0, 0.1)),
            ..Snapshot::default()
        };
        let names = |provider| limit_models(&snapshot, provider, false, NOW).0.into_iter().map(|slot| format!("{} {}", slot.name, slot.span)).collect::<Vec<_>>();
        assert_eq!(names(Provider::Claude), ["Session 5h", "Weekly 7d"]);
        assert_eq!(names(Provider::Codex), ["Weekly 7d"]);
        assert_eq!(names(Provider::All), ["Session 5h", "Weekly 7d", "Codex 7d"]);
        let (_, note) = limit_models(&Snapshot::default(), Provider::Codex, false, NOW);
        assert_eq!(note.as_deref(), Some("Codex limits: none seen yet — run a Codex turn"));
    }

    #[test]
    fn a_codex_window_past_its_reset_is_unknown_not_zero_and_an_old_reading_is_shown_in_full() {
        let mut limits = codex_weekly(80.0, 0.5);
        limits.captured_at_ms = NOW - 3 * 3_600_000;
        limits.seven_day.as_mut().unwrap().resets_at_ms = Some(NOW - 1);
        let (slots, note) = limit_models(&Snapshot { codex_limits: Some(limits), ..Snapshot::default() }, Provider::Codex, false, NOW);
        assert!(slots[0].model.expired, "past its reset: unknown");
        assert!(!slots[0].dimmed, "an old Codex reading is still the latest: not dimmed");
        assert_eq!(note, None);
    }

    #[test]
    fn a_session_meter_shows_usage_against_the_clock() {
        let model = window_model(&window(18.0, SESSION_WINDOW_MS, 0.33), SESSION_WINDOW_MS, Some("$1.00 so far".into()), NOW);
        assert!((model.elapsed.unwrap() - 33.0).abs() < 0.01);
        assert!((model.pace.unwrap() + 15.0).abs() < 0.01, "15 points behind");
        assert_eq!(model.tone, Tone::Green);
        assert!((model.projected.unwrap() - 54.5).abs() < 0.1);
        assert_eq!(model.resets.as_deref(), Some("resets in 3h 20m"));
        assert_eq!(model.spend.as_deref(), Some("$1.00 so far"));
    }

    #[test]
    fn a_weekly_meter_ahead_of_pace_is_the_warning_and_projects_past_the_limit() {
        let model = window_model(&window(42.0, WEEKLY_WINDOW_MS, 0.14), WEEKLY_WINDOW_MS, None, NOW);
        assert!((model.pace.unwrap() - 28.0).abs() < 0.01, "28 points ahead");
        assert_eq!(model.tone, Tone::Orange);
        assert!(model.projected.unwrap() > 100.0);
        let resets = model.resets.expect("six days out");
        assert!(resets.starts_with("resets ") && !resets.starts_with("resets in"), "a reset days away is a moment: {resets}");
    }

    #[test]
    fn a_window_with_no_reset_time_has_only_its_usage() {
        let model = window_model(&LimitWindow { used_percent: 7.0, resets_at_ms: None, window_minutes: None }, SESSION_WINDOW_MS, None, NOW);
        assert_eq!((model.projected, model.elapsed, model.pace, model.resets), (None, None, None, None));
        assert_eq!(model.tone, Tone::Green);
    }

    #[test]
    fn the_reset_line_is_omitted_once_it_is_behind_us() {
        assert_eq!(reset_text(Some(NOW + 3_600_000), NOW).as_deref(), Some("resets in 1h"));
        assert_eq!(reset_text(Some(NOW + DAY_MS - 60_000), NOW).as_deref(), Some("resets in 23h 59m"));
        assert_eq!(reset_text(Some(NOW), NOW), None, "the reset instant itself is not a countdown");
        assert_eq!(reset_text(Some(NOW - 1), NOW), None);
        assert_eq!(reset_text(None, NOW), None);
    }

    fn stack_of(number: i64, entries: &[i64]) -> StackView {
        let entry = |n: i64| crate::stacks::StackEntry { number: n, branch: None, state: EntryState::Open, session: crate::stacks::EntrySession::None, checked: true };
        StackView { number: Some(number), repo_url: None, entries: entries.iter().copied().map(entry).collect() }
    }

    #[test]
    fn a_click_finds_its_stack_by_key_and_only_a_pr_still_in_it() {
        use crate::act::Target;
        let stacks = [stack_of(746, &[740, 741]), stack_of(800, &[801])];
        assert_ne!(stacks[0].key(), stacks[1].key(), "two stacks, two keys");
        assert_eq!(stacks[0].key(), stack_of(746, &[740, 799]).key(), "the same stack after a poll adds a PR on top");
        let found = |target: &Target| stack_for(&stacks, target).map(|(stack, pr)| (stack.number, pr));
        assert_eq!(found(&Target::Stack(stacks[1].key())), Some((Some(800), None)));
        assert_eq!(found(&Target::StackPr { stack: stacks[0].key(), pr: 741 }), Some((Some(746), Some(741))));
        assert_eq!(found(&Target::StackPr { stack: stacks[0].key(), pr: 801 }), None, "a PR of another stack");
        assert_eq!(found(&Target::Stack("gone".into())), None);
        assert_eq!(found(&Target::Session("x".into())), None);
    }

    #[test]
    fn a_prs_pills_say_the_same_facts_worded_for_where_they_are() {
        let ci = |state, failed: usize, pending| sl_core::gh::Ci { state, failed: vec![sl_core::gh::FailedJob { name: "j".into(), link: None }; failed], pending };
        let info = |ci| sl_core::gh::GhInfo { pr_number: Some(1), is_draft: Some(false), ci: Some(ci), ..sl_core::gh::GhInfo::default() };
        let texts = |info: &sl_core::gh::GhInfo, place| {
            let (pills, ci) = pr_pills(info, place);
            (pills.into_iter().map(|p| p.text).collect::<Vec<_>>(), ci.map(|p| p.text))
        };
        let failing = info(ci(sl_core::gh::CiState::Failing, 2, 0));
        assert_eq!(texts(&failing, PillsFor::Card), (vec!["Ready".to_owned()], Some("CI ✗ 2 failed".to_owned())));
        assert_eq!(texts(&failing, PillsFor::Header), (vec!["Ready for review".to_owned()], Some("CI ✗ 2".to_owned())));
        let pending = info(ci(sl_core::gh::CiState::Pending, 0, 3));
        assert_eq!(texts(&pending, PillsFor::Card).1.as_deref(), Some("CI … 3 running"));
        assert_eq!(texts(&pending, PillsFor::Header).1.as_deref(), Some("CI … 3"));
        let passing = info(ci(sl_core::gh::CiState::Passing, 0, 0));
        assert_eq!(texts(&passing, PillsFor::Card).1.as_deref(), Some("CI ✓"));
    }

    #[test]
    fn a_session_with_no_pr_has_its_branch_in_the_header() {
        let mut session = crate::fixture::load("full").unwrap().open.remove(0);
        session.gh = None;
        session.linear = None;
        session.branch = Some("feat/x".into());
        let header = header_content(&session);
        assert_eq!(header.title, "No PR yet");
        assert_eq!(header.symbol, Some(("arrow.triangle.branch", Tone::Dim)));
        assert_eq!(header.lines.len(), 1, "the branch");
        let detached = OpenSession { branch: Some("@abc1234".into()), ..session };
        assert_eq!(header_content(&detached).title, "Detached HEAD · no PR");
    }

    /// A closed session's submenu card: its name and PR, then where, when
    /// and which session it was.
    #[test]
    fn a_closed_sessions_header_names_it_and_says_when_it_closed() {
        let entry = crate::fixture::load("full").unwrap().closed.remove(0);
        let entry = Remembered { name: "fix it".into(), project: "proj".into(), branch: Some("feat/x".into()), pr_number: Some(9), session_id: "s1".into(), last_seen_ms: NOW - 3_600_000, ..entry };
        let header = closed_header(&entry, NOW);
        assert_eq!(header.symbol, Some(("clock.arrow.circlepath", Tone::Dim)));
        assert_eq!(header.title, "fix it · #9");
        let lines: Vec<String> = header.lines.iter().map(|line| line.iter().map(|seg| seg.text.as_str()).collect()).collect();
        assert_eq!(lines, [format!("proj · {}", crate::sticky::closed_ago(NOW - 3_600_000, NOW)), "feat/x".to_owned(), "s1".to_owned()]);
        assert_eq!(closed_header(&Remembered { pr_number: None, branch: None, ..entry }, NOW).title, "fix it", "no PR, just the name");
    }

    /// Under a PR's number: the base and its tip on one line, then the
    /// parent PR and "no reviews yet" on another, each line only when it
    /// has something to say.
    #[test]
    fn a_prs_header_lists_its_base_then_its_parent_and_reviews() {
        let mut session = crate::fixture::load("full").unwrap().open.remove(0);
        session.linear = None;
        let lines = |gh: sl_core::gh::GhInfo| {
            let header = header_content(&OpenSession { gh: Some(gh), ..session.clone() });
            assert_eq!(header.title, "#7");
            header.lines.iter().map(|line| line.iter().map(|seg| seg.text.as_str()).collect::<String>()).collect::<Vec<_>>()
        };
        let pr = sl_core::gh::GhInfo { pr_number: Some(7), ..sl_core::gh::GhInfo::default() };
        let symbol = |gh: sl_core::gh::GhInfo| header_content(&OpenSession { gh: Some(gh), ..session.clone() }).symbol;
        assert_eq!(symbol(pr.clone()), Some(("arrow.triangle.pull", Tone::Green)), "ready");
        assert_eq!(symbol(sl_core::gh::GhInfo { is_draft: Some(true), ..pr.clone() }), Some(("arrow.triangle.pull", Tone::Yellow)), "a draft");
        assert_eq!(lines(pr.clone()), ["no reviews yet"], "nothing but the missing reviews");
        let reviewed = |gh: sl_core::gh::GhInfo| sl_core::gh::GhInfo {
            reviews: vec![sl_core::gh::Review { author: "a".into(), state: sl_core::gh::ReviewState::Approved }],
            ..gh
        };
        assert!(lines(reviewed(pr.clone())).is_empty(), "reviewed, no base: no lines");
        let requested = sl_core::gh::GhInfo { requested: vec!["b".into()], ..pr.clone() };
        assert!(lines(requested).is_empty(), "a review asked for is not 'no reviews yet'");
        let based = sl_core::gh::GhInfo { base: Some("main".into()), base_sha: Some("abcdef1234".into()), base_pr: Some(6), ..pr.clone() };
        assert_eq!(lines(based.clone()), ["onto main · abcdef1", "stacked on #6 · no reviews yet"]);
        assert_eq!(lines(reviewed(based)), ["onto main · abcdef1", "stacked on #6"]);
    }
}
