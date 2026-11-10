//! `BarController`: owns the menu bar item, its menu and popover, the
//! windows, the repaint timer and the shared snapshot, and answers every
//! click (an [`Act`], see [`crate::act`]).
//!
//! It is a plain Rust struct in an `Rc`: every AppKit callback reaches it
//! through `sl_macos`'s closure-backed targets and delegates, each holding it
//! weakly. No class is defined here, and the crate is `forbid(unsafe_code)`.
//!
//! Two AppKit lifetime rules shape this file, and both are load-bearing:
//!
//! 1. **The repaint timer must be registered in the run loop's *common*
//!    modes.** `+[NSTimer scheduledTimerWithTimeInterval:...]` installs the
//!    timer into `NSDefaultRunLoopMode` only. While a menu is open the run
//!    loop runs in `NSEventTrackingRunLoopMode`, so a scheduled timer stops
//!    firing the instant the user opens the menu -- exactly when the live
//!    "updated Ns ago" row needs to keep counting. [`BarController::install`]
//!    therefore uses `sl_macos::timer::RepeatingTimer`, which builds the
//!    timer unscheduled and adds it to `NSRunLoopCommonModes` by hand.
//!
//! 2. **`setDelegate:`/`setTarget:` do not retain.** `NSMenu`'s delegate and
//!    a control's target are unretained properties, so a target or delegate
//!    nothing else holds is freed at once and silently never called.
//!    `sl_macos::action` and `sl_macos::delegate` keep each one alive on the
//!    object it serves (an associated object), and those closures hold the
//!    controller weakly; `main` holds the `Rc<BarController>` for the whole
//!    run, and the `Retained<NSStatusItem>` (a dropped status item vanishes
//!    from the menu bar) lives in its ivars rather than in a temporary.
//!
//! And one structural rule, which is what the whole [`Row`] machinery below
//! exists to enforce:
//!
//! 3. **A repaint assigns titles. It never builds menu structure.** Adding or
//!    removing an `NSMenuItem` in a menu that is currently open closes it, so
//!    every row this menu will ever show is created once, in
//!    [`BarController::new`], and a tick only ever assigns a title or flips
//!    `hidden`. That is why the open-sessions block is a fixed set of slots
//!    with spares kept hidden, rather than a list rebuilt from the sessions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAppearance, NSAppearanceCustomization, NSAutoresizingMaskOptions, NSBezelStyle, NSBezierPath, NSButton, NSControlSize, NSLineCapStyle, NSLineJoinStyle, NSCompositingOperation, NSTextField, NSView, NSAlertSecondButtonReturn, NSApplication, NSCellImagePosition, NSScrollView, NSTextView, NSColor, NSFont, NSFontWeight, NSImage, NSImageSymbolConfiguration, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSMutableAttributedString, NSPoint, NSRect, NSSize, NSURL,
    NSString,
};

use crate::act::{Act, Slot as TargetSlot, Target};
use crate::awake::AutoCaffeinate;
use crate::compact::{CompactInput, CompactPopover};
use crate::closed_window::ClosedWindow;
use crate::settings_window::SettingsWindow;
use crate::sticky::{Remembered, Sticky};
use crate::stacks::{EntrySession, EntryState, StackEntry, StackView};
use crate::menu_rows::{self, ActionContent, ActionView, HeaderContent, HeaderView, RowContent, RowView};
use crate::meters::{DETAIL_FONT_SIZE, LimitsPanel, MeterSlot, PANEL_GAP, PANEL_INSET, PANEL_MIN_WIDTH, PANEL_RIGHT};
use crate::view_prefs::{self, BarWindow, ColdMode, Layout, MeterStyle, Provider, Source, SortOrder, Tuning};
use crate::predictions;
use crate::refresh;
use crate::actions::{self, Outbox, Outcome};
use crate::markdown::{self, Run};
use crate::stats::StatsWindow;
use crate::sessions::{self, BarState, Icon, MAX_JOBS, MAX_REVIEWERS, MAX_SESSIONS, OpenSession, Seg, Tone, Urgency};
use crate::snapshot::{LimitWindow, Snapshot, now_ms};
use crate::tray_model::{BarMeter, header_content, METER_PAGES, bar_meter, window_model, closed_fold, closed_header, closed_row, footer_text, limit_models, meter_dots, stack_mark, stack_row};

/// How often the main thread repaints while the menu, the popover or a window
/// is up. Deliberately much faster than the background refresh: the repaint
/// is what makes the "updated Ns ago" row tick over while the menu is open,
/// which is the one visible symptom of gotcha #1 being handled correctly.
const TICK_INTERVAL: f64 = 1.0;

/// Let the OS coalesce the timer with other wakeups. A 10% tolerance on a 1s
/// repaint is invisible and measurably cheaper on battery.
const TICK_TOLERANCE: f64 = 0.1;

/// How often it looks at the menu bar icon while nothing else is up. The icon
/// follows the usage refresh and the sessions pass, which are seconds apart
/// themselves, and every wakeup keeps the Mac from idling a little longer.
/// Opening anything repaints at once and ticks fast again.
const IDLE_TICK_INTERVAL: f64 = 15.0;
const IDLE_TICK_TOLERANCE: f64 = 2.0;

/// Ticks so far, for the self-test to see the timer firing while a menu is
/// being tracked (gotcha #1).
pub(crate) static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// --------------------------------------------------------------------------
// Rows
// --------------------------------------------------------------------------

/// One live menu row: the item, plus the last content pushed into it and its
/// last hidden state.
///
/// The cache is not premature optimisation, it is the reason the repaint can
/// safely run once a second on ~70 rows: an unchanged row does no Objective-C
/// work at all, so only the handful of rows that actually moved touch AppKit
/// on a given tick.
struct Row {
    item: Retained<NSMenuItem>,
    last: RefCell<Option<String>>,
    hidden: Cell<bool>,
}

impl Row {
    /// A separator, wrapped as a row so it can be hidden along with the
    /// section it belongs to -- a separator left behind by an empty section
    /// reads as a rendering bug.
    fn separator(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        Self::append(menu, NSMenuItem::separatorItem(mtm))
    }

    fn append(menu: &NSMenu, item: Retained<NSMenuItem>) -> Self {
        menu.addItem(&item);
        Self { item, last: RefCell::new(None), hidden: Cell::new(false) }
    }

    fn show(&self) {
        if self.hidden.replace(false) {
            self.item.setHidden(false);
        }
    }

    /// Hide the row. Hiding, not removing: see rule #3 in the module docs.
    fn hide(&self) {
        if !self.hidden.replace(true) {
            self.item.setHidden(true);
        }
    }
}

/// A line of text that is not selectable, indented one level under the
/// meters: a plain item lights up under the pointer, and a disabled one dims
/// its title whatever colour it carries, so this is a label in a view on an
/// item that has no action.
struct NoteRow {
    row: Row,
    view: Retained<NSView>,
    text: Retained<NSTextField>,
}

impl NoteRow {
    /// Where its text starts: a standard row's text inset, one indent in.
    const INSET: f64 = 17.0 + 12.0;
    /// The menu's own inset on the right, as on the left.
    const RIGHT: f64 = 17.0;
    const VERTICAL_PAD: f64 = 2.0;

    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let text = NSTextField::labelWithString(&NSString::new(), mtm);
        text.setMaximumNumberOfLines(0);
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 22.0)));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        view.addSubview(&text);
        let item = NSMenuItem::new(mtm);
        item.setView(Some(&view));
        Self { row: Row::append(menu, item), view, text }
    }

    /// Show the row with `title`, in the default colour.
    fn set(&self, title: &str) {
        self.row.show();
        if self.row.last.borrow().as_deref() == Some(title) {
            return;
        }
        self.text.setStringValue(&NSString::from_str(title));
        self.text.setTextColor(Some(&NSColor::labelColor()));
        // Fit the view to the text, which the menu will not do for a view.
        self.text.sizeToFit();
        let size = self.text.frame().size;
        self.text.setFrameOrigin(NSPoint::new(Self::INSET, Self::VERTICAL_PAD));
        self.view.setFrameSize(NSSize::new(Self::INSET + size.width + Self::RIGHT, size.height + 2.0 * Self::VERTICAL_PAD));
        *self.row.last.borrow_mut() = Some(title.to_owned());
    }

    fn hide(&self) {
        self.row.hide();
    }
}

// --------------------------------------------------------------------------
// Section row groups
// --------------------------------------------------------------------------

/// The Usage Limits rows: a meter for each captured window and the age of the
/// capture they came from.
///
/// Top-level rather than a submenu: these are the numbers the tray exists to
/// show, and they should be readable the moment the menu opens.
struct LimitRows {
    /// The menu item holding `panel`'s view.
    row: Row,
    panel: LimitsPanel,
    /// The plan's "Data Updated At". Deliberately its own row and not folded
    /// into the "Updated Ns ago" row at the bottom of the menu: that one is
    /// the *snapshot's* clock, this is the capture's, and they run at very
    /// different speeds.
    updated: NoteRow,
}

/// A failed job's row and the submenu behind it: what it is, and what can
/// be done about it.
struct JobMenu {
    /// The job its actions are about, set as it is painted.
    target: TargetSlot,
    row: Row,
    view: ActionView,
    header: HeaderView,
    retry: Row,
    logs: Row,
    link: Row,
}

/// A menu item drawn by `view`, appended to `menu`.
fn view_row(mtm: MainThreadMarker, menu: &NSMenu, view: &NSView) -> Row {
    let item = section_row(mtm, "");
    item.setView(Some(view));
    Row::append(menu, item)
}

/// A clickable action row: a click asks for `act` about what `target` holds.
fn action_row(mtm: MainThreadMarker, menu: &NSMenu, act: Act, target: &TargetSlot, content: ActionContent) -> (Row, ActionView) {
    let view = ActionView::new(mtm);
    crate::act::bind_slot(&view, act, target);
    view.set(content);
    (view_row(mtm, menu, &view), view)
}

fn action(symbol: &'static str, title: &str) -> ActionContent {
    ActionContent { symbol: Some(symbol), title: vec![sessions::seg(title, Tone::Plain)], enabled: true, ..ActionContent::default() }
}

/// One open session: its top-level row and the submenu behind it, laid out
/// as a card -- the PR, its reviewers, the actions, the failed CI jobs.
struct SessionMenu {
    /// The session its actions are about, set as it is painted.
    target: TargetSlot,
    row: Row,
    /// Draws `row`: icon, title, detail line, cache column, chevron.
    view: RowView,
    header: HeaderView,
    reviewers: Vec<(Row, ActionView)>,
    review_separator: Row,
    copy_review: Row,
    open_pr: Row,
    open_linear: Row,
    copy_rc: Row,
    zed_review: Row,
    jobs_separator: Row,
    jobs: Vec<JobMenu>,
}

impl SessionMenu {
    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let target = TargetSlot::default();
        let view = RowView::new(mtm);
        let row = view_row(mtm, menu, &view);
        row.hide();
        let sub = attach_submenu(mtm, &row);
        sub.setMinimumWidth(menu_rows::SUBMENU_WIDTH);
        let header = HeaderView::new(mtm);
        view_row(mtm, &sub, &header);
        // Who reviewed and how: information, not actions. A disabled item is
        // never highlighted, and a view with no action sends nothing.
        let reviewers: Vec<(Row, ActionView)> = (0..MAX_REVIEWERS)
            .map(|_| {
                let view = ActionView::new(mtm);
                let row = view_row(mtm, &sub, &view);
                row.item.setEnabled(false);
                row.hide();
                (row, view)
            })
            .collect();
        let jobs_separator = Row::separator(mtm, &sub);
        let jobs: Vec<JobMenu> = (0..MAX_JOBS)
            .map(|_| {
                let job_target = TargetSlot::default();
                let view = ActionView::new(mtm);
                let row = view_row(mtm, &sub, &view);
                row.hide();
                let job_menu = attach_submenu(mtm, &row);
                job_menu.setMinimumWidth(240.0);
                let header = HeaderView::new(mtm);
                view_row(mtm, &job_menu, &header);
                Row::separator(mtm, &job_menu);
                let add = |symbol, title, selector| {
                    let (row, _view) = action_row(mtm, &job_menu, selector, &job_target, action(symbol, title));
                    row
                };
                let retry = add("arrow.clockwise", "Re-run this job…", Act::RetryJob);
                let logs = add("doc.text", "Show failed-step logs…", Act::ShowLogs);
                let link = add("link", "Copy job link", Act::CopyJobLink);
                JobMenu { target: job_target, row, view, header, retry, logs, link }
            })
            .collect();
        let review_separator = Row::separator(mtm, &sub);
        let add = |symbol, title, selector| {
            let (row, _view) = action_row(mtm, &sub, selector, &target, action(symbol, title));
            row
        };
        let copy_review = add("doc.on.doc", "Copy review feedback", Act::CopyReview);
        let open_pr = add("arrow.up.right.square", "Open PR in browser", Act::OpenPr);
        let open_linear = add("ticket", "Open in Linear", Act::OpenLinear);
        // Shown for every session, so not kept: nothing ever hides them.
        add("folder", "Copy full path", Act::CopyCwd);
        let copy_rc = add("dot.radiowaves.left.and.right", "Copy remote-control session ID", Act::CopyRcId);
        // Zed, a section of its own: the worktree, and the review as a file.
        Row::separator(mtm, &sub);
        add(crate::zed_logo::SYMBOL, "Open worktree in Zed", Act::OpenZed);
        let zed_review = add(crate::zed_logo::SYMBOL, "Open review feedback in Zed", Act::OpenReviewInZed);
        Self { target, row, view, header, reviewers, review_separator, copy_review, open_pr, open_linear, copy_rc, zed_review, jobs_separator, jobs }
    }

    /// `holds_awake`: the tray is keeping the Mac awake for this session.
    /// `mark`: show whose session it is (Claude Code's and Codex's are listed
    /// together).
    fn paint(&self, session: &OpenSession, holds_awake: bool, mark: bool, now: i64, tuning: &Tuning) {
        *self.target.borrow_mut() = Target::Session(session.session_id.clone());
        self.row.show();
        self.view.set(RowContent {
            icon: Some(sessions::state_glyph(session, now, holds_awake)),
            ring: sessions::context_ring(session, tuning),
            title: sessions::row_title(session),
            detail: sessions::row_detail(session, now),
            right: sessions::row_right(session, now, tuning),
            mark: mark.then_some(session.provider),
            chevron: Some(false),
            tinted: false,
            dimmed: false,
        });
        self.header.set(header_content(session));
        let info = session.gh.as_ref();
        let has_pr = info.is_some_and(|i| i.pr_number.is_some());

        // Reviewers, each a row that opens what they wrote; the "no reviews
        // yet" placeholder is said in the header instead.
        let entries: Vec<(Option<String>, Vec<Seg>)> =
            info.filter(|_| has_pr).map(sessions::reviewer_lines).unwrap_or_default().into_iter().filter(|(name, _)| name.is_some()).collect();
        for ((row, view), (_, segs)) in self.reviewers.iter().zip(&entries) {
            row.show();
            view.set(ActionContent { title: segs.clone(), enabled: true, ..ActionContent::default() });
        }
        for (row, _) in &self.reviewers[entries.len().min(self.reviewers.len())..] {
            row.hide();
        }
        for row in [&self.review_separator, &self.copy_review, &self.open_pr, &self.zed_review] {
            if has_pr { row.show() } else { row.hide() }
        }
        if session.linear.is_some() { self.open_linear.show() } else { self.open_linear.hide() }
        if session.remote_control.is_some() { self.copy_rc.show() } else { self.copy_rc.hide() }

        let failed: &[sl_core::gh::FailedJob] = info.and_then(|i| i.ci.as_ref()).map_or(&[], |ci| &ci.failed);
        if failed.is_empty() { self.jobs_separator.hide() } else { self.jobs_separator.show() }
        for (job_menu, job) in self.jobs.iter().zip(failed) {
            *job_menu.target.borrow_mut() = Target::Job { session: session.session_id.clone(), job: job.name.clone() };
            job_menu.row.show();
            let id = job.link.as_deref().and_then(sl_core::gh::run_and_job_ids).map(|(_, id)| id);
            job_menu.view.set(ActionContent {
                symbol: Some("xmark"),
                symbol_tone: Some(Tone::Red),
                title: vec![sessions::seg(&job.name, Tone::Plain)],
                chevron: true,
                enabled: true,
            });
            job_menu.header.set(HeaderContent {
                title: job.name.clone(),
                lines: id.map_or_else(|| vec![vec![sessions::seg("failed", Tone::Dim)]], |id| vec![vec![sessions::seg(format!("failed · job {id}"), Tone::Dim)]]),
                ..HeaderContent::default()
            });
            for row in [&job_menu.retry, &job_menu.logs] {
                if id.is_some() { row.show() } else { row.hide() }
            }
            if job.link.is_some() { job_menu.link.show() } else { job_menu.link.hide() }
        }
        for job_menu in &self.jobs[failed.len().min(self.jobs.len())..] {
            job_menu.row.hide();
        }
    }
}

/// The Open Sessions block: one [`SessionMenu`] per session.
struct OpenRows {
    slots: Vec<Slot>,
    /// `More sessions · 3`, when the open sessions outnumber the rows
    /// Settings allows.
    more: CaptionRow,
    /// The closed sessions, under the open ones.
    closed: ClosedRows,
    /// The PR stacks the sessions are on, under the closed ones.
    stacks: StackRows,
    separator: Row,
}

/// Rows reserved for stacks, and for the PRs in each.
const MAX_STACKS: usize = 4;
const MAX_STACK_ENTRIES: usize = 16;

/// The Stacks group: its caption, and a row per stack.
struct StackRows {
    caption: CaptionRow,
    slots: Vec<StackMenu>,
}

/// One stack: a row, and behind it its PRs, top first as GitHub lists them.
struct StackMenu {
    /// The stack "Open stack on GitHub" is about, set as it is painted.
    target: TargetSlot,
    row: Row,
    view: RowView,
    header: HeaderView,
    /// A row per PR, and the PR its click opens.
    entries: Vec<(Row, ActionView, TargetSlot)>,
    open_stack: Row,
}

impl StackRows {
    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let caption = CaptionRow::new(mtm, menu);
        caption.row.hide();
        Self { caption, slots: (0..MAX_STACKS).map(|_| StackMenu::new(mtm, menu)).collect() }
    }
}

impl StackMenu {
    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let target = TargetSlot::default();
        let view = RowView::new(mtm);
        let row = view_row(mtm, menu, &view);
        row.hide();
        let sub = attach_submenu(mtm, &row);
        sub.setMinimumWidth(menu_rows::SUBMENU_WIDTH);
        let header = HeaderView::new(mtm);
        view_row(mtm, &sub, &header);
        Row::separator(mtm, &sub);
        let entries: Vec<(Row, ActionView, TargetSlot)> = (0..MAX_STACK_ENTRIES)
            .map(|_| {
                let entry = TargetSlot::default();
                let (row, view) = action_row(mtm, &sub, Act::OpenStackPr, &entry, ActionContent::default());
                row.hide();
                (row, view, entry)
            })
            .collect();
        Row::separator(mtm, &sub);
        let (open_stack, _open_view) = action_row(mtm, &sub, Act::OpenStack, &target, action("arrow.up.right.square", "Open stack on GitHub"));
        Self { target, row, view, header, entries, open_stack }
    }

    fn paint(&self, stack: &StackView) {
        let key = stack.key();
        *self.target.borrow_mut() = Target::Stack(key.clone());
        self.row.show();
        self.view.set(stack_row(stack));
        let live = stack.entries.iter().filter(|e| matches!(e.session, EntrySession::Live(_))).count();
        let closed = stack.entries.iter().filter(|e| matches!(e.session, EntrySession::Closed(_))).count();
        let mut line = vec![format!("{} of {} merged", stack.merged(), stack.entries.len())];
        if live > 0 {
            line.push(format!("{live} open session{}", if live == 1 { "" } else { "s" }));
        }
        if closed > 0 {
            line.push(format!("{closed} closed"));
        }
        self.header.set(HeaderContent {
            symbol: Some(("square.stack.3d.up", Tone::Dim)),
            title: stack.number.map_or_else(|| "PR chain".to_owned(), |n| format!("Stack #{n}")),
            lines: vec![vec![sessions::seg(line.join(" · "), Tone::Dim)]],
            ..HeaderContent::default()
        });
        // Top first, as GitHub's own stack panel lists them.
        let shown: Vec<&StackEntry> = stack.entries.iter().rev().take(self.entries.len()).collect();
        for ((row, view, target), entry) in self.entries.iter().zip(&shown) {
            row.show();
            *target.borrow_mut() = Target::StackPr { stack: key.clone(), pr: entry.number };
            view.set(stack_entry_content(entry));
        }
        for (row, _, _) in &self.entries[shown.len()..] {
            row.hide();
        }
        if stack.repo_url.is_some() && stack.next_open().is_some() { self.open_stack.show() } else { self.open_stack.hide() }
    }
}

fn stack_entry_content(entry: &StackEntry) -> ActionContent {
    let (symbol, tone) = stack_mark(entry.state);
    let mut title = vec![sessions::seg(format!("#{}", entry.number), if entry.state == EntryState::Merged { Tone::Dim } else { Tone::Plain })];
    if let Some(branch) = &entry.branch {
        title.push(sessions::seg(format!("  {}", sessions::truncate_middle(branch, 34)), Tone::Dim));
    }
    match &entry.session {
        EntrySession::Live(name) => title.push(sessions::seg(format!("  ● {name}"), Tone::Green)),
        EntrySession::Closed(name) => title.push(sessions::seg(format!("  ◌ {name}"), Tone::Dim)),
        EntrySession::None => {}
    }
    ActionContent { symbol: Some(symbol), symbol_tone: Some(tone), title, enabled: true, ..ActionContent::default() }
}

/// Fill the Stacks group, or hide it when no session is on a stack.
fn paint_stacks(rows: &StackRows, stacks: &[StackView]) {
    let shown = &stacks[..stacks.len().min(rows.slots.len())];
    if shown.is_empty() {
        rows.caption.row.hide();
    } else {
        rows.caption.set("Stacks", shown.len(), false);
    }
    for (slot, stack) in rows.slots.iter().zip(shown) {
        slot.paint(stack);
    }
    for slot in &rows.slots[shown.len()..] {
        slot.row.hide();
    }
}

/// Rows reserved for closed sessions; more are not listed.
const MAX_CLOSED: usize = 8;

/// The Closed group: a row that folds it, and a row per closed session.
struct ClosedRows {
    fold: Row,
    fold_view: RowView,
    slots: Vec<ClosedMenu>,
    /// "All closed sessions…", under the rows: the window with every one.
    all: Row,
}

/// One closed session: a dimmed row, and behind it what can still be done.
struct ClosedMenu {
    /// The closed session its actions are about, set as it is painted.
    target: TargetSlot,
    row: Row,
    view: RowView,
    header: HeaderView,
}

impl ClosedRows {
    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let fold_view = RowView::new(mtm);
        crate::act::bind(&fold_view, Act::ToggleClosed);
        let item = NSMenuItem::new(mtm);
        item.setView(Some(&fold_view));
        let fold = Row::append(menu, item);
        fold.hide();
        let slots = (0..MAX_CLOSED).map(|_| ClosedMenu::new(mtm, menu)).collect();
        let (all, _) = action_row(mtm, menu, Act::ShowClosedWindow, &TargetSlot::default(), action("magnifyingglass", "All closed sessions…"));
        all.hide();
        Self { fold, fold_view, slots, all }
    }
}

impl ClosedMenu {
    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let target = TargetSlot::default();
        let view = RowView::new(mtm);
        let row = view_row(mtm, menu, &view);
        row.hide();
        let sub = attach_submenu(mtm, &row);
        sub.setMinimumWidth(menu_rows::SUBMENU_WIDTH);
        let header = HeaderView::new(mtm);
        view_row(mtm, &sub, &header);
        Row::separator(mtm, &sub);
        let add = |symbol, title, selector| {
            let (_, _view) = action_row(mtm, &sub, selector, &target, action(symbol, title));
        };
        add(crate::zed_logo::SYMBOL, "Open worktree in Zed", Act::OpenClosedZed);
        add("doc.on.doc", "Copy resume command", Act::CopyClosedId);
        Row::separator(mtm, &sub);
        add("xmark.circle", "Dismiss", Act::DismissClosed);
        Self { target, row, view, header }
    }

    fn paint(&self, entry: &Remembered, mark: bool, now: i64) {
        *self.target.borrow_mut() = Target::Closed(entry.session_id.clone());
        self.row.show();
        self.view.set(closed_row(entry, mark, now));
        self.header.set(closed_header(entry, now));
    }
}

/// Fill the Closed group, or hide it when nothing has closed. Folded unless
/// `open`.
fn paint_closed(rows: &ClosedRows, entries: &[Remembered], open: bool, marks: bool, now: i64) {
    let shown = &entries[..entries.len().min(rows.slots.len())];
    if entries.is_empty() {
        rows.fold.hide();
    } else {
        rows.fold.show();
        rows.fold_view.set(closed_fold(entries, open));
    }
    for (slot, entry) in rows.slots.iter().zip(shown) {
        if open { slot.paint(entry, marks, now) } else { slot.row.hide() }
    }
    for slot in &rows.slots[shown.len()..] {
        slot.row.hide();
    }
    if open && !entries.is_empty() { rows.all.show() } else { rows.all.hide() }
}

/// A ring of how full a session's context is: `left` filled steps of
/// `RING_STEPS`, clockwise from the top, blue while there is room, orange
/// then red as it fills; `size` points with a stroke `line` wide, `white` for one
/// drawn on a highlight, where its blue would vanish.
pub(crate) fn context_ring_sized(left: u8, tone: Tone, size: f64, line: f64, white: bool) -> Retained<NSImage> {
    let fraction = f64::from(left) / f64::from(sessions::RING_STEPS);
    let draw = block2::RcBlock::new(move |bounds: NSRect| -> objc2::runtime::Bool {
        let line_width = line;
        let center = NSPoint::new(bounds.size.width / 2.0, bounds.size.height / 2.0);
        let radius = bounds.size.width.min(bounds.size.height) / 2.0 - line_width / 2.0 - 0.5;
        let track = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(NSPoint::new(center.x - radius, center.y - radius), NSSize::new(2.0 * radius, 2.0 * radius)));
        track.setLineWidth(line_width);
        let track_color = if white { NSColor::whiteColor() } else { NSColor::labelColor() };
        track_color.colorWithAlphaComponent(if white { 0.3 } else { 0.18 }).setStroke();
        track.stroke();
        let color = match tone {
            _ if white => NSColor::whiteColor(),
            Tone::Plain | Tone::Dim => NSColor::systemBlueColor(),
            other => meter_color(other),
        };
        // Not flipped: 90 degrees is the top, and clockwise on screen runs
        // towards smaller angles.
        let arc = NSBezierPath::bezierPath();
        arc.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(center, radius, 90.0, 90.0 - 360.0 * fraction.min(0.9999), true);
        arc.setLineWidth(line_width);
        arc.setLineCapStyle(NSLineCapStyle::Round);
        color.setStroke();
        arc.stroke();
        objc2::runtime::Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(size, size), false, &draw);
    image.setTemplate(false);
    image
}

/// An SF Symbol at `point_size`, regular weight, in `color`.
pub(crate) fn symbol_image_sized_pub(name: &str, color: &NSColor, point_size: f64) -> Option<Retained<NSImage>> {
    symbol_image_sized(name, color, point_size, sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular))
}

/// As [`symbol_image`], at a chosen point size and weight.
fn symbol_image_sized(name: &str, color: &NSColor, point_size: f64, weight: NSFontWeight) -> Option<Retained<NSImage>> {
    if name == crate::zed_logo::SYMBOL {
        return crate::zed_logo::image(point_size.round(), color, false);
    }
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(point_size, weight);
    let config = size.configurationByApplyingConfiguration(&NSImageSymbolConfiguration::configurationWithHierarchicalColor(color));
    let image = image.imageWithSymbolConfiguration(&config).unwrap_or(image);
    image.setTemplate(false);
    Some(image)
}

/// One session's place in the list: a group's caption or the Cold row,
/// shown only above the first session of its group, and the session itself.
struct Slot {
    /// `NEEDS YOU · 2`, above the first session of a group.
    caption: CaptionRow,
    /// The Cold group's own row, above its first session: it folds them.
    cold: Row,
    cold_view: RowView,
    menu: SessionMenu,
}

/// A group's caption: a small label in a view, which the menu does not
/// highlight or dim.
struct CaptionRow {
    row: Row,
    label: Retained<NSTextField>,
}

impl CaptionRow {
    const HEIGHT: f64 = 22.0;

    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_MIN_WIDTH, Self::HEIGHT)));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let label = NSTextField::labelWithString(&NSString::new(), mtm);
        view.addSubview(&label);
        let item = NSMenuItem::new(mtm);
        item.setView(Some(&view));
        let row = Row::append(menu, item);
        row.hide();
        Self { row, label }
    }

    fn set(&self, label: &str, count: usize, urgent: bool) {
        self.row.show();
        let key = format!("{label}{count}{urgent}");
        if self.row.last.borrow().as_deref() == Some(key.as_str()) {
            return;
        }
        self.label.setAttributedStringValue(&menu_rows::caption_text(label, count, urgent));
        self.label.sizeToFit();
        // Bottom-aligned in a non-flipped view: a little air above, little below.
        self.label.setFrameOrigin(NSPoint::new(PANEL_INSET, 2.0));
        *self.row.last.borrow_mut() = Some(key);
    }
}

// --------------------------------------------------------------------------
// Usage meters (drawn by `meters`) and the footer
// --------------------------------------------------------------------------

const FOOTER_HEIGHT: f64 = 30.0;
/// Between the footer's buttons.
const FOOTER_BUTTON_GAP: f64 = 6.0;

/// A meter's colour: a muted green and a coral red, softer than the system
/// colours, which are tuned for small text rather than for bars this long.
/// The one palette for status colour drawn as a shape -- the meters, the
/// rings round a session's icon and the glyphs inside them -- so a ring and a
/// meter in the same state are the same colour. Softer than the system
/// colours, which text keeps (see [`tone_color`]).
pub(crate) fn meter_color(tone: Tone) -> Retained<NSColor> {
    match tone {
        Tone::Green => NSColor::colorWithSRGBRed_green_blue_alpha(0.37, 0.62, 0.39, 1.0),
        // A warning: ahead of pace, a cache about to go cold.
        Tone::Orange => NSColor::colorWithSRGBRed_green_blue_alpha(0.85, 0.46, 0.36, 1.0),
        // Past it: nearly out of a window, a cache in its last minutes, a failure.
        Tone::Red => NSColor::colorWithSRGBRed_green_blue_alpha(0.84, 0.30, 0.30, 1.0),
        other => tone_color(other),
    }
}

/// A rounded bar, its ends fully round.
pub(crate) fn fill_pill(x: f64, y: f64, width: f64, height: f64, color: &NSColor) {
    if width <= 0.0 {
        return;
    }
    let radius = (height / 2.0).min(width / 2.0);
    color.setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)), radius, radius).fill();
}

/// The last row: when the numbers were last folded and from how many records,
/// with Quit beside it. The age ticks every second, so a stalled repaint timer
/// shows here at a glance rather than passing for "nothing new".
struct FooterRow {
    row: Row,
    view: Retained<NSView>,
    label: Retained<NSTextField>,
    /// Auto caffeinate; its target, the controller, is set in `install`.
    /// Hidden when the tray may not hold the Mac awake at all.
    auto: menu_rows::ToggleButton,
    /// Opens the Settings window; its target, the controller, is set in `install`.
    settings: Retained<NSButton>,
    /// Opens the Stats window; its target, the controller, is set in `install`.
    stats: Retained<NSButton>,
    /// Its target, `NSApp`, is set in `install`.
    quit: Retained<NSButton>,
}

impl FooterRow {
    fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Self {
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_MIN_WIDTH, FOOTER_HEIGHT)));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let label = NSTextField::labelWithString(&NSString::new(), mtm);
        view.addSubview(&label);
        let auto = auto_button(mtm);
        view.addSubview(&auto);
        let settings = settings_button(mtm);
        let stats = stats_button(mtm);
        let quit = quit_button(mtm);
        view.addSubview(&settings);
        view.addSubview(&stats);
        view.addSubview(&quit);
        let item = NSMenuItem::new(mtm);
        item.setView(Some(&view));
        Self { row: Row::append(menu, item), view, label, auto, settings, stats, quit }
    }

    /// Show `text`, and the Auto caffeinate button at `auto` (`None`: hidden).
    fn set(&self, text: &str, auto: Option<bool>) {
        self.row.show();
        let key = format!("{text}\u{1}{auto:?}");
        if self.row.last.borrow().as_deref() == Some(key.as_str()) {
            return;
        }
        self.label.setAttributedStringValue(&styled(text, &dim_color(), DETAIL_FONT_SIZE, sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular)));
        self.label.sizeToFit();
        self.auto.setHidden(auto.is_none());
        if let Some(on) = auto {
            self.auto.set_on(on);
        }
        // Right to left: Quit, Stats, Settings, then Auto caffeinate if shown.
        let auto: &NSView = &self.auto;
        let buttons: Vec<&NSView> = [&*self.quit, &*self.stats, &*self.settings].into_iter().map(|b| -> &NSView { b }).chain([auto]).filter(|b| !b.isHidden()).collect();
        let buttons_width: f64 = buttons.iter().map(|b| b.frame().size.width).sum::<f64>() + FOOTER_BUTTON_GAP * (buttons.len().saturating_sub(1)) as f64;
        let label = self.label.frame().size;
        self.view.setFrameSize(NSSize::new((PANEL_INSET + label.width + PANEL_GAP + buttons_width + PANEL_RIGHT).max(PANEL_MIN_WIDTH), FOOTER_HEIGHT));
        let width = self.view.frame().size.width;
        self.label.setFrameOrigin(NSPoint::new(PANEL_INSET - crate::meters::LABEL_PAD, ((FOOTER_HEIGHT - label.height) / 2.0).round()));
        let mut right = width - PANEL_RIGHT;
        for button in buttons {
            let size = button.frame().size;
            right -= size.width;
            button.setFrameOrigin(NSPoint::new(right, ((FOOTER_HEIGHT - size.height) / 2.0).round()));
            right -= FOOTER_BUTTON_GAP;
        }
        *self.row.last.borrow_mut() = Some(key);
    }
}

/// The footer's cup: Auto caffeinate, filled with the accent colour while on.
/// The same control in both layouts' footers, as tall as the buttons beside it.
pub(crate) fn auto_button(mtm: MainThreadMarker) -> menu_rows::ToggleButton {
    let height = settings_button(mtm).frame().size.height;
    let button = menu_rows::ToggleButton::new(mtm, "cup.and.saucer", "Auto caffeinate: keep the Mac awake while a session is working", NSSize::new(34.0, height));
    crate::act::bind(&button, Act::ToggleAutoAwake);
    // Pinned right, as the buttons beside it: the menu widens the footer
    // after it is laid out, and an unpinned cup would be left behind.
    button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
    button
}

/// The footer's gear: opens the Settings window.
pub(crate) fn settings_button(mtm: MainThreadMarker) -> Retained<NSButton> {
    icon_button(mtm, "gearshape", "Settings", Act::ShowSettings)
}

/// The footer's power symbol: quits.
pub(crate) fn quit_button(mtm: MainThreadMarker) -> Retained<NSButton> {
    icon_button(mtm, "power", "Quit", Act::Quit)
}

/// The footer's chart: opens the Stats window.
pub(crate) fn stats_button(mtm: MainThreadMarker) -> Retained<NSButton> {
    icon_button(mtm, "chart.bar", "Stats", Act::ShowStats)
}

/// A footer button showing the SF Symbol `symbol`, or `title` where this
/// macOS lacks it; `title` is its tooltip either way.
fn icon_button(mtm: MainThreadMarker, symbol: &str, title: &str, act: Act) -> Retained<NSButton> {
    let button = footer_button(mtm, title, act);
    if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), Some(&NSString::from_str(title))) {
        button.setImage(Some(&image));
        button.setImagePosition(NSCellImagePosition::ImageOnly);
        button.sizeToFit();
    }
    button.setToolTip(Some(&NSString::from_str(title)));
    button
}

/// A small push button for the footer, pinned to the right edge.
fn footer_button(mtm: MainThreadMarker, title: &str, act: Act) -> Retained<NSButton> {
    let button = sl_macos::controls::button(mtm, title);
    crate::act::bind(&button, act);
    button.setBezelStyle(NSBezelStyle::Push);
    button.setControlSize(NSControlSize::Small);
    button.setFont(Some(&NSFont::systemFontOfSize(NSFont::smallSystemFontSize())));
    button.sizeToFit();
    button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
    button
}

/// Every live row in the menu.
struct Rows {
    limits: LimitRows,
    open: OpenRows,
    footer: FooterRow,
}

pub struct Ivars {
    /// Written by the background refresh thread, read here. The only thing
    /// shared across the thread boundary, and plain owned data on both sides.
    shared: refresh::Shared,
    /// Whose limits the meters and the glyph show, Claude Code's or Codex's;
    /// from the footer's switch.
    meter_provider: Cell<Provider>,
    /// Whose spend the Stats window shows; from its toolbar.
    stats_provider: Cell<Provider>,
    /// The open and closed sessions and the stacks, as the sessions thread
    /// last published them; same discipline as `shared`.
    board: sessions::Board,
    /// The Cold group unfolded; it starts folded. Kept for the life of the
    /// process.
    cold_open: Cell<bool>,
    /// Every session seen running, for Dismiss to forget one; shared with
    /// the sessions thread, which keeps it.
    sticky: Arc<Mutex<Sticky>>,
    /// The Closed group unfolded; it starts folded, as Cold does.
    closed_open: Cell<bool>,
    /// The order the menu opened with, held until it closes so rows do not
    /// move under the pointer (see `sessions::arrange`).
    frozen: RefCell<Option<Vec<(OpenSession, Urgency)>>>,
    menu_open: Cell<bool>,
    /// The rate-limit capture, read between refreshes when it changes.
    capture: RefCell<crate::limits::CaptureWatch>,
    /// The Settings window, built the first time the gear is clicked.
    settings: RefCell<Option<SettingsWindow>>,
    /// The Closed Sessions window, built the first time it is asked for.
    closed_window: RefCell<Option<ClosedWindow>>,
    /// Results of background actions, waiting for the main thread to show.
    outbox: Outbox,
    /// The "Auto caffeinate" switch; shared with the poller.
    auto_awake: AutoCaffeinate,
    /// Which surface the menu bar icon opens, as applied to the status
    /// item: Settings' choice once [`apply_layout`](BarController::apply_layout) has run.
    layout: Cell<Layout>,
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    /// The Compact layout's popover, built up front like the menu.
    compact: CompactPopover,
    rows: Rows,
    /// The repaint timer, invalidated when this is dropped.
    timer: RefCell<Option<sl_macos::timer::RepeatingTimer>>,
    /// Whether `timer` is the fast one (see [`TICK_INTERVAL`]).
    tick_fast: Cell<bool>,
    /// This controller, for a timer built after [`install`](BarController::install).
    me: RefCell<std::rc::Weak<BarController>>,
    /// The Stats window, built the first time it is asked for and kept: it
    /// only orders out when closed.
    stats: RefCell<Option<StatsWindow>>,
    /// The badge last drawn on the menu bar icon, so `setImage:` -- real
    /// layout work on a live status item -- only happens when it changes.
    last_bar: Cell<Option<BarIcon>>,
}

/// The tray: the status item, its menu and popover, the windows, and the
/// repaint timer, answering every click through [`perform`](Self::perform).
pub struct BarController {
    ivars: Ivars,
    mtm: MainThreadMarker,
}

impl BarController {
    fn ivars(&self) -> &Ivars {
        &self.ivars
    }

    fn mtm(&self) -> MainThreadMarker {
        self.mtm
    }

    /// Answer one click.
    fn perform(&self, act: Act, target: &Target, sender: &AnyObject) {
        match act {
            Act::ToggleCold => self.toggle_cold(sender),
            Act::ToggleClosed => self.toggle_closed(sender),
            Act::OpenStackPr => self.open_stack_pr(target),
            Act::OpenStack => self.open_stack(target),
            Act::OpenClosedZed => self.open_closed_zed(target),
            Act::CopyClosedId => self.copy_closed_id(target),
            Act::DismissClosed => self.dismiss_closed(target),
            Act::ShowClosedWindow => self.show_closed_window(sender),
            Act::SearchClosed => self.search_closed(sender),
            Act::ClosedWindowZed => self.closed_window_zed(target),
            Act::ClosedWindowCopyId => self.closed_window_copy_id(target),
            Act::ClosedWindowDismiss => self.closed_window_dismiss(target),
            Act::ShowStats => self.show_stats(sender),
            Act::ToggleAutoAwake => self.toggle_auto_awake(sender),
            Act::TogglePopover => self.toggle_popover(sender),
            Act::ToggleCard => self.toggle_card(target),
            Act::ToggleClosedGroup => self.toggle_closed_group(sender),
            Act::FilterSessions => self.filter_sessions(sender),
            Act::ShowSettings => self.show_settings(sender),
            Act::SettingsLayout => self.settings_layout(sender),
            Act::StatsProvider => self.stats_provider(sender),
            Act::ShowMeterPage => self.show_meter_page(sender),
            Act::SettingsMeters => self.settings_meters(sender),
            Act::ToggleSpend => self.toggle_spend(sender),
            Act::SettingsUsage => Self::settings_usage(sender),
            Act::SettingsPr => Self::settings_pr(sender),
            Act::ToggleNotify => Self::toggle_notify(sender),
            Act::SettingsCacheLead => Self::settings_cache_lead(sender),
            Act::SettingsNeedsYou => self.settings_needs_you(sender),
            Act::SettingsRing => self.settings_ring(sender),
            Act::SettingsMaxSessions => self.settings_max_sessions(sender),
            Act::SettingsSort => self.settings_sort(sender),
            Act::SettingsCold => self.settings_cold(sender),
            Act::SettingsDensity => self.settings_density(sender),
            Act::SettingsBarWindow => self.settings_bar_window(sender),
            Act::ToggleBarBadge => self.toggle_bar_badge(sender),
            Act::CopyCwd => self.copy_cwd(target),
            Act::CopyReview => self.copy_review(target),
            Act::OpenReviewInZed => self.open_review_in_zed(target),
            Act::OpenZed => self.open_zed(target),
            Act::OpenPr => self.open_pr(target),
            Act::OpenLinear => self.open_linear(target),
            Act::ToggleRcAwake => self.toggle_rc_awake(sender),
            Act::CopyRcId => self.copy_rc_id(target),
            Act::ToggleLinear => Self::toggle_linear(sender),
            Act::ShowLogs => self.show_logs(target),
            Act::RetryJob => self.retry_job(target),
            Act::CopyJobLink => self.copy_job_link(target),
            Act::Quit => self.quit(sender),
        }
        // A click can open a window or start an action whose result is
        // wanted promptly.
        self.retick();
    }

    /// Whether anything that changes by the second is up -- the menu, the
    /// popover, a window -- or an action's result is on its way.
    fn wants_fast_tick(&self) -> bool {
        let ivars = self.ivars();
        let visible = |shown: Option<bool>| shown.unwrap_or(false);
        ivars.menu_open.get()
            || ivars.compact.is_shown()
            || visible(ivars.stats.borrow().as_ref().map(StatsWindow::is_visible))
            || visible(ivars.settings.borrow().as_ref().map(SettingsWindow::is_visible))
            || visible(ivars.closed_window.borrow().as_ref().map(ClosedWindow::is_visible))
            || actions::running()
            || !ivars.outbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty()
    }

    /// Tick fast or slow, as [`wants_fast_tick`](Self::wants_fast_tick)
    /// says; a no-op when the timer already does.
    fn retick(&self) {
        let fast = self.wants_fast_tick();
        if fast != self.ivars().tick_fast.get() {
            self.set_tick(fast);
        }
    }

    /// Replace the timer with a fast or a slow one.
    fn set_tick(&self, fast: bool) {
        let ivars = self.ivars();
        let (interval, tolerance) = if fast { (TICK_INTERVAL, TICK_TOLERANCE) } else { (IDLE_TICK_INTERVAL, IDLE_TICK_TOLERANCE) };
        // Gotcha #1: in the run loop's common modes, so it keeps firing while
        // the menu is open (see `RepeatingTimer`). It holds the controller
        // weakly: the controller owns the timer, not the other way round.
        let this = ivars.me.borrow().clone();
        let timer = sl_macos::timer::RepeatingTimer::common_modes(interval, tolerance, move || {
            if let Some(this) = this.upgrade() {
                this.tick();
            }
        });
        // The old timer is invalidated as it is dropped here.
        *ivars.timer.borrow_mut() = Some(timer);
        ivars.tick_fast.set(fast);
    }

    /// Repaint immediately on open so the menu never shows a title that is
    /// up to a second stale, and trace once -- under `SL_TRAY_DEBUG=1`
    /// this line appearing is the only cheap proof that the delegate was
    /// actually retained (gotcha #2); if the controller had been dropped,
    /// it would silently never print. Gated like every other trace: an
    /// accessory app launched from Finder or launchd has nobody reading
    /// its stderr, and an unconditional print per menu open is unbounded
    /// log noise.
    fn menu_will_open(&self) {
        crate::trace("menuWillOpen:");
        // A fresh order for this opening, held until it closes.
        *self.ivars().frozen.borrow_mut() = None;
        self.ivars().menu_open.set(true);
        self.repaint();
        self.retick();
    }

    fn menu_did_close(&self) {
        crate::trace("menuDidClose:");
        let ivars = self.ivars();
        ivars.menu_open.set(false);
        *ivars.frozen.borrow_mut() = None;
        self.retick();
    }

    /// The Compact popover closed, by any route: a click elsewhere, Esc,
    /// an action, the menu bar icon.
    fn popover_did_close(&self) {
        self.ivars().compact.did_close();
        self.retick();
    }

    /// A click on the Cold row: fold or unfold the cold sessions. A view
    /// in a menu item takes the click without closing the menu.
    fn toggle_cold(&self, _sender: &AnyObject) {
        let open = &self.ivars().cold_open;
        open.set(!open.get());
        // Now, not on the next tick: that is up to a second away.
        self.repaint();
    }

    /// A click on the Closed row: fold or unfold the closed sessions.
    fn toggle_closed(&self, _sender: &AnyObject) {
        let open = &self.ivars().closed_open;
        open.set(!open.get());
        self.repaint();
    }

    /// A PR in a stack's submenu: open it.
    fn open_stack_pr(&self, target: &Target) {
        if let Some((stack, Some(pr))) = self.stack(target)
            && let Some(url) = stack.pr_url(pr)
        {
            actions::open_url(&url);
        }
    }

    /// The stack on GitHub: its lowest PR not yet merged, whose page
    /// shows the whole stack.
    fn open_stack(&self, target: &Target) {
        if let Some((stack, _)) = self.stack(target)
            && let Some(url) = stack.next_open().and_then(|entry| stack.pr_url(entry.number))
        {
            actions::open_url(&url);
        }
    }

    fn open_closed_zed(&self, target: &Target) {
        if let Some(entry) = self.closed(target) {
            actions::open_in_zed(&entry.cwd);
        }
    }

    /// What resumes it, to paste in its worktree: `claude --resume <id>`
    /// or `codex resume <id>`.
    fn copy_closed_id(&self, target: &Target) {
        if let Some(entry) = self.closed(target) {
            copy_text(&entry.resume_command());
        }
    }

    /// Forget a closed session, saved at once, and drop its row now
    /// rather than on the next poll.
    fn dismiss_closed(&self, target: &Target) {
        if let Some(entry) = self.closed(target) {
            self.forget_closed(&entry.session_id);
        }
    }

    /// The Closed group's "All closed sessions…": the window with every
    /// one, searchable. Closes the menu or popover it was opened from.
    fn show_closed_window(&self, _sender: &AnyObject) {
        let ivars = self.ivars();
        ivars.menu.cancelTracking();
        ivars.compact.close();
        {
            let mut window = ivars.closed_window.borrow_mut();
            window.get_or_insert_with(|| ClosedWindow::new(self.mtm())).show();
        }
        self.update_closed_window();
    }

    /// Each keystroke in the Closed Sessions window's search field.
    fn search_closed(&self, _sender: &AnyObject) {
        self.update_closed_window();
    }

    fn closed_window_zed(&self, target: &Target) {
        if let Some(entry) = self.find_closed(target) {
            actions::open_in_zed(&entry.cwd);
        }
    }

    fn closed_window_copy_id(&self, target: &Target) {
        if let Some(entry) = self.find_closed(target) {
            copy_text(&entry.resume_command());
        }
    }

    fn closed_window_dismiss(&self, target: &Target) {
        let entry = self.find_closed(target);
        if let Some(entry) = entry {
            self.forget_closed(&entry.session_id);
        }
    }

    /// From the footer's Stats button. A button in a menu item's view does
    /// not close the menu by itself, so close it.
    fn show_stats(&self, _sender: &AnyObject) {
        self.ivars().menu.cancelTracking();
        self.ivars().compact.close();
        let provider = self.ivars().stats_provider.get();
        let snapshot = self.snapshot(provider);
        let mut stats = self.ivars().stats.borrow_mut();
        stats
            .get_or_insert_with(|| StatsWindow::new(self.mtm(), &snapshot, provider, view_prefs::codex_available()))
            .show(&snapshot, provider);
    }

    /// From the "Auto caffeinate" switch. Like the footer's buttons, a
    /// control in a menu item's view leaves the menu open.
    fn toggle_auto_awake(&self, _sender: &AnyObject) {
        // Flipped from what is stored rather than read off the sender:
        // the footers' buttons and the Settings window all send this.
        let on = !self.ivars().auto_awake.load(Ordering::Relaxed);
        self.ivars().auto_awake.store(on, Ordering::Relaxed);
        crate::awake::save(on);
        self.repaint();
    }

    /// The status item's button, in the Compact layout: open or close the
    /// popover.
    fn toggle_popover(&self, _sender: &AnyObject) {
        let ivars = self.ivars();
        let Some(button) = ivars.status_item.button(self.mtm()) else { return };
        // Painted before it shows, so it never opens on stale numbers.
        if !ivars.compact.is_shown() {
            self.repaint_compact(true);
        }
        ivars.compact.toggle(&button);
    }

    /// A click on a card in the popover: a session's, a stack's or a closed
    /// session's. Fold or unfold it.
    fn toggle_card(&self, target: &Target) {
        self.ivars().compact.toggle_card(target);
        self.repaint_compact(false);
    }

    /// A click on the popover's Closed card: show or hide the closed sessions.
    fn toggle_closed_group(&self, _sender: &AnyObject) {
        self.ivars().compact.toggle_closed_group();
        self.repaint_compact(false);
    }

    /// The popover's All / Needs you / Working / Cold filter.
    fn filter_sessions(&self, sender: &AnyObject) {
        self.ivars().compact.set_filter(sl_macos::action::selected_segment(sender));
        self.repaint_compact(false);
    }

    /// The footer's gear, in either layout: the Settings window. The menu
    /// or popover closes, as it does for Stats.
    fn show_settings(&self, _sender: &AnyObject) {
        let ivars = self.ivars();
        ivars.menu.cancelTracking();
        ivars.compact.close();
        let auto = ivars.auto_awake.load(Ordering::Relaxed);
        let mut settings = ivars.settings.borrow_mut();
        settings
            .get_or_insert_with(|| SettingsWindow::new(self.mtm(), sessions::awake_enabled()))
            .show(&self.settings_state(auto));
    }

    /// Settings: the layout, by its segment in `Layout::ALL`.
    fn settings_layout(&self, sender: &AnyObject) {
        let Some(layout) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| Layout::ALL.get(i).copied()) else { return };
        view_prefs::save_layout(layout);
        self.apply_layout(layout);
    }

    /// The Stats window's toolbar: whose spend it shows, by its segment in
    /// `Provider::ALL`.
    fn stats_provider(&self, sender: &AnyObject) {
        let Some(provider) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| Provider::ALL.get(i).copied()) else { return };
        let ivars = self.ivars();
        view_prefs::save_stats_provider(provider);
        ivars.stats_provider.set(provider);
        if let Some(window) = ivars.stats.borrow().as_ref() {
            window.update(crate::latest::get(&ivars.shared).select(provider), provider);
        }
    }

    /// A click on one of the meters' page marks: show that page's tool's
    /// limits, there and on the glyph. The menu or popover stays open.
    fn show_meter_page(&self, sender: &AnyObject) {
        let Some(provider) = usize::try_from(sl_macos::action::tag(sender)).ok().and_then(|page| METER_PAGES.get(page).copied()) else { return };
        let ivars = self.ivars();
        view_prefs::save_meter_provider(provider);
        ivars.meter_provider.set(provider);
        self.repaint();
    }

    /// Settings: the meter style, by its segment in `MeterStyle::ALL`.
    fn settings_meters(&self, sender: &AnyObject) {
        let Some(style) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| MeterStyle::ALL.get(i).copied()) else { return };
        view_prefs::save_meters(style);
        self.repaint();
    }

    /// Settings: the spend line under the meters, shown or not.
    fn toggle_spend(&self, sender: &AnyObject) {
        view_prefs::save_switch(view_prefs::Switch::ShowSpend, sl_macos::action::is_on(sender));
        self.repaint();
    }

    /// Settings: the least time between two usage refreshes, by segment.
    /// Saving it wakes the refresh thread's wait, which applies it at once.
    fn settings_usage(sender: &AnyObject) {
        if let Some(secs) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::USAGE_EVERY.get(i)) {
            view_prefs::save_usage_every(*secs);
        }
    }

    /// Settings: how long a PR's CI and reviews are reused, by segment.
    fn settings_pr(sender: &AnyObject) {
        if let Some(secs) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::PR_EVERY.get(i)) {
            view_prefs::save_pr_every(*secs);
        }
    }

    /// Settings: a notification kind, by its index in `NOTIFY_KINDS`.
    /// The sessions thread reads the choice on its next pass.
    fn toggle_notify(sender: &AnyObject) {
        if let Ok(index) = usize::try_from(sl_macos::action::tag(sender)) {
            view_prefs::save_notify(index, sl_macos::action::is_on(sender));
        }
    }

    /// Settings: how early a long cache's expiry is warned of, by segment.
    fn settings_cache_lead(sender: &AnyObject) {
        if let Some(mins) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::CACHE_LEAD_MINS.get(i)) {
            view_prefs::save_cache_lead(*mins);
        }
    }

    /// Settings: how little cache left means a session needs you, by segment.
    fn settings_needs_you(&self, sender: &AnyObject) {
        if let Some(mins) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::NEEDS_YOU_MINS.get(i)) {
            view_prefs::save_needs_you(*mins);
            self.repaint();
        }
    }

    /// Settings: where the context rings turn orange and red, by segment.
    fn settings_ring(&self, sender: &AnyObject) {
        if let Some((note, _)) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::RING_POINTS.get(i)) {
            view_prefs::save_ring_points(*note);
            self.repaint();
        }
    }

    /// Settings: how many open sessions are listed, by segment.
    fn settings_max_sessions(&self, sender: &AnyObject) {
        if let Some(count) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::MAX_SESSION_CHOICES.get(i)) {
            view_prefs::save_max_sessions(*count);
            self.repaint();
        }
    }

    /// Settings: the order within a group, by segment in `SortOrder::ALL`.
    fn settings_sort(&self, sender: &AnyObject) {
        if let Some(order) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| SortOrder::ALL.get(i).copied()) {
            view_prefs::save_sort(order);
            self.repaint();
        }
    }

    /// Settings: what the Cold group does, by segment in `ColdMode::ALL`.
    fn settings_cold(&self, sender: &AnyObject) {
        if let Some(mode) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| ColdMode::ALL.get(i).copied()) {
            view_prefs::save_cold(mode);
            self.repaint();
        }
    }

    /// Settings: how much room the popover's cards take, by segment.
    fn settings_density(&self, sender: &AnyObject) {
        if let Some(density) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| view_prefs::Density::ALL.get(i).copied()) {
            view_prefs::save_density(density);
            self.repaint();
        }
    }

    /// Settings: which window the menu bar icon's meter shows, by segment.
    fn settings_bar_window(&self, sender: &AnyObject) {
        if let Some(window) = usize::try_from(sl_macos::action::selected_segment(sender)).ok().and_then(|i| BarWindow::ALL.get(i).copied()) {
            view_prefs::save_bar_window(window);
            self.repaint();
        }
    }

    /// Settings: the menu bar icon's badge, shown or not.
    fn toggle_bar_badge(&self, sender: &AnyObject) {
        view_prefs::save_switch(view_prefs::Switch::BarBadge, sl_macos::action::is_on(sender));
        self.repaint();
    }

    fn copy_cwd(&self, target: &Target) {
        if let Some(session) = self.session(target) {
            copy_text(&session.cwd);
        }
    }

    fn copy_review(&self, target: &Target) {
        if let Some(session) = self.session(target)
            && let Some(pr) = session.gh.as_ref().and_then(|g| g.pr_number)
        {
            actions::copy_review(self.ivars().outbox.clone(), session.cwd, pr);
        }
    }

    fn open_review_in_zed(&self, target: &Target) {
        if let Some(session) = self.session(target)
            && let Some(pr) = session.gh.as_ref().and_then(|g| g.pr_number)
        {
            actions::open_review_in_zed(self.ivars().outbox.clone(), session.cwd, pr);
        }
    }

    fn open_zed(&self, target: &Target) {
        if let Some(session) = self.session(target) {
            actions::open_in_zed(&session.cwd);
        }
    }

    fn open_pr(&self, target: &Target) {
        if let Some(session) = self.session(target)
            && let Some(url) = session.gh.and_then(|g| g.url)
        {
            actions::open_url(&url);
        }
    }

    fn open_linear(&self, target: &Target) {
        if let Some(session) = self.session(target)
            && let Some(linear) = session.linear
        {
            actions::open_url(&linear.url);
        }
    }

    /// Settings: whether `/rc` sessions keep the Mac awake.
    fn toggle_rc_awake(&self, sender: &AnyObject) {
        let on = sl_macos::action::is_on(sender);
        view_prefs::save_switch(view_prefs::Switch::RcAwake, on);
        self.repaint();
    }

    /// The remote-control session id of a session with `/rc` on.
    fn copy_rc_id(&self, target: &Target) {
        if let Some(session) = self.session(target)
            && let Some(id) = session.remote_control
        {
            copy_text(&id);
        }
    }

    fn toggle_linear(sender: &AnyObject) {
        view_prefs::save_switch(view_prefs::Switch::ShowLinear, sl_macos::action::is_on(sender));
    }

    fn show_logs(&self, target: &Target) {
        if let Some((session, job)) = self.job(target) {
            actions::show_logs(self.ivars().outbox.clone(), session.cwd, job.name, job.link);
        }
    }

    fn retry_job(&self, target: &Target) {
        if let Some((session, job)) = self.job(target)
            && confirm(self.mtm(), &format!("Re-run \"{}\"?", job.name), "This asks GitHub to run the job again, using CI minutes.")
        {
            actions::retry_job(self.ivars().outbox.clone(), session.cwd, job.name, job.link);
        }
    }

    fn copy_job_link(&self, target: &Target) {
        if let Some((_, job)) = self.job(target)
            && let Some(link) = job.link
        {
            copy_text(&link);
        }
    }

    /// The footers' Quit, in either layout.
    fn quit(&self, _sender: &AnyObject) {
        if !crate::effects::intercept(crate::effects::Effect::Quit) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
}

impl BarController {
    /// Timer callback. Registered into the run loop's common modes, so it
    /// keeps firing while the menu is being tracked.
    fn tick(&self) {
        // `SL_TRAY_DEBUG=1` turns this into the manual check for gotcha #1:
        // open the menu and keep watching. If the ticks keep coming while
        // the menu is open, the timer really is in the common run loop
        // modes; if they stop between `menuWillOpen:` and `menuDidClose:`,
        // it fell back to the default mode only.
        crate::trace("tick:");
        TICKS.fetch_add(1, Ordering::Relaxed);
        self.repaint();
        self.show_outcomes();
        // A window closed, or an action finished: tick slowly again.
        self.retick();
    }

    /// Build the controller, its status item and its whole menu.
    ///
    /// Every row the menu will ever show is created here, including the spare
    /// rows the variable-length lists keep hidden -- see rule #3 in the module
    /// docs.
    ///
    /// Returns a `Retained<Self>` that the caller **must** keep alive for the
    /// life of the process -- see gotcha #2 in the module docs. Nothing here
    /// registers the controller with AppKit yet; [`install`](Self::install)
    /// does that, on a value the caller already owns.
    pub fn new(
        mtm: MainThreadMarker,
        shared: refresh::Shared,
        board: sessions::Board,
        auto_awake: AutoCaffeinate,
        sticky: Arc<Mutex<Sticky>>,
    ) -> Rc<Self> {
        let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);

        let menu = NSMenu::new(mtm);
        // Manual enablement: with autoenabling on, every item whose action no
        // object in the responder chain implements is greyed out, which would
        // grey out the informational rows below.
        menu.setAutoenablesItems(false);

        // The section the menu bar title is drawn from, so it sits first.
        let panel = LimitsPanel::new(mtm);
        let limits_item = NSMenuItem::new(mtm);
        limits_item.setView(Some(&panel.view));
        let limits_row = Row::append(&menu, limits_item);
        limits_row.hide();
        let limits = LimitRows { row: limits_row, panel, updated: NoteRow::new(mtm, &menu) };
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let open_slots: Vec<Slot> = (0..MAX_SESSIONS)
            .map(|_| {
                let caption = CaptionRow::new(mtm, &menu);
                let cold_view = RowView::new(mtm);
                crate::act::bind(&cold_view, Act::ToggleCold);
                let cold_item = NSMenuItem::new(mtm);
                cold_item.setView(Some(&cold_view));
                let cold = Row::append(&menu, cold_item);
                cold.hide();
                Slot { caption, cold, cold_view, menu: SessionMenu::new(mtm, &menu) }
            })
            .collect();
        let more = CaptionRow::new(mtm, &menu);
        let closed = ClosedRows::new(mtm, &menu);
        let stack_rows = StackRows::new(mtm, &menu);
        let open_separator = Row::separator(mtm, &menu);
        open_separator.hide();
        // Shown with the open sessions, it is also what sets them off from the
        // footer; with none open, the separator under the limits does.
        let open = OpenRows { slots: open_slots, more, closed, stacks: stack_rows, separator: open_separator };

        // Everything the snapshot knows beyond the limits and the open
        // sessions -- costs, models, sessions, history -- lives in the Stats
        // window, opened by the footer's button.
        let footer = FooterRow::new(mtm, &menu);

        Rc::new(Self {
            mtm,
            ivars: Ivars {
            shared,
            board,
            cold_open: Cell::new(false),
            sticky,
            closed_open: Cell::new(false),
            frozen: RefCell::new(None),
            menu_open: Cell::new(false),
            capture: RefCell::new(crate::limits::CaptureWatch::new(sl_core::paths::limits_path())),
            settings: RefCell::new(None),
            closed_window: RefCell::new(None),
            outbox: Outbox::default(),
            auto_awake,
            layout: Cell::new(view_prefs::load_layout()),
            meter_provider: Cell::new(view_prefs::load_meter_provider()),
            stats_provider: Cell::new(view_prefs::load_stats_provider()),
            status_item,
            menu,
            compact: CompactPopover::new(mtm),
            rows: Rows { limits, open, footer },
            timer: RefCell::new(None),
            tick_fast: Cell::new(false),
            me: RefCell::new(std::rc::Weak::new()),
            last_bar: Cell::new(None),
            stats: RefCell::new(None),
            },
        })
    }

    /// Wire the controller into AppKit: menu onto the status item, delegate
    /// onto the menu, repaint timer onto the run loop.
    ///
    /// Takes `&self`, so it can only be called on a controller the caller is
    /// already holding -- the borrow handed to `setDelegate:` is a borrow of
    /// that live value, never of a temporary.
    pub fn install(self: &Rc<Self>) {
        let ivars = self.ivars();

        // Every click on every surface comes here. Held weakly: the controls
        // belong to the controller, not the other way round.
        let this = Rc::downgrade(self);
        crate::act::set_handler(move |act, target, sender| {
            if let Some(this) = this.upgrade() {
                this.perform(act, target, sender);
            }
        });
        let this = Rc::downgrade(self);
        ivars.compact.on_close(move || {
            if let Some(this) = this.upgrade() {
                this.popover_did_close();
            }
        });

        // Icon only: the numbers live in the menu. The image itself is set by
        // `repaint`, since its badge follows the open sessions. `button()` is
        // `None` if the item was already removed from the bar, in which case
        // there is nothing to decorate and nothing to report.
        if let Some(button) = ivars.status_item.button(self.mtm()) {
            button.setTitle(&NSString::new());
            button.setImagePosition(NSCellImagePosition::ImageOnly);
        }

        // Repaint the moment the menu opens, so it never shows a title up to
        // a second stale; hold its order until it closes.
        let (open, close) = (Rc::downgrade(self), Rc::downgrade(self));
        sl_macos::delegate::set_menu_delegate(
            &ivars.menu,
            sl_macos::delegate::MenuEvents {
                will_open: Box::new(move || {
                    if let Some(this) = open.upgrade() {
                        this.menu_will_open();
                    }
                }),
                did_close: Box::new(move || {
                    if let Some(this) = close.upgrade() {
                        this.menu_did_close();
                    }
                }),
            },
        );

        *ivars.me.borrow_mut() = Rc::downgrade(self);
        self.set_tick(false);

        // Hangs the menu on the status item, or points its button at the
        // popover, and paints.
        self.apply_layout(ivars.layout.get());
    }

    /// The Regular layout's menu, for the self-test and the view dumps.
    pub(crate) fn menu(&self) -> Retained<NSMenu> {
        self.ivars().menu.clone()
    }

    /// The menu bar icon's button, which opens the popover in Compact.
    pub(crate) fn status_button(&self) -> Option<Retained<objc2_app_kit::NSStatusBarButton>> {
        self.ivars().status_item.button(self.mtm())
    }

    /// The Compact popover's content and whether it is showing.
    pub(crate) fn popover(&self) -> (Retained<NSView>, bool) {
        (self.ivars().compact.root(), self.ivars().compact.is_shown())
    }

    /// Close the popover, as a click elsewhere would.
    pub(crate) fn close_popover(&self) {
        self.ivars().compact.close();
    }

    /// Switch the layout as Settings does.
    pub(crate) fn set_layout(&self, layout: Layout) {
        view_prefs::save_layout(layout);
        self.apply_layout(layout);
    }

    /// Show the tray as `layout` from now on: Regular hangs the menu on the
    /// status item; Compact takes it off, so a click reaches the button's
    /// action, which opens the popover instead.
    fn apply_layout(&self, layout: Layout) {
        let ivars = self.ivars();
        ivars.layout.set(layout);
        let button = ivars.status_item.button(self.mtm());
        match layout {
            Layout::Regular => {
                ivars.compact.close();
                ivars.status_item.setMenu(Some(&ivars.menu));
                if let Some(button) = &button {
                    sl_macos::action::unbind(button);
                }
            }
            Layout::Compact => {
                ivars.status_item.setMenu(None);
                if let Some(button) = &button {
                    crate::act::bind(button, Act::TogglePopover);
                }
            }
        }
        self.repaint();
    }

    /// Paint the popover, if it is showing or about to (`force`).
    fn repaint_compact(&self, force: bool) {
        let ivars = self.ivars();
        if !force && !ivars.compact.is_shown() {
            return;
        }
        let provider = ivars.meter_provider.get();
        let snapshot = self.snapshot(provider);
        let now = now_ms();
        let board = crate::latest::get(&ivars.board);
        let (meter_slots, note) = limit_models(&snapshot, provider, view_prefs::load_switch(view_prefs::Switch::ShowSpend), now);
        let auto = sessions::awake_enabled().then(|| sessions::Awake::now(ivars.auto_awake.load(Ordering::Relaxed)));
        let footer = footer_text(&snapshot, now);
        let input = CompactInput {
            sessions: &board.open,
            now,
            auto_awake: auto,
            meter_slots: &meter_slots,
            note: note.as_deref(),
            meters: view_prefs::load_meters(),
            footer: &footer,
            width: tray_width(self.mtm(), 0.26, 460.0, 560.0),
            max_list_height: list_room(self.mtm()),
            closed: &board.closed,
            stacks: &board.stacks,
            marks: view_prefs::show_marks(),
            meter_dots: meter_dots(provider),
            tuning: view_prefs::load_tuning(),
        };
        ivars.compact.update(&input);
    }

    /// Close whatever the click came from: a view in a menu item does not
    /// close the menu as a plain item does, and an action is the end of a
    /// visit to the popover. Whichever is not open, closing it does nothing.
    fn dismiss_surfaces(&self) {
        let ivars = self.ivars();
        ivars.menu.cancelTracking();
        ivars.compact.close();
    }

    /// The open session `target` is about, as it is now, closing what the
    /// click came from. `None` once it has gone.
    fn session(&self, target: &Target) -> Option<OpenSession> {
        self.dismiss_surfaces();
        let (Target::Session(id) | Target::Job { session: id, .. }) = target else { return None };
        crate::latest::get(&self.ivars().board).open.iter().find(|session| session.session_id == *id).cloned()
    }

    /// The session and the failed job `target` is about.
    fn job(&self, target: &Target) -> Option<(OpenSession, sl_core::gh::FailedJob)> {
        let session = self.session(target)?;
        let Target::Job { job, .. } = target else { return None };
        let job = session.gh.as_ref()?.ci.as_ref()?.failed.iter().find(|failed| failed.name == *job)?.clone();
        Some((session, job))
    }

    /// The closed session `target` is about, without closing anything: the
    /// Closed Sessions window's buttons stay where they are.
    fn find_closed(&self, target: &Target) -> Option<Remembered> {
        let Target::Closed(id) = target else { return None };
        crate::latest::get(&self.ivars().board).closed.iter().find(|entry| entry.session_id == *id).cloned()
    }

    /// As [`find_closed`](Self::find_closed), from the menu or the popover,
    /// which it closes.
    fn closed(&self, target: &Target) -> Option<Remembered> {
        self.dismiss_surfaces();
        self.find_closed(target)
    }

    /// The stack `target` is about, and the PR in it when one is named;
    /// closes what the click came from.
    fn stack(&self, target: &Target) -> Option<(StackView, Option<i64>)> {
        self.dismiss_surfaces();
        crate::tray_model::stack_for(&crate::latest::get(&self.ivars().board).stacks, target)
    }

    /// Forget a closed session, saved at once, and drop its row now rather
    /// than on the next poll.
    fn forget_closed(&self, session_id: &str) {
        let ivars = self.ivars();
        {
            // `sticky`, then the board: the order the sessions thread
            // takes them in, so the two cannot deadlock, and no poll can
            // publish between forgetting it and dropping its row.
            let mut sticky = ivars.sticky.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if sticky.dismiss(session_id) {
                sticky.save();
            }
            crate::latest::edit(&ivars.board, |published| published.closed.retain(|e| e.session_id != session_id));
        }
        self.repaint();
        self.update_closed_window();
    }

    /// Fill the Closed Sessions window, if it is up, from the latest list.
    fn update_closed_window(&self) {
        let ivars = self.ivars();
        if let Some(window) = ivars.closed_window.borrow().as_ref()
            && window.is_visible()
        {
            window.update(&crate::latest::get(&ivars.board).closed, now_ms());
        }
    }

    /// Do the AppKit half of whatever background actions have finished.
    fn show_outcomes(&self) {
        for outcome in actions::drain(&self.ivars().outbox) {
            match outcome {
                Outcome::Copy(text) => copy_text(&text),
                Outcome::Popup { title, body, markdown } => present_popup(self.mtm(), &title, &body, markdown),
            }
        }
    }

    /// The settings in use, for the Settings window's controls.
    fn settings_state(&self, auto: bool) -> crate::settings_window::SettingsState {
        let ivars = self.ivars();
        crate::settings_window::SettingsState {
            layout: ivars.layout.get(),
            meters: view_prefs::load_meters(),
            spend: view_prefs::load_switch(view_prefs::Switch::ShowSpend),
            poll: view_prefs::load_poll(),
            auto,
            notify: view_prefs::load_notify(),
            tuning: view_prefs::load_tuning(),
        }
    }

    /// The latest snapshot for `provider`, with the rate-limit capture as of
    /// now rather than as of the last refresh (Codex's limits are its own).
    fn snapshot(&self, provider: view_prefs::Provider) -> crate::snapshot::Snapshot {
        let mut snapshot = crate::latest::get(&self.ivars().shared).select(provider).clone();
        if provider != view_prefs::Provider::Codex {
            snapshot.rate_limits = self.ivars().capture.borrow_mut().fresher(snapshot.rate_limits);
        }
        snapshot
    }

    /// Redraw the menu bar title and every live menu row from the current
    /// shared snapshot. Cheap and idempotent: safe to call every second, and
    /// safe to call while the menu is open (it only assigns titles and flips
    /// `hidden`, it never adds or removes items).
    fn repaint(&self) {
        let ivars = self.ivars();
        let provider = ivars.meter_provider.get();
        let snapshot = self.snapshot(provider);
        let now = now_ms();
        let rows = &ivars.rows;

        let awake_enabled = sessions::awake_enabled();
        let awake = sessions::Awake::now(ivars.auto_awake.load(Ordering::Relaxed));
        let auto = awake.auto;
        // Nothing is up but the menu bar icon: paint that alone. The menu is
        // painted in full the moment it opens (`menu_will_open`), and the
        // popover when it shows.
        if !self.wants_fast_tick() {
            let tuning = view_prefs::load_tuning();
            self.paint_bar(sessions::bar_state(&crate::latest::get(&ivars.board).open, awake, awake_enabled), bar_meter(&snapshot, provider, tuning.bar_window, now), tuning.bar_badge);
            return;
        }
        if let Some(window) = ivars.stats.borrow().as_ref()
            && window.is_visible()
        {
            let stats_provider = ivars.stats_provider.get();
            window.update(crate::latest::get(&ivars.shared).select(stats_provider), stats_provider);
        }
        if let Some(window) = ivars.settings.borrow().as_ref()
            && window.is_visible()
        {
            window.sync(&self.settings_state(auto));
        }
        self.update_closed_window();
        if ivars.layout.get() == Layout::Compact {
            let tuning = view_prefs::load_tuning();
            self.paint_bar(sessions::bar_state(&crate::latest::get(&ivars.board).open, awake, awake_enabled), bar_meter(&snapshot, provider, tuning.bar_window, now), tuning.bar_badge);
            self.repaint_compact(false);
            return;
        }
        let board = crate::latest::get(&ivars.board);
        let tuning = view_prefs::load_tuning();
        // One width, from the screen, whatever the rows hold: a menu sized by
        // its widest row jumps as branch names and numbers come and go.
        let width = tray_width(self.mtm(), 0.3, 520.0, 640.0);
        ivars.menu.setMinimumWidth(width);
        paint_limits(&rows.limits, &snapshot, provider, view_prefs::load_meters(), width, now);
        self.paint_bar(sessions::bar_state(&board.open, awake, awake_enabled), bar_meter(&snapshot, provider, tuning.bar_window, now), tuning.bar_badge);
        let arranged = sessions::arrange(&board.open, ivars.frozen.borrow().as_deref(), now, &tuning);
        if ivars.menu_open.get() {
            *ivars.frozen.borrow_mut() = Some(arranged.clone());
        }
        let marks = view_prefs::show_marks();
        paint_open(&rows.open, &arranged, &OpenPaint { cold_open: ivars.cold_open.get(), auto_awake: awake_enabled.then_some(awake), marks, now, tuning: &tuning });
        let (closed, stacks) = (&board.closed, &board.stacks);
        paint_closed(&rows.open.closed, closed, ivars.closed_open.get(), marks, now);
        paint_stacks(&rows.open.stacks, stacks);
        // The line under the sessions, open or closed, and stacks sets them off from the footer.
        if arranged.is_empty() && closed.is_empty() && stacks.is_empty() { rows.open.separator.hide() } else { rows.open.separator.show() }

        // The one row that changes every single second, so a stalled timer is
        // visible at a glance rather than being mistaken for "nothing new".
        rows.footer.set(&footer_text(&snapshot, now), awake_enabled.then_some(auto));
    }

    /// Put the icon for `state` and the session `meter` on the menu bar, if
    /// either changed. Dimmed, with no badge, while no session is open.
    fn paint_bar(&self, state: BarState, meter: Option<BarMeter>, badge: bool) {
        let ivars = self.ivars();
        let Some(button) = ivars.status_item.button(self.mtm()) else { return };
        // The image draws itself in whatever appearance it is drawn in, but
        // keying on the menu bar's appearance as well means a missed redraw
        // after the menu bar flips light/dark lasts a tick at most.
        let icon = BarIcon { state, meter, badge, dark: is_dark(&button.effectiveAppearance()), provider: ivars.meter_provider.get() };
        if ivars.last_bar.get() == Some(icon) {
            return;
        }
        // As with the title cache this replaced: only remember a paint that
        // actually happened.
        let image = bar_image(icon);
        button.setImage(Some(&image));
        ivars.last_bar.set(Some(icon));
    }
}

/// Fill the Open Sessions block, or hide all of it when nothing is open.
/// `sessions` arrives grouped by urgency (see `sessions::arrange`): each group
/// opens with its caption, except Cold, which opens with a row of its own
/// that folds the rest of it away.
fn paint_open(open: &OpenRows, sessions: &[(OpenSession, Urgency)], paint: &OpenPaint) {
    let OpenPaint { cold_open, auto_awake, marks, now, tuning } = *paint;
    let cold = tuning.cold_mode(Layout::Regular);
    // Cold, hidden, is not a session to count or to reserve a row for.
    let listed: Vec<&(OpenSession, Urgency)> = sessions.iter().filter(|(_, group)| !(cold == ColdMode::Hide && *group == Urgency::Cold)).collect();
    let capacity = open.slots.len().min(tuning.max_sessions);
    let shown = &listed[..listed.len().min(capacity)];
    // The separator under the block is the caller's: closed sessions share it.
    for (i, (slot, (session, group))) in open.slots.iter().zip(shown.iter().copied()).enumerate() {
        let first = i == 0 || shown[i - 1].1 != *group;
        let members: Vec<&OpenSession> = shown.iter().filter(|(_, g)| g == group).map(|(s, _)| s).collect();
        // Cold folds behind a row of its own, unless it is to be shown in full.
        let folds = *group == Urgency::Cold && cold == ColdMode::Fold;
        match (first, *group) {
            (true, Urgency::Cold) if folds => {
                slot.caption.row.hide();
                slot.cold.show();
                slot.cold_view.set(cold_row(&members, cold_open));
            }
            (true, group) => {
                slot.cold.hide();
                slot.caption.set(group.label(), members.len(), group == Urgency::NeedsYou);
            }
            (false, _) => {
                slot.caption.row.hide();
                slot.cold.hide();
            }
        }
        if folds && !cold_open {
            slot.menu.row.hide();
        } else {
            slot.menu.paint(session, auto_awake.is_some_and(|awake| awake.holds(session)), marks, now, tuning);
        }
    }
    for slot in &open.slots[shown.len()..] {
        slot.caption.row.hide();
        slot.cold.hide();
        slot.menu.row.hide();
    }
    match listed.len() - shown.len() {
        0 => open.more.row.hide(),
        extra => open.more.set("More sessions", extra, false),
    }
}

/// What one [`paint_open`] needs beside the sessions.
#[derive(Clone, Copy)]
struct OpenPaint<'a> {
    /// The Cold group unfolded (when it folds at all).
    cold_open: bool,
    /// `Some` while the tray may hold the Mac awake, `None` when it may not.
    auto_awake: Option<sessions::Awake>,
    /// Mark each row with whose session it is.
    marks: bool,
    now: i64,
    tuning: &'a Tuning,
}

/// The Cold group's row: `Cold · 3`, and under it whose caches have gone.
fn cold_row(members: &[&OpenSession], open: bool) -> RowContent {
    let names: Vec<&str> = members.iter().map(|s| s.name.as_str()).collect();
    RowContent {
        icon: Some(Icon::Cold),
        ring: None,
        title: vec![sessions::seg(format!("Cold · {}", members.len()), Tone::Plain)],
        detail: vec![sessions::seg(format!("{} — caches expired", names.join(" · ")), Tone::Dim)],
        right: None,
        mark: None,
        chevron: Some(open),
        tinted: true,
        dimmed: false,
    }
}

/// The tray's width: `share` of the main screen's width, kept between `min`
/// and `max` points, so the menu or popover stays one width on a given screen.
pub(crate) fn tray_width(mtm: MainThreadMarker, share: f64, min: f64, max: f64) -> f64 {
    let screen = objc2_app_kit::NSScreen::mainScreen(mtm).map_or(1512.0, |screen| screen.visibleFrame().size.width);
    (screen * share).clamp(min, max).round()
}

/// How tall the popover's card list may be: the main screen's visible height,
/// less room for the rest of the popover and a margin, never under 200 points.
fn list_room(mtm: MainThreadMarker) -> f64 {
    let screen = objc2_app_kit::NSScreen::mainScreen(mtm).map_or(800.0, |screen| screen.visibleFrame().size.height);
    (screen - 320.0).max(200.0)
}

/// Put `text` on the general pasteboard.
fn copy_text(text: &str) {
    if crate::effects::intercept(crate::effects::Effect::Copy(text.to_owned())) {
        return;
    }
    sl_macos::system::copy_to_pasteboard(text);
}

/// A modal yes/no. `true` for the first (affirmative) button.
fn confirm(mtm: MainThreadMarker, message: &str, detail: &str) -> bool {
    if let Some(answer) = crate::effects::scripted_confirm(message) {
        return answer;
    }
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str("Re-run"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    NSApplication::sharedApplication(mtm).activate();
    alert.runModal() == NSAlertFirstButtonReturn
}

/// A modal popup with `body` in a scrolling, read-only text view and Close /
/// Copy buttons. Markdown bodies are rendered; anything else is shown as plain
/// monospaced text. Copy always copies the source text.
fn present_popup(mtm: MainThreadMarker, title: &str, body: &str, render_markdown: bool) {
    if crate::effects::intercept(crate::effects::Effect::Popup(title.to_owned())) {
        return;
    }
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.addButtonWithTitle(&NSString::from_str("Close"));
    alert.addButtonWithTitle(&NSString::from_str("Copy"));

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(760.0, 440.0));
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
    scroll.setHasVerticalScroller(true);
    let text = NSTextView::initWithFrame(NSTextView::alloc(mtm), frame);
    text.setEditable(false);
    text.setVerticallyResizable(true);
    text.setHorizontallyResizable(false);
    match sl_macos::system::text_storage(&text) {
        Some(storage) if render_markdown => storage.setAttributedString(&markdown_title(&markdown::parse(body))),
        _ => {
            text.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(12.0, sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular))));
            text.setString(&NSString::from_str(body));
        }
    }
    scroll.setDocumentView(Some(&text));
    alert.setAccessoryView(Some(&scroll));

    NSApplication::sharedApplication(mtm).activate();
    if alert.runModal() == NSAlertSecondButtonReturn {
        copy_text(body);
    }
}

/// Styled markdown runs as one attributed string: headings sized and bold,
/// code monospaced on a tinted background, links clickable, tones coloured.
fn markdown_title(runs: &[Run]) -> Retained<NSAttributedString> {
    let out = NSMutableAttributedString::new();
    for run in runs {
        let style = &run.style;
        let (regular, bold) = (sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular), sl_macos::consts::font_weight(sl_macos::consts::Weight::Bold));
        let weight = if style.bold { bold } else { regular };
        let font = if style.code {
            NSFont::monospacedSystemFontOfSize_weight(12.0, weight)
        } else {
            let size = match style.heading {
                0 => 13.0,
                1 => 20.0,
                2 => 17.0,
                3 => 15.0,
                _ => 14.0,
            };
            NSFont::systemFontOfSize_weight(size, weight)
        };
        let color = match (&style.link, style.tone) {
            (Some(_), _) => NSColor::linkColor(),
            (None, Some(tone)) => tone_color(tone),
            (None, None) => NSColor::labelColor(),
        };
        let background = NSColor::quaternaryLabelColor();
        let link = style.link.as_deref().and_then(|url| NSURL::URLWithString(&NSString::from_str(url)));

        let attrs = sl_macos::text::Attrs { font: Some(&font), color: Some(&color), background: style.code.then_some(&*background), link: link.as_deref(), ..Default::default() };
        let piece = sl_macos::text::attributed(&run.text, &attrs);
        out.appendAttributedString(&piece);
    }
    Retained::into_super(out)
}

// --------------------------------------------------------------------------
// Submenu construction
// --------------------------------------------------------------------------

/// A fresh submenu attached to `parent`.
///
/// The returned `Retained<NSMenu>` is only borrowed while rows are added:
/// `setSubmenu:` is a retaining property, so the menu item owns the submenu
/// and the items in it, and the [`Row`] handles kept in the ivars are extra
/// references to objects the menu already holds.
fn attach_submenu(mtm: MainThreadMarker, parent: &Row) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    parent.item.setSubmenu(Some(&menu));
    menu
}

// --------------------------------------------------------------------------
// Painting
// --------------------------------------------------------------------------

/// Paint the limit meters in `style`.
fn paint_limits(rows: &LimitRows, snapshot: &Snapshot, provider: Provider, style: MeterStyle, width: f64, now: i64) {
    let (slots, note) = limit_models(snapshot, provider, view_prefs::load_switch(view_prefs::Switch::ShowSpend), now);
    if rows.panel.paint(&slots, meter_dots(provider), style, width) {
        rows.row.show();
    } else {
        rows.row.hide();
    }
    match note {
        Some(note) => rows.updated.set(&note),
        None => rows.updated.hide(),
    }
}

// --------------------------------------------------------------------------
// Formatting helpers
// --------------------------------------------------------------------------

// --------------------------------------------------------------------------
// NSMenuItem construction
// --------------------------------------------------------------------------

/// A top-level row that owns a submenu. Enabled because AppKit will not open a
/// disabled item's submenu; it still has no action and no target, so a click
/// dispatches nothing and the unretained-target problem never arises.
fn section_row(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = sl_macos::system::menu_item(mtm, title);
    item.setEnabled(true);
    item
}

/// The AppKit colour for a [`Tone`]. System colours, not `sl_core`'s palette:
/// they resolve themselves for light and dark menus.
pub(crate) fn tone_color(tone: Tone) -> Retained<NSColor> {
    match tone {
        Tone::Plain => NSColor::labelColor(),
        Tone::Dim => dim_color(),
        Tone::Green => NSColor::systemGreenColor(),
        Tone::Yellow => NSColor::systemYellowColor(),
        Tone::Orange => NSColor::systemOrangeColor(),
        Tone::Red => NSColor::systemRedColor(),
    }
}

/// Secondary text: the label colour at 70%. The system's
/// `secondaryLabelColor` fades to near-unreadable on a menu's translucent
/// background; this stays clearly second to the label colour and still reads,
/// light or dark, since it is derived from the label colour itself.
pub(crate) fn dim_color() -> Retained<NSColor> {
    DIM.with(Clone::clone)
}

thread_local! {
    /// A dynamic colour, resolved against the appearance it is drawn in each
    /// time: the label colour with its alpha changed is not -- it freezes to
    /// whichever appearance was current when it was made, which leaves dim
    /// text white in a light menu.
    static DIM: Retained<NSColor> = sl_macos::draw::dynamic_color(|dark| if dark { NSColor::colorWithWhite_alpha(1.0, 0.7) } else { NSColor::colorWithWhite_alpha(0.0, 0.62) });
}

/// Coloured segments as one attributed menu title.
pub(crate) fn rich_title(segments: &[Seg]) -> Retained<NSAttributedString> {
    let title = NSMutableAttributedString::new();
    for seg in segments {
        title.appendAttributedString(&colored_title(&seg.text, &tone_color(seg.tone)));
    }
    Retained::into_super(title)
}

/// Coloured text segments (no icons) at `size`.
pub(crate) fn small_title(segments: &[Seg], size: f64) -> Retained<NSAttributedString> {
    let title = NSMutableAttributedString::new();
    let regular = sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular);
    for seg in segments {
        title.appendAttributedString(&styled(&seg.text, &tone_color(seg.tone), size, regular));
    }
    Retained::into_super(title)
}

/// `title` in the menu font and `color`.
fn colored_title(title: &str, color: &NSColor) -> Retained<NSAttributedString> {
    // `0.0` means "the standard size for this font role", i.e. whatever the
    // user's menus already use.
    let font = NSFont::menuFontOfSize(0.0);
    sl_macos::text::attributed(title, &sl_macos::text::Attrs { font: Some(&font), color: Some(color), ..Default::default() })
}

/// The menu bar icon's height, and where in it the glyph sits.
const BAR_HEIGHT: f64 = 16.0;
/// The glyph is drawn at 1/32 of `make-icon.swift`'s 1024-unit design space:
/// 624 x 416 units of prompt and meter become 19.5 x 13 points.
const GLYPH_SCALE: f64 = 32.0;
const GLYPH_WIDTH: f64 = 624.0 / GLYPH_SCALE;
const GLYPH_TOP: f64 = (BAR_HEIGHT - 416.0 / GLYPH_SCALE) / 2.0;
const BADGE_GAP: f64 = 2.5;
const BADGE_POINT_SIZE: f64 = 10.0;
/// Every badge [`bar_image`] can draw, for sizing the slot they share.
const BADGE_SYMBOLS: [&str; 3] = ["cup.and.saucer.fill", "bolt.fill", "moon.zzz.fill"];

/// How much of the icon's alpha is left while no session is open.
const BAR_DIMMED: f64 = 0.4;
/// The glyph's meter in design units: the track, and the fill's inset in it.
const METER_LEFT: f64 = 200.0;
const METER_WIDTH: f64 = 624.0;
const METER_INSET: f64 = 16.0;

/// Everything the menu bar icon is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarIcon {
    state: BarState,
    /// `None` until a limits capture has a 5-hour window: the meter is then an
    /// empty track, rather than a guess.
    meter: Option<BarMeter>,
    /// Whether the sessions' badge is drawn beside the glyph.
    badge: bool,
    dark: bool,
    /// Whose sessions and window it shows, for its spoken description.
    provider: Provider,
}

fn is_dark(appearance: &NSAppearance) -> bool {
    sl_macos::consts::is_dark(appearance)
}

/// A badge symbol at the menu bar size, in `color`; `None` if this macOS
/// lacks it.
fn badge_image(name: &str, color: &NSColor) -> Option<Retained<NSImage>> {
    symbol_image_sized(name, color, BADGE_POINT_SIZE, sl_macos::consts::font_weight(sl_macos::consts::Weight::Medium))
}

/// The menu bar icon: the app icon's glyph -- a prompt over a usage meter,
/// here the real 5-hour window -- and a badge for what the open sessions are
/// doing.
///
/// Not a template image, since the meter has a colour and a template keeps
/// only alpha. The rest is drawn in `labelColor`, which the drawing handler
/// resolves in the menu bar's own appearance each time the image is drawn, so
/// it still follows a light or dark menu bar. Dimmed by hand while no session
/// is open.
fn bar_image(icon: BarIcon) -> Retained<NSImage> {
    let state = icon.state;
    let (symbol, doing) = match state {
        BarState::Caffeinated => (Some("cup.and.saucer.fill"), "keeping the Mac awake"),
        BarState::Working => (Some("bolt.fill"), "working"),
        BarState::Idle => (Some("moon.zzz.fill"), "idle"),
        BarState::NoSessions => (None, "no open sessions"),
    };
    let who = match icon.provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
        Provider::All => "Claude Code and Codex",
    };
    let description = format!("{who}: {doing}");
    let alpha = if state == BarState::NoSessions { BAR_DIMMED } else { 1.0 };
    let badge = symbol.filter(|_| icon.badge).and_then(|name| badge_image(name, &NSColor::labelColor()));
    // One width for every state, badge or none: an icon that changed width as
    // sessions went busy and idle would shove every menu bar item to its left.
    let slot = if icon.badge {
        BADGE_SYMBOLS.into_iter().filter_map(|name| badge_image(name, &NSColor::labelColor())).map(|image| image.size().width).fold(0.0, f64::max)
    } else {
        0.0
    };
    let width = if icon.badge { GLYPH_WIDTH + BADGE_GAP + slot } else { GLYPH_WIDTH };
    let meter = icon.meter;
    let draw = block2::RcBlock::new(move |_bounds: NSRect| -> objc2::runtime::Bool {
        draw_glyph(alpha, meter);
        if let Some(badge) = &badge {
            let size = badge.size();
            badge.drawInRect_fromRect_operation_fraction(
                NSRect::new(NSPoint::new(GLYPH_WIDTH + BADGE_GAP + ((slot - size.width) / 2.0).round(), ((BAR_HEIGHT - size.height) / 2.0).round()), size),
                NSRect::ZERO,
                NSCompositingOperation::SourceOver,
                1.0,
            );
        }
        objc2::runtime::Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(width.ceil(), BAR_HEIGHT), false, &draw);
    let description = match meter {
        Some(meter) => format!("{description}; {}% of the limit used{}", meter.used, if meter.over_pace { ", ahead of pace" } else { "" }),
        None => description,
    };
    image.setAccessibilityDescription(Some(&NSString::from_str(&description)));
    image
}

/// For `--icon-preview`: the menu bar icon in four states -- working and on
/// pace, working and heading past the limit, idle with no capture, and no
/// session open -- on a light and a dark menu-bar-grey strip, at four times
/// size, as TIFFs at `<prefix>-light.tiff` and `<prefix>-dark.tiff`.
pub(crate) fn preview_icons(prefix: &str) -> bool {
    const SCALE: f64 = 4.0;
    let states = [
        (BarState::Working, Some(BarMeter { used: 40, over_pace: false })),
        (BarState::Working, Some(BarMeter { used: 60, over_pace: true })),
        (BarState::Idle, None),
        (BarState::NoSessions, Some(BarMeter { used: 25, over_pace: false })),
    ];
    let mut written = true;
    for (dark, which, background) in [(false, sl_macos::consts::Appearance::Light, 0.93), (true, sl_macos::consts::Appearance::Dark, 0.17)] {
        let Some(appearance) = sl_macos::consts::appearance(which) else { return false };
        let images: Vec<Retained<NSImage>> = states.iter().map(|(state, meter)| bar_image(BarIcon { state: *state, meter: *meter, badge: true, dark, provider: Provider::Claude })).collect();
        let slot = images.iter().map(|image| image.size().width).fold(0.0, f64::max) + 12.0;
        let size = NSSize::new(slot * images.len() as f64 * SCALE, (BAR_HEIGHT + 8.0) * SCALE);
        let draw = block2::RcBlock::new(move |bounds: NSRect| -> objc2::runtime::Bool {
            NSColor::colorWithWhite_alpha(background, 1.0).setFill();
            NSBezierPath::fillRect(bounds);
            for (i, image) in images.iter().enumerate() {
                let at = NSRect::new(NSPoint::new((i as f64 * slot + 6.0) * SCALE, 4.0 * SCALE), NSSize::new(image.size().width * SCALE, image.size().height * SCALE));
                image.drawInRect_fromRect_operation_fraction(at, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0);
            }
            objc2::runtime::Bool::YES
        });
        let strip = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
        let path = format!("{prefix}-{}.tiff", if dark { "dark" } else { "light" });
        let ok = Cell::new(false);
        let render = block2::RcBlock::new(|| {
            ok.set(strip.TIFFRepresentation().is_some_and(|tiff| tiff.writeToFile_atomically(&NSString::from_str(&path), true)));
        });
        appearance.performAsCurrentDrawingAppearance(&render);
        written &= ok.get();
    }
    written && preview_limits(&format!("{prefix}-limits")) && preview_compact(&format!("{prefix}-compact")) && preview_regular(&format!("{prefix}-regular")) && crate::settings_window::preview(&format!("{prefix}-settings.png"))
        && crate::closed_window::preview(&format!("{prefix}-closed-window.png"), &preview_closed(1_788_200_000_000), 1_788_200_000_000)
}

/// For `--icon-preview`: the menu's limits row from sample windows, in each
/// meter style, at the width a menu of open sessions gives it, in light mode,
/// as `<prefix>-<style>.png`.
fn preview_limits(prefix: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let now = 1_788_200_000_000;
    let session = preview_slot("Session", "5h", 17.0, predictions::SESSION_WINDOW_MS, 0.53, Some("$30.04 so far · $33.13/hr · $141.56 projected"), now);
    let weekly = preview_slot("Weekly", "7d", 41.0, predictions::WEEKLY_WINDOW_MS, 0.36, None, now);
    // All: Claude Code's two and Codex's weekly, as last seen an hour ago.
    let codex = MeterSlot { dimmed: true, ..preview_slot("Codex", "7d", 9.0, predictions::WEEKLY_WINDOW_MS, 0.12, None, now) };
    let claude = [session.clone(), weekly.clone()];
    let all = [session, weekly, codex];
    MeterStyle::ALL.into_iter().all(|style| {
        [("", &claude[..]), ("-all", &all[..])].into_iter().all(|(suffix, slots)| {
            let panel = LimitsPanel::new(mtm);
            // Light, so its dark text shows on the image's transparent background.
            panel.view.setAppearance(sl_macos::consts::appearance(sl_macos::consts::Appearance::Light).as_deref());
            // The menu stretches the row to its widest item.
            // Claude Code's page, the second dot the way to Codex's.
            panel.paint(slots, (suffix.is_empty()).then_some((2, 0)), style, 580.0);
            write_png(&panel.view, &format!("{prefix}{suffix}-{}.png", style.label().to_lowercase()))
        })
    })
}

/// A sample meter: `used`% of a window `fraction` of the way through.
fn preview_slot(name: &str, span: &str, used_percent: f64, window_ms: i64, fraction: f64, spend: Option<&str>, now: i64) -> MeterSlot {
    let window = LimitWindow { used_percent, resets_at_ms: Some(now + sl_core::num::trunc_i64(window_ms as f64 * (1.0 - fraction))), window_minutes: None };
    MeterSlot { name: name.into(), span: span.into(), model: window_model(&window, window_ms, spend.map(str::to_owned), now), dimmed: false }
}

/// A sample stack for the previews, shaped as stack #746 was.
pub(crate) fn preview_stack() -> StackView {
    use crate::stacks::{EntrySession as S, EntryState as E};
    let entry = |number: i64, branch: &str, state, session| StackEntry { number, branch: Some(branch.into()), state, session, checked: true };
    StackView {
        number: Some(746),
        repo_url: Some("https://github.com/o/r".into()),
        entries: vec![
            entry(751, "jakub/dn-529-m21-watch-substrate", E::Merged, S::None),
            entry(752, "jakub/dn-530-m22-m23-watch-as-a-trigger-type", E::Merged, S::Closed("M22 watch trigger".into())),
            entry(753, "jakub/dn-531-m24-fire-and-deliver", E::Open, S::Live("DN-531 / M4 #3".into())),
            entry(747, "jakub/dn-478-ontology-m3-engine", E::Draft, S::None),
            entry(770, "jakub/dn-478-m34-every-inferred", E::Draft, S::Live("DN-478 / M3.4 #3".into())),
        ],
    }
}

/// Sample closed sessions for the previews.
pub(crate) fn preview_closed(now: i64) -> Vec<Remembered> {
    let entry = |id: &str, name: &str, branch: &str, pr: Option<i64>, ago_min: i64| Remembered {
        session_id: id.into(),
        name: name.into(),
        project: "datanaut".into(),
        cwd: format!("/x/{id}"),
        branch: Some(branch.into()),
        pr_number: pr,
        base_pr: None,
        last_seen_ms: now - ago_min * 60_000,
        provider: Source::Claude,
    };
    vec![entry("c1", "DN-512 / M2 drafts", "jakub/dn-512-m2-drafts", Some(744), 50), entry("c2", "fix flaky ingest test", "jakub/fix-ingest-flake", None, 2 * 24 * 60)]
}

/// Sample sessions and meters for the previews: one of each state.
pub(crate) fn preview_samples(now: i64) -> (Vec<OpenSession>, Vec<MeterSlot>) {
    use sl_core::gh::{Assessment, Ci, CiState, FailedJob, GhInfo, Review, Score};
    let min = 60_000;
    let session = |id: &str, project: &str, busy: bool, branch: &str, left_min: Option<i64>, gh: Option<GhInfo>| OpenSession {
        session_id: id.into(),
        name: id.into(),
        project: project.into(),
        cwd: format!("/x/{id}"),
        activity: if busy { sessions::Activity::Busy } else { sessions::Activity::Idle },
        started_ms: 0,
        branch: Some(branch.into()),
        gh,
        cache: left_min.map(|left| crate::cache_ttl::CacheInfo { last_activity_ms: now - (60 - left) * min, ttl_ms: Some(60 * min), compacted: false }),
        remote_control: None,
        pid: None,
        // As the status line would report: some fuller than others.
        context: Some(sl_core::types::ContextCapture { percent: (id.len() as f64 * 7.0) % 95.0, used_tokens: None, limit_tokens: None, captured_at_ms: now }),
        linear: sl_core::branch::ticket_id(branch).map(|id| crate::linear::LinearInfo {
            url: format!("https://linear.app/x/issue/{id}"),
            state: ["In Progress", "In Review", "Todo"][id.len() % 3].into(),
            id,
        }),
        provider: Source::Claude,
        last_activity_ms: None,
    };
    let pr = |number: i64, draft: bool, ci: CiState| GhInfo {
        pr_number: Some(number),
        is_draft: Some(draft),
        ci: Some(Ci { state: ci, failed: Vec::new(), pending: 0 }),
        ..GhInfo::default()
    };
    let mut reviewed = pr(753, false, CiState::Passing);
    reviewed.base = Some("jakub/dn-530-m22-m23-watch-as-a-trigger-type-with-its-dependency-edges".into());
    reviewed.base_sha = Some("5a540c4a96".into());
    reviewed.base_pr = Some(752);
    reviewed.reviews = vec![Review { author: "claude".into(), state: sl_core::gh::ReviewState::Commented }];
    reviewed.assessment = Some(Assessment { author: "claude".into(), risk: Some(Score { value: 4, max: 5 }), quality: Some(Score { value: 4, max: 5 }) });
    let mut failing = pr(781, false, CiState::Failing);
    if let Some(ci) = failing.ci.as_mut() {
        ci.failed = vec![FailedJob { name: "test (ubuntu-latest)".into(), link: Some("https://github.com/o/r/actions/runs/1/job/2".into()) }];
    }
    let all = vec![
        session("ontology-supervisor", "datanaut", true, "jakub/dn-476-ontology-m1", None, None),
        session("live-email-slack-demo", "datanaut", false, "jakub/dn-479-ontology-m4-base", Some(55), None),
        session("DN-479 / M4", "datanaut", false, "jakub/dn-479-ontology-m4-base", Some(10), None),
        session("DN-540 / M5", "datanaut", false, "jakub/dn-540-m5", Some(54), Some(failing)),
        session("DN-531 / M4 #3", "datanaut", false, "jakub/dn-531-m24-fire-and-deliver", Some(-4000), Some(reviewed)),
        session("DN-478 / M3.4 #3", "datanaut", false, "jakub/dn-478-m34-every-inferred", Some(-60), Some(pr(770, true, CiState::Passing))),
        session("statusline-0b", "statusline", true, "feat/tray-stats-window", None, None),
        // A Codex thread: no prompt cache, idle since its last turn.
        OpenSession {
            provider: Source::Codex,
            cache: None,
            last_activity_ms: Some(now - 7 * min),
            ..session("Codex support", "statusline", false, "feat/tray-codex", None, None)
        },
    ];
    let meters = vec![
        preview_slot("Session", "5h", 7.0, predictions::SESSION_WINDOW_MS, 0.21, Some("$21.50 so far · $12.45/hr · $60.10 projected"), now),
        preview_slot("Weekly", "7d", 27.0, predictions::WEEKLY_WINDOW_MS, 0.51, None, now),
    ];
    (all, meters)
}

/// For `--icon-preview`: the Compact popover with sample sessions, as
/// `<prefix>-rings.png` (a reviewed PR unfolded), `<prefix>-bars.png` (a
/// failed CI run unfolded) and `<prefix>-stacks.png` (a stack unfolded, the
/// Closed group open) and `<prefix>-closed.png` (a closed session unfolded),
/// the last two tall enough not to scroll.
fn preview_compact(prefix: &str) -> bool {
    let now = 1_788_200_000_000;
    let (all, meters) = preview_samples(now);
    let stacks = vec![preview_stack()];
    let closed = preview_closed(now);
    let stack_key = format!("stack:{:?}:{:?}", stacks[0].number, stacks[0].entries.first().map(|e| e.number));
    [(MeterStyle::Rings, "rings", "DN-531 / M4 #3".to_owned(), false, 520.0), (MeterStyle::Bars, "bars", "DN-540 / M5".to_owned(), false, 520.0), (MeterStyle::Bars, "stacks", stack_key, true, 2000.0), (MeterStyle::Bars, "closed", "closed:c1".to_owned(), true, 2000.0), (MeterStyle::Bars, "tight", "DN-540 / M5".to_owned(), false, 520.0)]
        .into_iter()
        .all(|(style, name, expanded, closed_open, max_list_height)| {
            let input = CompactInput {
                sessions: &all,
                now,
                auto_awake: Some(sessions::Awake { auto: true, rc: true }),
                meter_slots: &meters,
                note: None,
                meters: style,
                footer: "Updated 29s ago · 433,498 records",
                width: 460.0,
                max_list_height,
                closed: &closed,
                stacks: &stacks,
                marks: true,
                meter_dots: Some((2, 0)),
                tuning: Tuning { density: if name == "tight" { view_prefs::Density::Tight } else { view_prefs::Density::Comfortable }, ..Tuning::default() },
            };
            crate::compact::preview(&format!("{prefix}-{name}.png"), &input, Some(&expanded), closed_open)
        })
}

/// For `--icon-preview`: the Regular menu with the sample sessions, dark, as
/// `<prefix>.png`: the menu's own rows (one highlighted, the Cold group
/// folded) and beside them four sessions' submenus -- reviewed, draft, failed
/// CI with its job's submenu, and no PR. A menu cannot be drawn offscreen, but
/// every row in it is a view that can.
fn preview_regular(prefix: &str) -> bool {
    const GAP: f64 = 24.0;
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let now = 1_788_200_000_000;
    let (all, meters) = preview_samples(now);
    let width = menu_rows::ROW_WIDTH;
    let canvas = menu_rows::Backdrop::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0)));
    canvas.setAutoresizesSubviews(false);
    canvas.setAppearance(sl_macos::consts::appearance(sl_macos::consts::Appearance::Dark).as_deref());

    // The menu.
    let menu_view = card(mtm);
    let mut y = 6.0;
    let stack = |parent: &NSView, view: &NSView, height: f64, y: &mut f64| {
        view.setFrame(NSRect::new(NSPoint::new(0.0, *y), NSSize::new(width, height)));
        parent.addSubview(view);
        *y += height;
    };
    let panel = LimitsPanel::new(mtm);
    panel.paint(&meters, Some((2, 0)), MeterStyle::Bars, width);
    let height = panel.view.frame().size.height;
    stack(&menu_view, &panel.view, height, &mut y);
    stack(&menu_view, &separator_line(mtm), 11.0, &mut y);
    let tuning = Tuning::default();
    let arranged = sessions::arrange(&all, None, now, &tuning);
    let mut highlighted = false;
    for (i, (session, group)) in arranged.iter().enumerate() {
        let first = i == 0 || arranged[i - 1].1 != *group;
        let members: Vec<&OpenSession> = arranged.iter().filter(|(_, g)| g == group).map(|(s, _)| s).collect();
        if first && *group == Urgency::Cold {
            let row = RowView::new(mtm);
            row.set(cold_row(&members, false));
            stack(&menu_view, &row, 44.0, &mut y);
            break;
        }
        if first {
            let caption = NSTextField::labelWithString(&NSString::new(), mtm);
            caption.setAttributedStringValue(&menu_rows::caption_text(group.label(), members.len(), *group == Urgency::NeedsYou));
            caption.sizeToFit();
            caption.setFrameOrigin(NSPoint::new(PANEL_INSET, y + 6.0));
            menu_view.addSubview(&caption);
            y += CaptionRow::HEIGHT;
        }
        let row = RowView::new(mtm);
        row.set(RowContent {
            icon: Some(sessions::state_glyph(session, now, session.activity == sessions::Activity::Busy)),
            ring: sessions::context_ring(session, &tuning),
            title: sessions::row_title(session),
            detail: sessions::row_detail(session, now),
            right: sessions::row_right(session, now, &tuning),
            mark: Some(session.provider),
            chevron: Some(false),
            tinted: false,
            dimmed: false,
        });
        if !highlighted && *group == Urgency::Idle {
            row.force_highlight();
            highlighted = true;
        }
        stack(&menu_view, &row, 44.0, &mut y);
    }
    // The Closed group, unfolded, under the open sessions.
    let closed = preview_closed(now);
    let fold = RowView::new(mtm);
    fold.set(closed_fold(&closed, true));
    stack(&menu_view, &fold, 44.0, &mut y);
    for entry in &closed {
        let row = RowView::new(mtm);
        row.set(closed_row(entry, true, now));
        stack(&menu_view, &row, 44.0, &mut y);
    }
    // The Stacks group: one stack.
    let sample_stack = preview_stack();
    let caption = NSTextField::labelWithString(&NSString::new(), mtm);
    caption.setAttributedStringValue(&menu_rows::caption_text("Stacks", 1, false));
    caption.sizeToFit();
    caption.setFrameOrigin(NSPoint::new(PANEL_INSET, y + 6.0));
    menu_view.addSubview(&caption);
    y += CaptionRow::HEIGHT;
    let row = RowView::new(mtm);
    row.set(stack_row(&sample_stack));
    stack(&menu_view, &row, 44.0, &mut y);
    stack(&menu_view, &separator_line(mtm), 11.0, &mut y);
    let footer = FooterRow::new(mtm, &NSMenu::new(mtm));
    footer.set("Updated 29s ago · 433,498 records", Some(true));
    // Out of its menu item, which would otherwise keep it at the origin.
    footer.row.item.setView(None);
    stack(&menu_view, &footer.view, FOOTER_HEIGHT, &mut y);
    menu_view.setFrame(NSRect::new(NSPoint::new(GAP, GAP), NSSize::new(width, y + 6.0)));
    canvas.addSubview(&menu_view);

    // The submenus, from real `SessionMenu`s painted with the samples.
    let throwaway = NSMenu::new(mtm);
    let submenu_of = |name: &str| {
        let session = all.iter().find(|s| s.name == name)?;
        let menu = SessionMenu::new(mtm, &throwaway);
        menu.paint(session, false, true, now, &Tuning::default());
        let job = menu.jobs.first().and_then(|job| job.row.item.submenu());
        Some((menu.row.item.submenu()?, job))
    };
    let mut x = GAP + width + GAP;
    let mut bottom = y + 6.0 + GAP;
    for column in [["DN-531 / M4 #3", "DN-478 / M3.4 #3"], ["DN-540 / M5", "live-email-slack-demo"]] {
        let mut top = GAP;
        let mut column_width: f64 = 0.0;
        for name in column {
            let Some((sub, job)) = submenu_of(name) else { continue };
            let view = submenu_column(mtm, &sub, menu_rows::SUBMENU_WIDTH, name == "DN-478 / M3.4 #3");
            view.setFrameOrigin(NSPoint::new(x, top));
            canvas.addSubview(&view);
            let size = view.frame().size;
            column_width = column_width.max(size.width);
            if let Some(job) = job.filter(|_| name == "DN-540 / M5") {
                let jobs = submenu_column(mtm, &job, 240.0, false);
                jobs.setFrameOrigin(NSPoint::new(x + size.width + 4.0, top + 90.0));
                canvas.addSubview(&jobs);
                column_width = column_width.max(size.width + 4.0 + jobs.frame().size.width);
            }
            top += size.height + GAP;
        }
        bottom = bottom.max(top);
        x += column_width + GAP;
    }
    // The stack's submenu, in a column of its own.
    let stack_menu = StackMenu::new(mtm, &throwaway);
    stack_menu.paint(&sample_stack);
    if let Some(sub) = stack_menu.row.item.submenu() {
        let view = submenu_column(mtm, &sub, menu_rows::SUBMENU_WIDTH, false);
        view.setFrameOrigin(NSPoint::new(x, GAP));
        canvas.addSubview(&view);
        bottom = bottom.max(GAP + view.frame().size.height + GAP);
        x += view.frame().size.width + GAP;
    }
    canvas.setFrameSize(NSSize::new(x, bottom));
    write_png(&canvas, &format!("{prefix}.png"))
}

/// A menu-coloured card for the preview to stack rows on.
fn card(mtm: MainThreadMarker) -> crate::compact::TintView {
    let view = crate::compact::TintView::new(mtm, Tone::Plain);
    view.ivars().strength.set(0.16);
    view.ivars().radius.set(10.0);
    view.setAutoresizesSubviews(false);
    view
}

/// A menu separator, for the preview.
fn separator_line(mtm: MainThreadMarker) -> Retained<NSView> {
    let holder = crate::meters::FlippedView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 11.0)));
    let line = crate::compact::TintView::new(mtm, Tone::Plain);
    line.ivars().strength.set(0.35);
    line.ivars().radius.set(0.0);
    line.setFrame(NSRect::new(NSPoint::new(14.0, 5.0), NSSize::new(menu_rows::ROW_WIDTH - 28.0, 1.0)));
    holder.addSubview(&line);
    holder.retained_view()
}

/// A submenu laid out as the menu lays it out, from its items' own views:
/// hidden items skipped, separators drawn. `highlight_zed` lights its "Open
/// in Zed" row, to show a highlighted action.
fn submenu_column(mtm: MainThreadMarker, menu: &NSMenu, width: f64, highlight_zed: bool) -> crate::compact::TintView {
    let column = card(mtm);
    let mut y = 5.0;
    // As wide as its widest row, as a menu is.
    let width = menu.itemArray().to_vec().iter().filter_map(|item| item.view()).map(|view| view.frame().size.width).fold(width, f64::max);
    for item in menu.itemArray().to_vec() {
        if item.isHidden() {
            continue;
        }
        if item.isSeparatorItem() {
            let line = separator_line(mtm);
            line.setFrame(NSRect::new(NSPoint::new(0.0, y), NSSize::new(width, 11.0)));
            line.setAutoresizesSubviews(false);
            if let Some(inner) = line.subviews().to_vec().first() {
                inner.setFrameSize(NSSize::new(width - 28.0, 1.0));
            }
            column.addSubview(&line);
            y += 11.0;
            continue;
        }
        let Some(view) = item.view() else { continue };
        item.setView(None);
        let height = view.frame().size.height;
        view.setFrame(NSRect::new(NSPoint::new(0.0, y), NSSize::new(width, height)));
        if highlight_zed
            && let Some(action) = ActionView::from_view(&view)
            && action.is_titled("Open worktree in Zed")
        {
            action.force_highlight();
        }
        column.addSubview(&view);
        y += height;
    }
    column.setFrameSize(NSSize::new(width, y + 5.0));
    column
}

/// Render `view` as it stands to a PNG at `path`.
pub(crate) fn write_png(view: &NSView, path: &str) -> bool {
    let bounds = view.bounds();
    let Some(bitmap) = view.bitmapImageRepForCachingDisplayInRect(bounds) else { return false };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
    let png = sl_macos::draw::png(&bitmap);
    png.is_some_and(|png| png.writeToFile_atomically(&NSString::from_str(path), true))
}

/// The glyph from `packaging/make-icon.swift`, without its tile, mapped from
/// that file's y-down design space into this y-up canvas, in `labelColor` at
/// `alpha`. The meter's track is a faint one, and its fill is `meter`'s usage,
/// green or yellow; with no `meter` the track is left empty.
fn draw_glyph(alpha: f64, meter: Option<BarMeter>) {
    let x = |design: f64| (design - 200.0) / GLYPH_SCALE;
    let y = |design: f64| BAR_HEIGHT - GLYPH_TOP - (design - 320.0) / GLYPH_SCALE;
    let ink = NSColor::labelColor();
    let rounded = |left: f64, top: f64, width: f64, height: f64, radius: f64, color: &NSColor| {
        color.setFill();
        let rect = NSRect::new(NSPoint::new(x(left), y(top + height)), NSSize::new(width / GLYPH_SCALE, height / GLYPH_SCALE));
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius / GLYPH_SCALE, radius / GLYPH_SCALE).fill();
    };

    // The prompt: a chevron and a cursor.
    let chevron = NSBezierPath::bezierPath();
    chevron.moveToPoint(NSPoint::new(x(290.0), y(320.0)));
    chevron.lineToPoint(NSPoint::new(x(435.0), y(408.0)));
    chevron.lineToPoint(NSPoint::new(x(290.0), y(496.0)));
    chevron.setLineWidth(64.0 / GLYPH_SCALE);
    chevron.setLineCapStyle(NSLineCapStyle::Round);
    chevron.setLineJoinStyle(NSLineJoinStyle::Round);
    ink.colorWithAlphaComponent(alpha).setStroke();
    chevron.stroke();
    rounded(490.0, 440.0, 240.0, 64.0, 32.0, &ink.colorWithAlphaComponent(alpha));

    // The meter: a track, filled as far as the session window is used. A
    // sliver narrower than its own height is drawn at that height, so 1%
    // still reads as a dot rather than vanishing.
    rounded(METER_LEFT, 640.0, METER_WIDTH, 96.0, 48.0, &ink.colorWithAlphaComponent(0.3 * alpha));
    if let Some(meter) = meter.filter(|meter| meter.used > 0) {
        let span = METER_WIDTH - 2.0 * METER_INSET;
        let fill = (span * f64::from(meter.used) / 100.0).max(64.0);
        let color = if meter.over_pace { NSColor::systemYellowColor() } else { NSColor::systemGreenColor() };
        rounded(METER_LEFT + METER_INSET, 656.0, fill, 64.0, 32.0, &color.colorWithAlphaComponent(alpha));
    }
}

/// `text` in the system font at `size` and `weight`, with monospaced digits so
/// a meter's numbers do not jitter as they tick.
pub(crate) fn styled(text: &str, color: &NSColor, size: f64, weight: NSFontWeight) -> Retained<NSAttributedString> {
    let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, weight);
    sl_macos::text::attributed(text, &sl_macos::text::Attrs { font: Some(&font), color: Some(color), ..Default::default() })
}
