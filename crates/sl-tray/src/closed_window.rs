//! The Closed Sessions window: every remembered session no longer running,
//! newest first, filtered as you type, each with what can still be done with
//! it.
//!
//! ```text
//! [ Search closed sessions                         ]
//! 44 closed sessions
//! DN-512 / M2 drafts                      [Zed] [Copy Resume] [Dismiss]
//! datanaut · jakub/dn-512-m2-drafts · #744 · closed 50m
//! ───────────────────────────────────────────────────
//! ...
//! ```
//!
//! The menu and the popover list only the newest few; this is where the rest
//! are. The list is rebuilt only when what it shows changes.

use std::cell::RefCell;

use crate::act::{Act, Target};
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSBezelStyle, NSButton, NSControlSize, NSFont, NSLineBreakMode, NSScrollView,
    NSSearchField, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::meters::FlippedView;
use crate::sessions::{Seg, Tone, seg};
use crate::sticky::{Remembered, closed_ago};

const WIDTH: f64 = 620.0;
const HEIGHT: f64 = 520.0;
const MARGIN: f64 = 16.0;
const ROW_HEIGHT: f64 = 52.0;
/// Where the list starts, under the search field and the count.
const LIST_TOP: f64 = 70.0;

pub(crate) struct ClosedWindow {
    window: Retained<NSWindow>,
    search: Retained<NSSearchField>,
    count: Retained<NSTextField>,
    list: FlippedView,
    /// What the list was last built from.
    key: RefCell<Option<ListKey>>,
}

/// The query, the rows it leaves, and how long ago each closed (which changes
/// with the minute): an equal one builds the same list.
type ListKey = (String, Vec<Remembered>, Vec<String>);

impl ClosedWindow {
    /// The window, its search field and buttons bound to their acts.
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let content = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
        );

        let search = NSSearchField::initWithFrame(
            NSSearchField::alloc(mtm),
            NSRect::new(
                NSPoint::new(MARGIN, 14.0),
                NSSize::new(WIDTH - 2.0 * MARGIN, 24.0),
            ),
        );
        search.setPlaceholderString(Some(&NSString::from_str(
            "Search by name, project, branch, PR or id",
        )));
        // Each keystroke filters at once.
        search.setSendsSearchStringImmediately(true);
        crate::act::bind(&search, Act::SearchClosed);
        content.addSubview(&search);

        let count = NSTextField::labelWithString(&NSString::new(), mtm);
        count.setTextColor(Some(&crate::controller::dim_color()));
        count.setFrame(NSRect::new(
            NSPoint::new(MARGIN, 46.0),
            NSSize::new(WIDTH - 2.0 * MARGIN, 18.0),
        ));
        content.addSubview(&count);

        let list = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 100.0)),
        );
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, LIST_TOP),
                NSSize::new(WIDTH, HEIGHT - LIST_TOP),
            ),
        );
        scroll.setDrawsBackground(false);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDocumentView(Some(&list));
        content.addSubview(&scroll);

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
        let window = sl_macos::window::new_window(
            mtm,
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable,
        );
        window.setTitle(&NSString::from_str("Closed Sessions"));
        window.setContentView(Some(&content));
        window.center();
        Self {
            window,
            search,
            count,
            list,
            key: RefCell::new(None),
        }
    }

    /// Bring the window forward, its search field ready to type in.
    pub(crate) fn show(&self) {
        NSApplication::sharedApplication(self.window.mtm()).activate();
        self.window.makeKeyAndOrderFront(None);
        self.window.makeFirstResponder(Some(&self.search));
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// Show `closed`, filtered by what is in the search field.
    pub(crate) fn update(&self, closed: &[Remembered], now: i64) {
        let query = self.search.stringValue().to_string();
        let shown: Vec<Remembered> = closed
            .iter()
            .filter(|entry| matches(entry, &query))
            .cloned()
            .collect();
        // How long ago each closed changes with the minute.
        let ages: Vec<String> = shown
            .iter()
            .map(|e| closed_ago(e.last_seen_ms, now))
            .collect();
        let key = (query.clone(), shown.clone(), ages.clone());
        if self.key.borrow().as_ref() == Some(&key) {
            return;
        }
        let mtm = self.window.mtm();
        self.count.setStringValue(&NSString::from_str(&count_line(
            shown.len(),
            closed.len(),
            &query,
        )));
        for view in self.list.subviews().to_vec() {
            view.removeFromSuperview();
        }
        for (index, (entry, age)) in shown.iter().zip(&ages).enumerate() {
            let row = row_view(mtm, entry, age);
            row.setFrameOrigin(NSPoint::new(0.0, index as f64 * ROW_HEIGHT));
            self.list.addSubview(&row);
        }
        let height = (shown.len() as f64 * ROW_HEIGHT).max(HEIGHT - LIST_TOP);
        self.list.setFrameSize(NSSize::new(WIDTH, height));
        *self.key.borrow_mut() = Some(key);
    }
}

/// `44 closed sessions`, or `3 of 44 match "dn-5"`.
fn count_line(shown: usize, total: usize, query: &str) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if query.trim().is_empty() {
        format!("{total} closed session{}", plural(total))
    } else {
        format!("{shown} of {total} match \u{201c}{}\u{201d}", query.trim())
    }
}

/// Whether `entry` matches `query`: every word in it, in any of the name,
/// project, branch, `#PR`, session id or `claude` / `codex`, ignoring case.
fn matches(entry: &Remembered, query: &str) -> bool {
    let haystack = format!(
        "{} {} {} {} {} {}",
        entry.name,
        entry.project,
        entry.branch.as_deref().unwrap_or(""),
        entry
            .pr_number
            .map(|pr| format!("#{pr} {pr}"))
            .unwrap_or_default(),
        entry.session_id,
        match entry.provider {
            crate::view_prefs::Source::Claude => "claude",
            crate::view_prefs::Source::Codex => "codex",
        }
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// One closed session: name, where it was and when, and its three buttons.
fn row_view(mtm: MainThreadMarker, entry: &Remembered, age: &str) -> Retained<NSView> {
    let row: Retained<NSView> = FlippedView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, ROW_HEIGHT)),
    )
    .retained_view();

    // The buttons, right to left.
    let mut right = WIDTH - MARGIN;
    for (title, act) in [
        ("Dismiss", Act::ClosedWindowDismiss),
        ("Copy Resume", Act::ClosedWindowCopyId),
        ("Zed", Act::ClosedWindowZed),
    ] {
        let button = small_button(mtm, title, act, Target::Closed(entry.session_id.clone()));
        let size = button.frame().size;
        right -= size.width;
        button.setFrameOrigin(NSPoint::new(
            right,
            ((ROW_HEIGHT - size.height) / 2.0).round(),
        ));
        row.addSubview(&button);
        right -= 6.0;
    }

    let mut detail = vec![entry.project.clone()];
    detail.extend(entry.branch.clone());
    detail.extend(entry.pr_number.map(|pr| format!("#{pr}")));
    detail.push(age.to_owned());
    let lines: [(Vec<Seg>, f64, f64); 2] = [
        (vec![seg(&entry.name, Tone::Plain)], 13.0, 8.0),
        (vec![seg(detail.join(" · "), Tone::Dim)], 11.5, 28.0),
    ];
    for (segs, size, y) in lines {
        let label = NSTextField::labelWithString(&NSString::new(), mtm);
        label.setAttributedStringValue(&crate::controller::small_title(&segs, size));
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        label.sizeToFit();
        label.setFrame(NSRect::new(
            NSPoint::new(MARGIN, y),
            NSSize::new((right - 8.0 - MARGIN).max(80.0), label.frame().size.height),
        ));
        row.addSubview(&label);
    }

    let line = crate::compact::TintView::new(mtm, Tone::Plain);
    line.ivars().strength.set(0.25);
    line.ivars().radius.set(0.0);
    line.setFrame(NSRect::new(
        NSPoint::new(MARGIN, ROW_HEIGHT - 1.0),
        NSSize::new(WIDTH - 2.0 * MARGIN, 1.0),
    ));
    row.addSubview(&line);
    row
}

/// A small push button, asking for `act` about `target`.
fn small_button(
    mtm: MainThreadMarker,
    title: &str,
    act: Act,
    target: Target,
) -> Retained<NSButton> {
    let button = sl_macos::controls::button(mtm, title);
    crate::act::bind_to(&button, act, target);
    button.setBezelStyle(NSBezelStyle::Push);
    button.setControlSize(NSControlSize::Small);
    button.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    button.sizeToFit();
    button
}

/// For `--icon-preview`: the window with `closed`, dark, as a PNG at `path`.
pub(crate) fn preview(path: &str, closed: &[Remembered], now: i64) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let window = ClosedWindow::new(mtm);
    window.update(closed, now);
    let Some(content) = window.window.contentView() else {
        return false;
    };
    let dark = sl_macos::consts::appearance(sl_macos::consts::Appearance::Dark);
    objc2_app_kit::NSAppearanceCustomization::setAppearance(&*window.window, dark.as_deref());
    let backdrop = crate::menu_rows::Backdrop::new(mtm, content.frame());
    let backdrop_view: &NSView = &backdrop;
    objc2_app_kit::NSAppearanceCustomization::setAppearance(backdrop_view, dark.as_deref());
    backdrop.addSubview(&content);
    crate::controller::write_png(&backdrop, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, branch: Option<&str>, pr: Option<i64>) -> Remembered {
        Remembered {
            session_id: format!("id-{name}"),
            name: name.into(),
            project: "datanaut".into(),
            cwd: "/w".into(),
            branch: branch.map(Into::into),
            pr_number: pr,
            base_pr: None,
            last_seen_ms: 0,
            provider: crate::view_prefs::Source::Claude,
        }
    }

    #[test]
    fn every_word_must_match_somewhere_ignoring_case() {
        let e = entry(
            "DN-512 / M2 drafts",
            Some("jakub/dn-512-m2-drafts"),
            Some(744),
        );
        for query in [
            "",
            "drafts",
            "DATANAUT",
            "dn-512 m2",
            "#744",
            "744",
            "id-DN",
        ] {
            assert!(matches(&e, query), "{query:?}");
        }
        for query in ["statusline", "drafts zed", "#745"] {
            assert!(!matches(&e, query), "{query:?}");
        }
    }

    #[test]
    fn the_count_says_how_many_and_what_matched() {
        assert_eq!(count_line(44, 44, ""), "44 closed sessions");
        assert_eq!(count_line(1, 1, "  "), "1 closed session");
        assert_eq!(
            count_line(3, 44, " dn-5 "),
            "3 of 44 match \u{201c}dn-5\u{201d}"
        );
    }
}
