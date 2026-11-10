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

/// `text` without control characters, for text from elsewhere (a CI job's
/// name, a link GitHub was given) that must not reach the terminal as an
/// escape sequence of its own.
pub fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Visible width in terminal columns, as Claude Code lays its status rows
/// out: a wide character such as `⚡` or an emoji in a branch name takes two.
pub fn visible_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip_ansi(s).as_str())
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
        assert_eq!(
            strip_ansi("a \x1b]8;;https://x/1\x1b\\#730\x1b]8;;\x1b\\ b"),
            "a #730 b"
        );
        assert_eq!(strip_ansi("\x1b]8;;https://x\x07go\x1b]8;;\x07"), "go");
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(visible_width("\x1b[2m⚡high\x1b[0m"), 6);
        assert_eq!(visible_width("feat/🚀-launch"), 14);
        assert_eq!(visible_width("⠿⠤ ✓ ✗ ⚠"), 8, "narrow glyphs take one");
    }

    #[test]
    fn passes_through_plain_text() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }
}
