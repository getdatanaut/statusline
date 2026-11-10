//! A small markdown-to-styled-runs renderer for the review popups.
//!
//! `pulldown-cmark` does the parsing; this turns its events into flat
//! [`Run`]s of text plus a [`Style`], which the controller maps onto an
//! `NSAttributedString`. Keeping the AppKit side out of here keeps this
//! testable. It covers what review feedback uses -- headings, emphasis, inline
//! and fenced code (with coloured `diff` lines), lists, block quotes, links,
//! rules and tables -- and drops raw HTML such as comments and `<details>`.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::sessions::Tone;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    /// Monospaced, on a tinted background.
    pub code: bool,
    /// `1..=6`; `0` for body text.
    pub heading: u8,
    pub link: Option<String>,
    pub tone: Option<Tone>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

struct Builder {
    runs: Vec<Run>,
    bold: u32,
    italic: u32,
    quote: u32,
    heading: u8,
    link: Option<String>,
    /// One entry per open list: the next ordinal, or `None` for a bullet list.
    lists: Vec<Option<u64>>,
    code_lang: Option<String>,
    in_code_block: bool,
    in_table_head: bool,
    cells_in_row: usize,
}

impl Builder {
    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0 || self.heading > 0 || self.in_table_head,
            italic: self.italic > 0,
            code: false,
            heading: self.heading,
            link: self.link.clone(),
            tone: (self.quote > 0).then_some(Tone::Dim),
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        match self.runs.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => self.runs.push(Run { text: text.to_owned(), style }),
        }
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        self.push(text, style);
    }

    /// Start a new block: a blank line after whatever came before, unless
    /// nothing did or one is already there.
    fn block_break(&mut self) {
        let tail: String = self.runs.iter().rev().flat_map(|r| r.text.chars().rev()).take(2).collect();
        match (self.runs.is_empty(), tail.as_str()) {
            (true, _) | (_, "\n\n") => {}
            (_, t) if t.starts_with('\n') => self.text("\n"),
            _ => self.text("\n\n"),
        }
    }

    fn line_break(&mut self) {
        let ends_with_newline = self.runs.last().is_some_and(|r| r.text.ends_with('\n'));
        if !ends_with_newline && !self.runs.is_empty() {
            self.text("\n");
        }
    }

    fn code_block(&mut self, body: &str) {
        let diff = self.code_lang.as_deref() == Some("diff");
        for line in body.trim_end_matches('\n').split('\n') {
            let tone = if diff {
                match line.chars().next() {
                    Some('+') => Some(Tone::Green),
                    Some('-') => Some(Tone::Red),
                    Some('@') => Some(Tone::Dim),
                    _ => None,
                }
            } else {
                None
            };
            self.push(&format!("{line}\n"), Style { code: true, tone, ..Style::default() });
        }
    }
}

fn level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

pub fn parse(markdown: &str) -> Vec<Run> {
    let mut b = Builder {
        runs: Vec::new(),
        bold: 0,
        italic: 0,
        quote: 0,
        heading: 0,
        link: None,
        lists: Vec::new(),
        code_lang: None,
        in_code_block: false,
        in_table_head: false,
        cells_in_row: 0,
    };
    let mut code_body = String::new();
    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level: l, .. } => {
                    b.block_break();
                    b.heading = level(l);
                }
                Tag::Paragraph => {
                    if b.lists.is_empty() {
                        b.block_break();
                    }
                }
                Tag::BlockQuote(_) => {
                    b.block_break();
                    b.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    b.block_break();
                    b.in_code_block = true;
                    code_body.clear();
                    b.code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => Some(lang.split_whitespace().next().unwrap_or("").to_owned()),
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::List(start) => {
                    if b.lists.is_empty() {
                        b.block_break();
                    }
                    b.lists.push(start);
                }
                Tag::Item => {
                    b.line_break();
                    let depth = b.lists.len().saturating_sub(1);
                    let marker = match b.lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        _ => "• ".to_owned(),
                    };
                    b.push(&format!("{}{marker}", "    ".repeat(depth)), Style::default());
                }
                Tag::Emphasis => b.italic += 1,
                Tag::Strong => b.bold += 1,
                Tag::Link { dest_url, .. } => b.link = Some(dest_url.into_string()),
                Tag::Table(_) => b.block_break(),
                Tag::TableHead => {
                    b.in_table_head = true;
                    b.cells_in_row = 0;
                }
                Tag::TableRow => b.cells_in_row = 0,
                Tag::TableCell => {
                    if b.cells_in_row > 0 {
                        b.push("  │  ", Style { tone: Some(Tone::Dim), ..Style::default() });
                    }
                    b.cells_in_row += 1;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    b.heading = 0;
                    b.text("\n");
                }
                TagEnd::Paragraph => {
                    if b.lists.is_empty() {
                        b.text("\n");
                    } else {
                        b.line_break();
                    }
                }
                TagEnd::BlockQuote(_) => b.quote = b.quote.saturating_sub(1),
                TagEnd::CodeBlock => {
                    b.in_code_block = false;
                    let body = std::mem::take(&mut code_body);
                    b.code_block(&body);
                    b.code_lang = None;
                }
                TagEnd::List(_) => {
                    b.lists.pop();
                }
                TagEnd::Item => b.line_break(),
                TagEnd::Emphasis => b.italic = b.italic.saturating_sub(1),
                TagEnd::Strong => b.bold = b.bold.saturating_sub(1),
                TagEnd::Link => b.link = None,
                TagEnd::TableHead => {
                    b.in_table_head = false;
                    b.text("\n");
                }
                TagEnd::TableRow => b.text("\n"),
                _ => {}
            },
            Event::Text(text) => {
                if b.in_code_block {
                    code_body.push_str(&text);
                } else {
                    b.text(&text);
                }
            }
            Event::Code(text) => {
                let style = Style { code: true, ..b.style() };
                b.push(&text, style);
            }
            Event::SoftBreak => b.text(" "),
            Event::HardBreak => b.text("\n"),
            Event::Rule => {
                b.block_break();
                b.push("────────────────\n", Style { tone: Some(Tone::Dim), ..Style::default() });
            }
            // Raw HTML (`<!-- ... -->`, `<details>`) is markup, not content.
            _ => {}
        }
    }
    // No trailing blank lines.
    while let Some(last) = b.runs.last_mut() {
        let trimmed = last.text.trim_end_matches('\n').len();
        if trimmed == 0 {
            b.runs.pop();
        } else {
            last.text.truncate(trimmed);
            break;
        }
    }
    b.runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(runs: &[Run]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn headings_are_bold_and_sized_and_blocks_are_separated() {
        let runs = parse("# Title\n\nSome **bold** and `code` text.\n");
        assert_eq!(plain(&runs), "Title\n\nSome bold and code text.");
        assert_eq!(runs[0].style.heading, 1);
        assert!(runs[0].style.bold);
        assert!(runs.iter().any(|r| r.text == "bold" && r.style.bold));
        assert!(runs.iter().any(|r| r.text == "code" && r.style.code));
    }

    #[test]
    fn lists_get_bullets_and_numbers() {
        assert_eq!(plain(&parse("- one\n- two\n")), "• one\n• two");
        assert_eq!(plain(&parse("1. a\n2. b\n")), "1. a\n2. b");
        assert_eq!(plain(&parse("- a\n  - nested\n")), "• a\n    • nested");
    }

    #[test]
    fn diff_blocks_colour_added_removed_and_hunk_lines() {
        let runs = parse("```diff\n@@ -1 +1 @@\n-old\n+new\n ctx\n```\n");
        let tones: Vec<_> = runs.iter().map(|r| (r.text.trim_end().to_owned(), r.style.tone)).collect();
        assert_eq!(
            tones,
            [
                ("@@ -1 +1 @@".to_owned(), Some(Tone::Dim)),
                ("-old".to_owned(), Some(Tone::Red)),
                ("+new".to_owned(), Some(Tone::Green)),
                (" ctx".to_owned(), None),
            ]
        );
        assert!(runs.iter().all(|r| r.style.code));
    }

    #[test]
    fn links_keep_their_target_and_html_is_dropped() {
        let runs = parse("<!-- hidden -->\nSee [the PR](https://x/1).\n");
        assert_eq!(plain(&runs), "See the PR.");
        assert!(runs.iter().any(|r| r.text == "the PR" && r.style.link.as_deref() == Some("https://x/1")));
    }

    #[test]
    fn tables_render_as_rows_with_a_bold_header() {
        let runs = parse("| A | B |\n| --- | --- |\n| 1 | 2 |\n");
        assert_eq!(plain(&runs), "A  │  B\n1  │  2");
        assert!(runs[0].style.bold);
    }

    #[test]
    fn quotes_are_dim_and_empty_input_is_empty() {
        let runs = parse("> quoted\n");
        assert_eq!(plain(&runs), "quoted");
        assert_eq!(runs[0].style.tone, Some(Tone::Dim));
        assert!(parse("").is_empty());
    }
}
