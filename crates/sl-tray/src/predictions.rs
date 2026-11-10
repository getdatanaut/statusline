//! Render-time arithmetic: projections, deltas and the small guarded
//! divisions the menu's rows are built out of.
//!
//! Split out of `controller.rs` on purpose. Everything here needs a live
//! clock (`now_ms` at repaint time, not the background thread's snapshot
//! time), so it cannot live in `snapshot.rs`; but it is also the only real
//! *math* in the repaint path, and math buried in AppKit glue is math nobody
//! can test. So the glue calls into here and here holds no `objc2` types.
//!
//! Every division in this module is guarded: a fresh install with no usage at
//! all reaches all of these with zeroes, and `0.0 / 0.0` renders as `NaN` in a
//! menu bar rather than panicking, which is worse than useless.

use chrono::{Datelike, NaiveDate};

/// Milliseconds in a minute. Local to this module: the block countdown is the
/// only place it is needed.
const MILLIS_PER_MINUTE: i64 = 60_000;

// --------------------------------------------------------------------------
// Guarded division
// --------------------------------------------------------------------------

/// `numerator / denominator`, or `None` when the denominator is zero or the
/// result is not finite. The one place a divide-by-zero is decided, so no
/// caller has to remember to guard.
pub fn safe_div(numerator: f64, denominator: f64) -> Option<f64> {
    if denominator == 0.0 {
        return None;
    }
    let quotient = numerator / denominator;
    quotient.is_finite().then_some(quotient)
}

/// Cost per million tokens. `None` for a zero token count.
pub fn cost_per_mtok(cost: f64, tokens: u64) -> Option<f64> {
    safe_div(cost, tokens as f64 / 1_000_000.0)
}

/// `part` as a percentage of `whole`. `None` for a zero whole.
pub fn percent_of(part: f64, whole: f64) -> Option<f64> {
    safe_div(part * 100.0, whole)
}

/// Output-to-input token ratio. `None` when no input tokens were recorded
/// (a corpus of pure cache reads, or no usage at all).
pub fn output_input_ratio(output_tokens: u64, input_tokens: u64) -> Option<f64> {
    safe_div(output_tokens as f64, input_tokens as f64)
}

// --------------------------------------------------------------------------
// Calendar position (for the monthly projection)
// --------------------------------------------------------------------------

/// Number of days in `month` of `year`: the day-of-month of the day before
/// the first of the following month. `None` only for a month outside
/// chrono's representable range.
fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    Some(NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()?.day())
}

/// `(day_of_month, days_in_month)` for a `YYYY-MM-DD` date string. `None` for
/// a malformed date (which `UsageRecord::date` never produces, but the
/// snapshot's dates are plain strings and this is the boundary).
pub fn month_position(date: &str) -> Option<(u32, u32)> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some((parsed.day(), days_in_month(parsed.year(), parsed.month())?))
}

/// Linear extrapolation of this month's spend to the end of the month:
/// `(month_cost / day_of_month) * days_in_month`.
///
/// Deliberately naive -- it assumes the rest of the month looks like the part
/// already spent, which is exactly what the plan specifies. The `max(day, 1)`
/// in the plan's formula is unnecessary here because `day_of_month` is never
/// zero, but the guarded division keeps it safe regardless.
pub fn monthly_projection(month_cost: f64, today_date: &str) -> Option<f64> {
    let (day, days) = month_position(today_date)?;
    let per_day = safe_div(month_cost, f64::from(day.max(1)))?;
    Some(per_day * f64::from(days))
}

// --------------------------------------------------------------------------
// Current block projection
// --------------------------------------------------------------------------

/// Whole minutes left in the block, clamped at zero: a block whose window has
/// already closed has no negative time left, it has none.
pub fn remaining_minutes(end_time_ms: i64, now_ms: i64) -> i64 {
    end_time_ms.saturating_sub(now_ms).max(0) / MILLIS_PER_MINUTE
}

/// What the block will have cost by the time its 5-hour window closes, if the
/// current burn rate holds: `cost_so_far + (cost_per_hour / 60) * minutes_left`.
///
/// A block with no burn rate (a single entry, or every entry sharing one
/// timestamp -- see `sl_usage::aggregate::block_burn_rate`) projects to what
/// it has already cost rather than to nothing: the honest statement is "we
/// cannot extrapolate yet", not "$0.00 more".
pub fn block_projection(cost_so_far: f64, cost_per_hour: Option<f64>, end_time_ms: i64, now_ms: i64) -> f64 {
    let Some(per_hour) = cost_per_hour else {
        return cost_so_far;
    };
    cost_so_far + (per_hour / 60.0) * remaining_minutes(end_time_ms, now_ms) as f64
}

// --------------------------------------------------------------------------
// Rate-limit windows
// --------------------------------------------------------------------------

/// The 5-hour session window, in milliseconds.
///
/// The payload never says how long a window is -- only when it next resets --
/// so a window's start has to be derived as `resets_at - length`. That is fine
/// because the length is what each window is *named* after (`five_hour`,
/// `seven_day`), but it does mean a server that ever changes one would need
/// its constant changed with it.
pub const SESSION_WINDOW_MS: i64 = 5 * 60 * 60 * 1000;

/// The 7-day weekly window, in milliseconds. See [`SESSION_WINDOW_MS`].
pub const WEEKLY_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How old a rate-limit capture may be before the title stops presenting it as
/// live.
///
/// The hook only writes while Claude Code is actually running a turn, so an
/// idle machine freezes the percentage at whatever it was hours ago. Two
/// minutes is comfortably longer than any single turn's gap between status
/// line refreshes and short enough that a closed laptop is marked within the
/// same minute it is noticed.
pub const CAPTURE_STALE_AFTER_MS: i64 = 2 * 60 * 1000;

/// Whether a capture is too old to present as live. Exactly at the threshold
/// is still fresh; the marker is for captures that have stopped arriving, and
/// one that landed precisely `CAPTURE_STALE_AFTER_MS` ago has not.
pub fn capture_is_stale(captured_at_ms: i64, now_ms: i64) -> bool {
    now_ms.saturating_sub(captured_at_ms) > CAPTURE_STALE_AFTER_MS
}

/// How far into a window of `window_ms` `now_ms` is, as a percentage of the
/// window: `(now - (resets_at - window)) / window * 100`.
///
/// `None` when no time has elapsed in the window at all -- a `resets_at` a full
/// window or more away, which means either a clock disagreement or a capture
/// from a window that has not started. There is nothing to extrapolate from
/// zero elapsed time, and saying so is the point of the `Option`.
pub fn window_elapsed_percent(window_ms: i64, resets_at_ms: i64, now_ms: i64) -> Option<f64> {
    let window_start_ms = resets_at_ms.saturating_sub(window_ms);
    let elapsed = percent_of(now_ms.saturating_sub(window_start_ms) as f64, window_ms as f64)?;
    (elapsed > 0.0).then_some(elapsed)
}

/// Where a window's utilization is heading if the rest of it looks like the
/// part already spent: `used_percent / elapsed_percent * 100`.
///
/// Uncapped on the way up -- a projection over 100% is the entire reason to
/// show one. The *elapsed* percentage is capped at 100 though: past the reset
/// time the window is closed, nothing more can land in it, and dividing by
/// `120` would render the finished window as a projection *below* what it
/// actually used, which is worse than showing nothing.
///
/// `None` when [`window_elapsed_percent`] has nothing to say -- the division
/// itself goes through [`safe_div`], so there is exactly one guarded division
/// here and no new `/` to get wrong.
pub fn window_projection(used_percent: f64, window_ms: i64, resets_at_ms: i64, now_ms: i64) -> Option<f64> {
    let elapsed = window_elapsed_percent(window_ms, resets_at_ms, now_ms)?.min(100.0);
    safe_div(used_percent * 100.0, elapsed)
}

/// How far usage is ahead of an even burn across the window, in percentage
/// points: `used - elapsed`. Positive is ahead of pace (heading past the
/// limit before the reset), negative is behind it (headroom left over).
///
/// Elapsed is capped at 100 for the same reason as [`window_projection`]: a
/// closed window's pace is what it used against the whole of it.
pub fn window_pace(used_percent: f64, window_ms: i64, resets_at_ms: i64, now_ms: i64) -> Option<f64> {
    let elapsed = window_elapsed_percent(window_ms, resets_at_ms, now_ms)?.min(100.0);
    Some(used_percent - elapsed)
}

#[cfg(test)]
#[expect(clippy::float_cmp, reason = "each comparison is of a value exact by construction: a whole number or a half")]
mod tests {
    use super::*;

    /// An arbitrary window reset instant; [`at_elapsed`] places "now" inside
    /// the five hours leading up to it.
    const RESETS_AT: i64 = 1_788_200_000_000;

    /// The moment `fraction` of the way through the window ending at
    /// [`RESETS_AT`]: `0.0` is the window opening, `1.0` the reset itself.
    fn at_elapsed(fraction: f64) -> i64 {
        RESETS_AT - SESSION_WINDOW_MS + sl_core::num::trunc_i64(SESSION_WINDOW_MS as f64 * fraction)
    }

    #[test]
    fn window_elapsed_percent_measures_from_the_derived_window_start() {
        assert_eq!(window_elapsed_percent(SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.5)), Some(50.0));
        assert_eq!(window_elapsed_percent(SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.1)), Some(10.0));
        // At the reset instant the window is exactly full.
        assert_eq!(window_elapsed_percent(SESSION_WINDOW_MS, RESETS_AT, RESETS_AT), Some(100.0));
    }

    #[test]
    fn window_elapsed_percent_is_none_before_the_window_starts() {
        // Zero elapsed: the divisor the projection would use.
        assert_eq!(window_elapsed_percent(SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.0)), None);
        // A `resets_at` further out than the window is long -- clock skew, or
        // a window that has not opened.
        assert_eq!(window_elapsed_percent(SESSION_WINDOW_MS, RESETS_AT, RESETS_AT - SESSION_WINDOW_MS - 60_000), None);
    }

    #[test]
    fn window_projection_extrapolates_linearly() {
        // 10% used a tenth of the way in projects to the whole window.
        assert_eq!(window_projection(10.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.1)), Some(100.0));
        // 7% used a ninth of the way in -> ~63%, the shape the menu bar shows.
        let projected = window_projection(7.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(1.0 / 9.0)).expect("projects");
        assert!((projected - 63.0).abs() < 1.0, "expected roughly 63%, got {projected}");
    }

    #[test]
    fn window_projection_is_uncapped_above_one_hundred() {
        // Burning 40% in the first tenth of the window is heading well past
        // the limit, and the title has to say so rather than clamp to 100.
        assert_eq!(window_projection(40.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.1)), Some(400.0));
    }

    #[test]
    fn window_projection_is_none_with_no_elapsed_time() {
        assert_eq!(window_projection(10.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.0)), None, "the divide-by-zero case");
        assert_eq!(window_projection(0.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.0)), None);
    }

    #[test]
    fn a_window_that_has_already_reset_projects_to_what_it_used() {
        // Past the reset the window is closed: nothing more can land in it, so
        // the projection is the utilization itself rather than a number below
        // it (which is what an uncapped `used / 150 * 100` would give).
        assert_eq!(window_projection(43.0, SESSION_WINDOW_MS, RESETS_AT, RESETS_AT + SESSION_WINDOW_MS / 2), Some(43.0));
        assert_eq!(window_projection(43.0, SESSION_WINDOW_MS, RESETS_AT, RESETS_AT), Some(43.0), "the boundary instant is already full");
    }

    #[test]
    fn zero_usage_projects_to_zero_rather_than_nothing() {
        assert_eq!(window_projection(0.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.5)), Some(0.0));
    }

    #[test]
    fn the_weekly_window_measures_against_seven_days() {
        // Two days into the week.
        let now = RESETS_AT - WEEKLY_WINDOW_MS + 2 * 24 * 3_600_000;
        let elapsed = window_elapsed_percent(WEEKLY_WINDOW_MS, RESETS_AT, now).expect("inside the week");
        assert!((elapsed - 200.0 / 7.0).abs() < 1e-9, "2 of 7 days, got {elapsed}");
        // A reset four days out is outside a 5-hour window but well inside a week.
        assert_eq!(window_elapsed_percent(SESSION_WINDOW_MS, RESETS_AT, RESETS_AT - 4 * 24 * 3_600_000), None);
    }

    #[test]
    fn pace_is_used_minus_elapsed() {
        // 18% used 33% of the way in: 15 points behind.
        let behind = window_pace(18.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.33)).expect("paced");
        assert!((behind + 15.0).abs() < 1e-6, "got {behind}");
        // 42% used 14% of the way in: 28 points ahead.
        let ahead = window_pace(42.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.14)).expect("paced");
        assert!((ahead - 28.0).abs() < 1e-6, "got {ahead}");
        assert_eq!(window_pace(10.0, SESSION_WINDOW_MS, RESETS_AT, at_elapsed(0.0)), None, "no time elapsed, no pace");
        // A closed window is judged against the whole of it.
        assert_eq!(window_pace(43.0, SESSION_WINDOW_MS, RESETS_AT, RESETS_AT + 60_000), Some(-57.0));
    }

    #[test]
    fn capture_staleness_is_measured_from_the_capture_not_the_snapshot() {
        let captured = 1_788_200_000_000;
        assert!(!capture_is_stale(captured, captured), "a capture from this instant is fresh");
        assert!(!capture_is_stale(captured, captured + CAPTURE_STALE_AFTER_MS - 1));
        assert!(!capture_is_stale(captured, captured + CAPTURE_STALE_AFTER_MS), "exactly at the threshold is still fresh");
        assert!(capture_is_stale(captured, captured + CAPTURE_STALE_AFTER_MS + 1));
        // Overnight, the case the marker exists for.
        assert!(capture_is_stale(captured, captured + 8 * 3_600_000));
        // A capture from the future (clock adjustment) is not stale.
        assert!(!capture_is_stale(captured, captured - 60_000));
    }

    #[test]
    fn safe_div_guards_zero_and_non_finite() {
        assert_eq!(safe_div(10.0, 4.0), Some(2.5));
        assert_eq!(safe_div(0.0, 0.0), None, "the empty-corpus case: not NaN");
        assert_eq!(safe_div(1.0, 0.0), None);
        assert_eq!(safe_div(f64::INFINITY, 1.0), None, "non-finite results are not rendered");
    }

    #[test]
    fn cost_per_mtok_scales_by_a_million_and_guards_zero_tokens() {
        assert_eq!(cost_per_mtok(3.0, 1_000_000), Some(3.0));
        assert_eq!(cost_per_mtok(3.0, 500_000), Some(6.0));
        assert_eq!(cost_per_mtok(0.0, 0), None, "a day with no tokens has no price per token");
    }

    #[test]
    fn percent_and_ratio_guard_zero_denominators() {
        assert_eq!(percent_of(25.0, 200.0), Some(12.5));
        assert_eq!(percent_of(1.0, 0.0), None);
        assert_eq!(output_input_ratio(300, 100), Some(3.0));
        assert_eq!(output_input_ratio(300, 0), None, "cache-only usage has no input to divide by");
        assert_eq!(output_input_ratio(0, 0), None);
    }

    #[test]
    fn month_position_covers_short_leap_and_long_months() {
        assert_eq!(month_position("2026-08-31"), Some((31, 31)));
        assert_eq!(month_position("2026-02-01"), Some((1, 28)));
        assert_eq!(month_position("2028-02-10"), Some((10, 29)), "2028 is a leap year");
        assert_eq!(month_position("2026-12-31"), Some((31, 31)), "December rolls the year over");
        assert_eq!(month_position("not-a-date"), None);
    }

    #[test]
    fn monthly_projection_extrapolates_linearly() {
        // $10 over the first 10 days of a 31-day month -> $31.
        assert_eq!(monthly_projection(10.0, "2026-08-10"), Some(31.0));
        // On the last day the projection is just the actual.
        assert_eq!(monthly_projection(42.0, "2026-08-31"), Some(42.0));
        assert_eq!(monthly_projection(0.0, "2026-08-10"), Some(0.0), "no spend projects to no spend");
        assert_eq!(monthly_projection(10.0, "garbage"), None);
    }

    #[test]
    fn remaining_minutes_never_goes_negative() {
        let now = 1_000_000_000_000;
        assert_eq!(remaining_minutes(now + 90 * MILLIS_PER_MINUTE, now), 90);
        assert_eq!(remaining_minutes(now - MILLIS_PER_MINUTE, now), 0, "a closed window has no time left");
        assert_eq!(remaining_minutes(now, now), 0);
    }

    #[test]
    fn block_projection_adds_the_burn_rate_over_the_time_left() {
        let now = 1_000_000_000_000;
        let end = now + 120 * MILLIS_PER_MINUTE;
        // $6/hr for two more hours on top of $1 already spent.
        assert_eq!(block_projection(1.0, Some(6.0), end, now), 13.0);
        // No burn rate yet (single-entry block): project what is already spent.
        assert_eq!(block_projection(1.0, None, end, now), 1.0);
        // Window already closed: nothing left to extrapolate over.
        assert_eq!(block_projection(1.0, Some(6.0), now - 1, now), 1.0);
    }
}
