/// Strips the escape sequences this program emits: SGR (`\x1b[...m`, from
/// owo-colors truecolor + dim/reset) and OSC 8 hyperlinks
/// (`\x1b]8;;URL\x1b\\`, ended by ST or BEL), whose URL is invisible.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.as_str().starts_with(']') {
                let mut prev = c;
                for next in chars.by_ref() {
                    if next == '\u{7}' || (prev == '\u{1b}' && next == '\\') {
                        break;
                    }
                    prev = next;
                }
                continue;
            }
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Visible width in the same units Claude Code's COLUMNS budget uses:
/// UTF-16 code units, matching JS string `.length`. Every glyph this
/// program renders (braille bars, ⚠, ✓, ✗, ⚡) lives in the Basic
/// Multilingual Plane, so a plain char count is equivalent.
pub fn visible_width(s: &str) -> usize {
    strip_ansi(s).chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_truecolor_and_reset_codes() {
        assert_eq!(strip_ansi("\x1b[38;2;230;230;230mhi\x1b[0m"), "hi");
    }

    #[test]
    fn strips_osc8_hyperlinks_keeping_the_label() {
        assert_eq!(strip_ansi("a \x1b]8;;https://x/1\x1b\\#730\x1b]8;;\x1b\\ b"), "a #730 b");
        assert_eq!(strip_ansi("\x1b]8;;https://x\x07go\x1b]8;;\x07"), "go");
    }

    #[test]
    fn passes_through_plain_text() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }
}
