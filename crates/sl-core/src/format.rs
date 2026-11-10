use crate::types::ResetsAt;
use owo_colors::{OwoColorize, Rgb};

pub const WHITE: Rgb = Rgb(0xe6, 0xe6, 0xe6);
pub const GREEN: Rgb = Rgb(0x4e, 0xc9, 0x4e);
pub const YELLOW: Rgb = Rgb(0xd7, 0xaf, 0x5f);
pub const RED: Rgb = Rgb(0xe0, 0x52, 0x52);
pub const BRIGHT_GREEN: Rgb = Rgb(0x5f, 0xff, 0x5f);
pub const BRIGHT_RED: Rgb = Rgb(0xff, 0x5f, 0x5f);
pub const CYAN: Rgb = Rgb(0x56, 0xb6, 0xc2);

pub fn colored(text: &str, color: Rgb) -> String {
    text.color(color).to_string()
}

pub fn dimmed(text: &str) -> String {
    text.dimmed().to_string()
}

/// Token counts the way the status line shows them: 46090 -> "46.1k",
/// 200000 -> "200k", 1000000 -> "1M", 950 -> "950". Rounded before the unit
/// is picked, so 999,960 reads "1M", not "1000.0k".
pub fn fmt_tokens(n: i64) -> String {
    compact_with(n as f64, &[(1e6, "M"), (1e3, "k")])
}

/// A count in a few characters: 812, 1.5K, 25.4M, 89.2B, with one decimal
/// at most and none when it is whole. Rounded before the unit is picked, so
/// 999,960 reads "1M" rather than "1000K".
pub fn compact_number(value: f64) -> String {
    compact_with(value, &[(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "K")])
}

fn compact_with(value: f64, units: &[(f64, &str)]) -> String {
    // `units` run largest first. The largest the value reaches is the unit,
    // unless rounding to one decimal makes it a thousand of them: 999,960 is
    // "1M", not "1000.0k".
    let round = |x: f64| (x * 10.0).round() / 10.0;
    let Some(at) = units.iter().position(|(scale, _)| value.abs() >= *scale) else {
        return trim_decimals(value, 0);
    };
    let (scale, unit) = match at.checked_sub(1).map(|up| units[up]) {
        Some((up, up_unit)) if round(value / units[at].0).abs() >= up / units[at].0 => {
            (up, up_unit)
        }
        _ => units[at],
    };
    format!("{}{unit}", trim_decimals(round(value / scale), 1))
}

/// `value` to at most `places` decimals, without trailing zeros: 2.50 ->
/// "2.5", 3.00 -> "3".
pub fn trim_decimals(value: f64, places: usize) -> String {
    let text = format!("{value:.places$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

/// `n` with its digits grouped in threes by `separator`: 1234567 ->
/// "1,234,567".
pub fn group_thousands(n: u64, separator: char) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * separator.len_utf8());
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(digit);
    }
    out
}

/// Dollars as a price: "$3,448.05", or compact from ten thousand up:
/// "$56.5K". A value that rounds to zero cents has no sign.
pub fn money(value: f64) -> String {
    if value.abs() >= 10_000.0 {
        return format!("${}", compact_number(value));
    }
    let cents = crate::num::trunc_u64((value.abs() * 100.0).round());
    let sign = if value < 0.0 && cents > 0 { "-" } else { "" };
    format!(
        "{sign}${}.{:02}",
        group_thousands(cents / 100, ','),
        cents % 100
    )
}

/// How long `ms` is, in its largest whole unit: "5m", "3h", "2d" ("0m"
/// under a minute; negative is "0m").
pub fn ago_short(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    match minutes {
        0..60 => format!("{minutes}m"),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

/// Elapsed milliseconds -> "45s", "5m", "26h 57m"
pub fn fmt_elapsed(ms: i64) -> String {
    let seconds = crate::num::trunc_i64(((ms as f64) / 1000.0).floor().max(0.0));
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let mins = minutes % 60;
    if hours > 0 {
        return if mins > 0 {
            format!("{hours}h {mins}m")
        } else {
            format!("{hours}h")
        };
    }
    format!("{mins}m")
}

/// Countdown to a rate-limit reset. Accepts epoch seconds or a fixed-offset
/// ISO 8601 string (the payload format has varied across Claude Code
/// versions). Returns None for missing/unparseable/past timestamps.
pub fn fmt_countdown(resets_at: Option<&ResetsAt>, now_ms: i64) -> Option<String> {
    let target_ms = resets_at_ms(resets_at?)?;
    if target_ms <= now_ms {
        return None;
    }
    let minutes = (target_ms - now_ms) / 60_000;
    let days = minutes / 1440;
    let hours = (minutes % 1440) / 60;
    let mins = minutes % 60;
    if days > 0 {
        return Some(format!("{days}d {hours}h"));
    }
    if hours > 0 {
        return Some(format!("{hours}h {mins}m"));
    }
    Some(format!("{mins}m"))
}

/// A `resets_at` as epoch milliseconds, whichever of its two shapes it arrived
/// in. `None` only for an ISO string that does not parse.
///
/// The one place the two shapes are collapsed, so the status line's countdown
/// and the tray's window arithmetic cannot disagree about what a payload's
/// reset time means.
pub fn resets_at_ms(resets_at: &ResetsAt) -> Option<i64> {
    match resets_at {
        ResetsAt::Epoch(secs) => Some(crate::num::trunc_i64(secs * 1000.0)),
        ResetsAt::Iso(s) => parse_iso8601_to_epoch_ms(s),
    }
}

/// Parses a fixed-offset ISO 8601 datetime (`2026-06-11T19:20:00+00:00`,
/// `...+0000`, `...+00` or `...Z`) into epoch milliseconds, without pulling in
/// a date/time crate.
pub fn parse_iso8601_to_epoch_ms(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || (bytes[10] != b'T' && bytes[10] != b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = s.get(19..)?;
    let mut frac_ms: i64 = 0;
    if let Some(stripped) = rest.strip_prefix('.') {
        let frac_len = stripped
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(stripped.len());
        let frac_digits = &stripped.as_bytes()[..frac_len];
        // Pad/truncate the fractional-second digits to exactly 3 (milliseconds):
        // ".123" -> 123ms, ".1" -> 100ms, ".123456" -> 123ms (sub-ms precision dropped).
        let digit_at = |i: usize| frac_digits.get(i).copied().unwrap_or(b'0') - b'0';
        frac_ms =
            i64::from(digit_at(0)) * 100 + i64::from(digit_at(1)) * 10 + i64::from(digit_at(2));
        rest = stripped.get(frac_len..)?;
    }

    let offset_minutes: i64 = if rest.is_empty() || rest == "Z" {
        0
    } else {
        let (sign, unsigned): (i64, &str) = match rest.strip_prefix('-') {
            Some(r) => (-1, r),
            None => (1, rest.strip_prefix('+')?),
        };
        // `hh:mm`, or the basic forms `hhmm` and `hh`.
        let minutes = match unsigned.len() {
            5 if unsigned.get(2..3) == Some(":") => unsigned.get(3..5)?,
            4 => unsigned.get(2..4)?,
            2 => "00",
            _ => return None,
        };
        let oh: i64 = unsigned.get(0..2)?.parse().ok()?;
        let om: i64 = minutes.parse().ok()?;
        sign * (oh * 60 + om)
    };

    let days = days_from_civil(year, month, day);
    let epoch_seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some(epoch_seconds * 1000 + frac_ms)
}

/// Howard Hinnant's days-from-civil algorithm: days since the Unix epoch for
/// a given Gregorian calendar date, valid for any year (including negative).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// Braille gradient, empty -> full. Each bar cell has (length - 1) sub-levels.
const GRADIENT: [&str; 7] = ["⣀", "⣄", "⣤", "⣦", "⣶", "⣷", "⣿"];
const EMPTY_CHAR: &str = "⣀";

pub fn pct_color(pct: f64) -> Rgb {
    if pct >= 80.0 {
        RED
    } else if pct >= 50.0 {
        YELLOW
    } else {
        GREEN
    }
}

/// Braille progress bar; partially-filled cells use gradient glyphs, empty cells render dim.
pub fn make_bar(pct: f64, width: usize, plain: bool) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    let levels = GRADIENT.len() - 1;
    let total_steps = width * levels;
    let filled_steps = crate::num::trunc_usize(((clamped / 100.0) * total_steps as f64).round());
    let full = filled_steps / levels;
    let partial = filled_steps % levels;
    let empty = width - full - usize::from(partial > 0);

    let fill = GRADIENT[levels].repeat(full) + if partial > 0 { GRADIENT[partial] } else { "" };
    let rest = EMPTY_CHAR.repeat(empty);
    if plain {
        return format!("{fill}{rest}");
    }
    format!("{}{}", colored(&fill, pct_color(clamped)), dimmed(&rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_before_they_pick_a_unit() {
        assert_eq!(fmt_tokens(999_960), "1M", "was 1000.0k");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(999_949), "999.9k");
        assert_eq!(compact_number(950.0), "950", "was 0.9K");
        assert_eq!(fmt_tokens(1_050_000), "1.1M");
    }

    #[test]
    fn compact_keeps_one_decimal_and_carries_into_the_next_unit() {
        assert_eq!(compact_number(812.0), "812");
        assert_eq!(compact_number(1_500.0), "1.5K");
        assert_eq!(compact_number(25_400_000.0), "25.4M");
        assert_eq!(compact_number(3_000_000.0), "3M");
        assert_eq!(compact_number(999_960.0), "1M");
        assert_eq!(compact_number(89_200_000_000.0), "89.2B");
        assert_eq!(compact_number(1.5e12), "1.5T");
        assert_eq!(compact_number(-1_500.0), "-1.5K");
    }

    #[test]
    fn decimals_are_trimmed_but_whole_numbers_keep_their_zeros() {
        assert_eq!(trim_decimals(2.5, 2), "2.5");
        assert_eq!(trim_decimals(3.0, 2), "3");
        assert_eq!(trim_decimals(250.0, 0), "250");
        assert_eq!(
            trim_decimals(1.25, 1),
            "1.2",
            "rounded to even, as format! does"
        );
    }

    #[test]
    fn thousands_are_grouped_by_the_separator_given() {
        assert_eq!(group_thousands(0, ','), "0");
        assert_eq!(group_thousands(999, ','), "999");
        assert_eq!(group_thousands(1_000, ','), "1,000");
        assert_eq!(group_thousands(433_498, '\u{202f}'), "433\u{202f}498");
        assert_eq!(group_thousands(1_234_567, ','), "1,234,567");
    }

    #[test]
    fn money_reads_like_a_price_and_compacts_from_ten_thousand() {
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(62.215), "$62.22");
        assert_eq!(money(3448.05), "$3,448.05");
        assert_eq!(money(9_999.994), "$9,999.99");
        assert_eq!(money(10_000.0), "$10K");
        assert_eq!(money(56_464.7), "$56.5K");
        assert_eq!(money(-0.001), "$0.00");
        assert_eq!(money(-2.5), "-$2.50");
    }

    #[test]
    fn ago_is_the_largest_whole_unit() {
        assert_eq!(ago_short(-5), "0m");
        assert_eq!(ago_short(59_999), "0m");
        assert_eq!(ago_short(5 * 60_000), "5m");
        assert_eq!(ago_short(59 * 60_000), "59m");
        assert_eq!(ago_short(60 * 60_000), "1h");
        assert_eq!(ago_short(3 * 3_600_000 + 60_000), "3h");
        assert_eq!(ago_short(24 * 3_600_000 - 1), "23h");
        assert_eq!(ago_short(2 * 86_400_000), "2d");
    }
    use crate::ansi::strip_ansi;

    #[test]
    fn fmt_tokens_plain_numbers_under_1k() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(950), "950");
    }

    #[test]
    fn fmt_tokens_thousands() {
        assert_eq!(fmt_tokens(46_090), "46.1k");
        assert_eq!(fmt_tokens(200_000), "200k");
        assert_eq!(fmt_tokens(600_000), "600k");
    }

    #[test]
    fn fmt_tokens_millions() {
        assert_eq!(fmt_tokens(1_000_000), "1M");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn fmt_elapsed_seconds_minutes_hours() {
        assert_eq!(fmt_elapsed(45_000), "45s");
        assert_eq!(fmt_elapsed(5 * 60_000), "5m");
        assert_eq!(fmt_elapsed((26 * 60 + 57) * 60_000), "26h 57m");
        assert_eq!(fmt_elapsed(2 * 3_600_000), "2h");
    }

    #[test]
    fn fmt_elapsed_never_negative() {
        assert_eq!(fmt_elapsed(-5_000), "0s");
    }

    fn iso_ms(s: &str) -> i64 {
        parse_iso8601_to_epoch_ms(s).expect("valid iso date in test fixture")
    }

    #[test]
    fn fmt_countdown_iso_string() {
        let now = iso_ms("2026-06-11T14:34:00Z");
        assert_eq!(
            fmt_countdown(
                Some(&ResetsAt::Iso("2026-06-11T19:20:00+00:00".into())),
                now
            )
            .as_deref(),
            Some("4h 46m")
        );
    }

    #[test]
    fn fmt_countdown_epoch_seconds() {
        let now = iso_ms("2026-06-11T14:34:00Z");
        let target = iso_ms("2026-06-12T04:00:00Z") / 1000;
        assert_eq!(
            fmt_countdown(Some(&ResetsAt::Epoch(target as f64)), now).as_deref(),
            Some("13h 26m")
        );
    }

    #[test]
    fn fmt_countdown_days_for_long_windows() {
        let now = iso_ms("2026-06-11T14:34:00Z");
        assert_eq!(
            fmt_countdown(Some(&ResetsAt::Iso("2026-06-13T20:00:00Z".into())), now).as_deref(),
            Some("2d 5h")
        );
    }

    #[test]
    fn fmt_countdown_null_for_past_missing_or_garbage() {
        let now = iso_ms("2026-06-11T14:34:00Z");
        assert_eq!(
            fmt_countdown(Some(&ResetsAt::Iso("2026-06-11T10:00:00Z".into())), now),
            None
        );
        assert_eq!(fmt_countdown(None, now), None);
        assert_eq!(
            fmt_countdown(Some(&ResetsAt::Iso("not a date".into())), now),
            None
        );
    }

    #[test]
    fn microsecond_precision_and_a_plus_zero_offset_parse() {
        // The shape `~/.claude.json`'s cached utilization carries: six
        // fractional digits and an explicit `+00:00` rather than `Z`.
        let with_micros = parse_iso8601_to_epoch_ms("2026-08-24T14:09:59.911461+00:00")
            .expect("microseconds parse");
        let whole_second =
            parse_iso8601_to_epoch_ms("2026-08-24T14:09:59Z").expect("plain seconds parse");
        // Sub-millisecond precision is dropped, not rounded: .911461 -> 911ms.
        assert_eq!(with_micros - whole_second, 911);
        assert_eq!(
            parse_iso8601_to_epoch_ms("2026-08-24T14:09:59.911461+00:00"),
            parse_iso8601_to_epoch_ms("2026-08-24T14:09:59.911Z")
        );
    }

    #[test]
    fn resets_at_ms_collapses_both_payload_shapes() {
        // Claude Code 2.1.258 sends epoch seconds; older builds sent ISO.
        assert_eq!(
            resets_at_ms(&ResetsAt::Epoch(1_787_566_377.0)),
            Some(1_787_566_377_000)
        );
        assert_eq!(
            resets_at_ms(&ResetsAt::Iso("2026-08-24T14:09:59.911461+00:00".into())),
            Some(1_787_580_599_911)
        );
        assert_eq!(resets_at_ms(&ResetsAt::Iso("not a date".into())), None);
    }

    #[test]
    fn every_offset_form_keeps_its_minutes_and_anything_else_is_refused() {
        let extended = iso_ms("2026-01-01T05:30:00+05:30");
        assert_eq!(extended, iso_ms("2026-01-01T00:00:00Z"));
        assert_eq!(iso_ms("2026-01-01T05:30:00+0530"), extended, "basic form");
        assert_eq!(iso_ms("2026-01-01T05:00:00+05"), extended, "hours only");
        assert_eq!(
            iso_ms("2025-12-31T18:30:00-0530"),
            extended,
            "a negative basic offset"
        );
        for broken in [
            "2026-01-01T05:30:00+05:30garbage",
            "2026-01-01T05:30:00+053",
            "2026-01-01T05:30:00+5",
            "2026-01-01T05:30:00+05-30",
        ] {
            assert_eq!(parse_iso8601_to_epoch_ms(broken), None, "{broken}");
        }
    }

    #[test]
    fn parse_iso8601_to_epoch_ms_fractional_seconds() {
        let base = iso_ms("2024-01-15T10:30:00Z");
        assert_eq!(iso_ms("2024-01-15T10:30:00.123Z"), base + 123);
        assert_eq!(iso_ms("2024-01-15T10:30:00.1Z"), base + 100);
        assert_eq!(iso_ms("2024-01-15T10:30:00.123456Z"), base + 123);
    }

    #[test]
    fn make_bar_empty_and_full() {
        assert_eq!(make_bar(0.0, 6, true), "⣀⣀⣀⣀⣀⣀");
        assert_eq!(make_bar(100.0, 6, true), "⣿⣿⣿⣿⣿⣿");
    }

    #[test]
    fn make_bar_partial_fill_uses_gradient_glyphs() {
        let bar = make_bar(50.0, 6, true);
        assert_eq!(bar.chars().count(), 6);
        assert!(bar.starts_with("⣿⣿⣿"));
    }

    #[test]
    fn make_bar_clamps_out_of_range_percentages() {
        assert_eq!(make_bar(250.0, 6, true), "⣿⣿⣿⣿⣿⣿");
        assert_eq!(make_bar(-10.0, 6, true), "⣀⣀⣀⣀⣀⣀");
    }

    #[test]
    fn make_bar_colored_strips_back_to_plain() {
        assert_eq!(
            strip_ansi(&make_bar(35.0, 6, false)),
            make_bar(35.0, 6, true)
        );
    }
}
