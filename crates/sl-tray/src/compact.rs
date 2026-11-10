//! The Compact layout: a popover instead of a menu.
//!
//! ```text
//! ( 7% ) Session 5h          ( 27% ) Weekly 7d
//! $21.50 so far · $12.45/hr · $60.10 projected
//! [ All 8 | Needs you 2 | Working 2 | Cold 3 ]
//! datanaut
//! ┌ ⚡ ontology-supervisor                 working ┐
//! ┌ ◔ DN-479 / M4                         10m left ┐
//! ┌ ❄ DN-531 / M4 #3  #753                    cold ┐
//! │   jakub/dn-531-m24-fire-and-deliver            │
//! │   #753 [Ready] [CI ✓] [Risk 4/5] [Quality 4/5] │
//! │   → jakub/dn-530-m22-…-dependency-edges        │
//! │   [Zed] [PR] [Copy review] [Copy path]           │
//! └────────────────────────────────────────────────┘
//! Updated 29s ago · 433 498 records   ☕ ⚙ Stats Quit
//! ```
//!
//! Each session is a one-line card; clicking one unfolds it in place to show
//! its PR and the actions on it, which the Regular layout keeps in submenus.
//!
//! Unlike the menu, a popover may change its views while it is open, so the
//! cards are rebuilt -- but only when what they show changes, never on a tick
//! that changed nothing, so a button is not swapped out from under the
//! pointer every second.

use std::cell::{Cell, RefCell};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearanceCustomization, NSApplication, NSBezelStyle, NSBezierPath, NSButton,
    NSCellImagePosition, NSColor, NSControl, NSControlSize, NSEvent, NSEventMask, NSFont, NSImage,
    NSLineBreakMode, NSPopover, NSPopoverBehavior, NSScrollView, NSSegmentDistribution,
    NSSegmentedControl, NSStatusBarButton, NSTextField, NSView, NSViewController,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSRectEdge, NSSize, NSString};

use crate::act::{Act, Target};
use crate::controller::{rich_title, tone_color};
use crate::meters::{
    DETAIL_FONT_SIZE, FlippedView, LimitsPanel, MeterSlot, PANEL_INSET, PANEL_RIGHT,
};
use crate::sessions::{self, Activity, MAX_JOBS, OpenSession, Seg, Tone, Urgency};
use crate::stacks::{EntrySession, StackView};
use crate::sticky::Remembered;
use crate::tray_model::{Pill, PillsFor, pill};
use crate::view_prefs::{ColdMode, Density, Layout, MeterStyle, SortOrder, Source, Tuning};
use sl_macos::canvas::{Canvas, CanvasBehavior, ControlCanvas, Hit};

/// The popover's width.
const WIDTH: f64 = 460.0;
/// Card insets: the cards from the popover's edges, the text from the card's.
const CARD_MARGIN: f64 = 12.0;
const CARD_PAD: f64 = 10.0;

/// The room a card takes, by the density Settings picks. The comfortable
/// numbers are what the popover has always used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Metrics {
    /// A collapsed card's height.
    pub header: f64,
    /// Between cards.
    pub gap: f64,
}

impl Metrics {
    pub(crate) fn of(density: Density) -> Self {
        match density {
            Density::Comfortable => Self {
                header: 34.0,
                gap: 4.0,
            },
            Density::Tight => Self {
                header: 28.0,
                gap: 2.0,
            },
        }
    }
}

/// Where an unfolded card's details start: under the title, past the icon.
/// The card's one text column: the title and every line under it start
/// here, after the status icon.
const DETAIL_X: f64 = CARD_PAD + crate::menu_rows::STATUS_SIZE + 8.0;
/// A text field draws its text this far inside its frame.
const LABEL_PAD: f64 = crate::meters::LABEL_PAD;
const PILL_HEIGHT: f64 = 18.0;
const PILL_FONT: f64 = 11.0;
const ACTION_HEIGHT: f64 = 32.0;
/// The box every action button's icon is drawn centred in, so each title
/// sits at the same height whatever its icon's shape.
const ACTION_ICON_BOX: f64 = 12.0;
/// An SF Symbol's size in that box; the Zed logo fills a little less of it,
/// as a symbol's glyph does.
const ACTION_SYMBOL_POINTS: f64 = 10.0;
const ACTION_LOGO_SIDE: f64 = 9.0;
const FOOTER_HEIGHT: f64 = 34.0;
/// A click on the menu bar icon this soon after the popover closed is the
/// click that closed it.
const REOPEN_GUARD: std::time::Duration = std::time::Duration::from_millis(300);

/// Which sessions the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Only(Urgency),
}

impl Filter {
    /// The filter's segments, in order. Idle sessions show under All only:
    /// the other three are the ones worth singling out.
    const SEGMENTS: [Filter; 4] = [
        Filter::All,
        Filter::Only(Urgency::NeedsYou),
        Filter::Only(Urgency::Working),
        Filter::Only(Urgency::Cold),
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Only(group) => group.label(),
        }
    }

    fn admits(self, group: Urgency) -> bool {
        match self {
            Filter::All => true,
            Filter::Only(only) => only == group,
        }
    }
}

/// Everything one paint of the popover needs from the controller.
pub(crate) struct CompactInput<'a> {
    pub sessions: &'a [OpenSession],
    pub now: i64,
    /// `Some` while the tray may hold the Mac awake: its switches, the
    /// "Auto caffeinate" one shown in the footer. `None` under
    /// `SL_TRAY_NO_CAFFEINATE`.
    pub auto_awake: Option<sessions::Awake>,
    /// The meters to show, as the provider picks them.
    pub meter_slots: &'a [MeterSlot],
    /// A line about the limits capture itself (none yet, stale), if any.
    pub note: Option<&'a str>,
    pub meters: MeterStyle,
    pub footer: &'a str,
    /// The popover's width, fixed for the screen so it never jumps.
    pub width: f64,
    /// How tall the card list may grow before it scrolls: what the screen
    /// leaves once the meters, filter and footer are placed.
    pub max_list_height: f64,
    /// Sessions no longer running, shown folded under All.
    pub closed: &'a [Remembered],
    /// The PR stacks the sessions are on, shown under All.
    pub stacks: &'a [StackView],
    /// Mark each card with whose session it is: Claude Code's and Codex's
    /// are listed together.
    pub marks: bool,
    /// The meters' page dots, `(pages, current)`; `None` when Codex is not
    /// installed.
    pub meter_dots: Option<(usize, usize)>,
    /// How the sessions are tuned in Settings.
    pub tuning: Tuning,
}

/// Everything the card list is built from: an equal one builds the same list.
#[derive(PartialEq)]
struct ListKey {
    width: f64,
    metrics: Metrics,
    more: usize,
    filter: Filter,
    cards: Vec<CardModel>,
    stacks: Vec<StackView>,
    /// The Closed card's count.
    closed_count: usize,
    /// The closed sessions with cards of their own.
    closed: Vec<Remembered>,
    /// Closed cards say how long ago, so they change with the minute.
    closed_ages: Vec<String>,
    closed_open: bool,
    expanded: Option<String>,
}

struct State {
    /// The Closed group unfolded.
    closed_open: bool,
    /// The card that is unfolded: a session's id, or a stack's or closed
    /// session's [`stack_key`] / [`closed_key`].
    expanded: Option<String>,
    filter: Filter,
    /// What the list was last built from; rebuilt only when it changes.
    key: Option<ListKey>,
    /// The global mouse monitor that closes the popover on a click elsewhere.
    monitor: Option<sl_macos::event::GlobalMonitor>,
    /// When the popover last closed, for telling a click on the menu bar icon
    /// that closed it from one that should open it (see `toggle`).
    closed_at: Option<std::time::Instant>,
}

pub(crate) struct CompactPopover {
    /// Holds its content controller, and through it the views below.
    popover: Retained<NSPopover>,
    root: FlippedView,
    limits: LimitsPanel,
    note: Retained<NSTextField>,
    filter: Retained<NSSegmentedControl>,
    scroll: Retained<NSScrollView>,
    list: FlippedView,
    footer: Retained<NSTextField>,
    auto: crate::menu_rows::ToggleButton,
    settings: Retained<NSButton>,
    stats: Retained<NSButton>,
    quit: Retained<NSButton>,
    state: RefCell<State>,
}

impl CompactPopover {
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let root = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 400.0)),
        );
        let limits = LimitsPanel::new(mtm);
        limits
            .view
            .setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::empty());
        root.addSubview(&limits.view);

        let note = label(mtm, "", DETAIL_FONT_SIZE, &crate::controller::dim_color());
        root.addSubview(&note);

        let labels: Vec<&str> = Filter::SEGMENTS.iter().map(|f| f.label()).collect();
        let filter = sl_macos::controls::segmented(mtm, &labels);
        crate::act::bind(&filter, Act::FilterSessions);
        filter.setSegmentDistribution(NSSegmentDistribution::Fill);
        filter.setSelectedSegment(0);
        root.addSubview(&filter);

        let list = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 100.0)),
        );
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 100.0)),
        );
        scroll.setDrawsBackground(false);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDocumentView(Some(&list));
        root.addSubview(&scroll);

        let footer = label(mtm, "", DETAIL_FONT_SIZE, &crate::controller::dim_color());
        root.addSubview(&footer);
        let auto = crate::controller::auto_button(mtm);
        let settings = crate::controller::settings_button(mtm);
        let stats = crate::controller::stats_button(mtm);
        let quit = crate::controller::quit_button(mtm);
        root.addSubview(&auto);
        for button in [&settings, &stats, &quit] {
            root.addSubview(button);
        }

        let controller = NSViewController::new(mtm);
        controller.setView(&root);
        let popover = NSPopover::new(mtm);
        popover.setContentViewController(Some(&controller));
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(false);

        Self {
            popover,
            root,
            limits,
            note,
            filter,
            scroll,
            list,
            footer,
            auto,
            settings,
            stats,
            quit,
            state: RefCell::new(State {
                closed_open: false,
                expanded: None,
                filter: Filter::All,
                key: None,
                monitor: None,
                closed_at: None,
            }),
        }
    }

    /// The popover's content view.
    pub(crate) fn root(&self) -> Retained<NSView> {
        self.root.retained_view()
    }

    pub(crate) fn is_shown(&self) -> bool {
        self.popover.isShown()
    }

    /// Show the popover under the status item's button, or close it if it is
    /// open.
    pub(crate) fn toggle(&self, button: &NSStatusBarButton) {
        if self.popover.isShown() {
            self.close();
            return;
        }
        // A transient popover closes on the mouse-down of a click on the menu
        // bar icon, and the icon's action follows on the mouse-up: without
        // this, that click would close the popover and open it again.
        if self
            .state
            .borrow_mut()
            .closed_at
            .take()
            .is_some_and(|at| at.elapsed() < REOPEN_GUARD)
        {
            return;
        }
        let mtm = MainThreadMarker::from(button);
        // An accessory app is not active until asked, and a transient popover
        // of an inactive app does not close on a click elsewhere.
        NSApplication::sharedApplication(mtm).activate();
        self.popover.showRelativeToRect_ofView_preferredEdge(
            button.bounds(),
            button,
            NSRectEdge::MinY,
        );
        // Clicks in other apps never reach this one; watch for them, so the
        // popover closes like a menu does.
        let popover = self.popover.clone();
        let monitor = sl_macos::event::GlobalMonitor::new(
            mtm,
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
            move |_event| {
                if popover.isShown() {
                    popover.close();
                }
            },
        );
        // Replacing it removes the old one.
        self.state.borrow_mut().monitor = monitor;
    }

    /// The popover has closed, however it happened: stop watching for clicks
    /// elsewhere, and note when.
    pub(crate) fn did_close(&self) {
        let mut state = self.state.borrow_mut();
        state.closed_at = Some(std::time::Instant::now());
        // Dropping it removes it.
        state.monitor = None;
    }

    /// Call `did_close` whenever the popover closes, by any route.
    pub(crate) fn on_close(&self, did_close: impl Fn() + 'static) {
        sl_macos::delegate::set_popover_delegate(&self.popover, did_close);
    }

    pub(crate) fn close(&self) {
        if self.popover.isShown() {
            self.popover.close();
        }
        self.state.borrow_mut().monitor = None;
    }

    /// Fold or unfold the card `target` names, folding any other: a
    /// session's, a stack's or a closed session's.
    pub(crate) fn toggle_card(&self, target: &Target) {
        let key = match target {
            Target::Session(id) => id.clone(),
            Target::Stack(key) => key.clone(),
            Target::Closed(id) => closed_key(id),
            _ => return,
        };
        let mut state = self.state.borrow_mut();
        state.expanded = if state.expanded.as_deref() == Some(key.as_str()) {
            None
        } else {
            Some(key)
        };
    }

    /// Fold or unfold the Closed group.
    pub(crate) fn toggle_closed_group(&self) {
        let mut state = self.state.borrow_mut();
        state.closed_open = !state.closed_open;
    }

    /// Show the filter at `segment`.
    pub(crate) fn set_filter(&self, segment: isize) {
        if let Some(filter) = usize::try_from(segment)
            .ok()
            .and_then(|i| Filter::SEGMENTS.get(i))
        {
            self.state.borrow_mut().filter = *filter;
        }
    }

    /// Paint everything from `input`, rebuilding the cards only if what they
    /// show has changed.
    pub(crate) fn update(&self, input: &CompactInput) {
        let mtm = MainThreadMarker::from(&**self.root.view());
        let mut y = 8.0;

        let width = input.width;
        if self
            .limits
            .paint(input.meter_slots, input.meter_dots, input.meters, width)
        {
            self.limits.view.setHidden(false);
            let size = self.limits.view.frame().size;
            self.limits.view.setFrame(NSRect::new(
                NSPoint::new(0.0, y),
                NSSize::new(width, size.height),
            ));
            let height = size.height;
            y += height + 2.0;
        } else {
            self.limits.view.setHidden(true);
        }
        match input.note {
            Some(note) => {
                self.note.setStringValue(&NSString::from_str(note));
                self.note.sizeToFit();
                self.note.setFrameOrigin(NSPoint::new(PANEL_INSET, y));
                self.note.setHidden(false);
                y += self.note.frame().size.height + 6.0;
            }
            None => self.note.setHidden(true),
        }

        let groups: Vec<Urgency> = input
            .sessions
            .iter()
            .map(|s| sessions::urgency(s, input.now, &input.tuning))
            .collect();
        for (i, filter) in Filter::SEGMENTS.iter().enumerate() {
            // All counts what it lists: Cold folded or hidden is not in it.
            let cold_left_out =
                *filter == Filter::All && input.tuning.cold_mode(Layout::Compact) != ColdMode::Show;
            let count = groups
                .iter()
                .filter(|g| filter.admits(**g) && !(cold_left_out && **g == Urgency::Cold))
                .count();
            let label = format!("{} {count}", filter.label());
            let segment = isize::try_from(i).unwrap_or(isize::MAX);
            // A new label lays the control out again; most repaints change no count.
            if self
                .filter
                .labelForSegment(segment)
                .is_none_or(|shown| shown.to_string() != label)
            {
                self.filter
                    .setLabel_forSegment(&NSString::from_str(&label), segment);
            }
        }
        self.filter.setFrame(NSRect::new(
            NSPoint::new(CARD_MARGIN, y),
            NSSize::new(width - 2.0 * CARD_MARGIN, 24.0),
        ));
        y += 32.0;

        let list_height = self.rebuild_list(mtm, input, &groups, width);
        let shown_height = list_height.min(input.max_list_height);
        self.scroll.setFrame(NSRect::new(
            NSPoint::new(0.0, y),
            NSSize::new(width, shown_height),
        ));
        y += shown_height + 4.0;

        self.footer
            .setStringValue(&NSString::from_str(input.footer));
        self.footer.sizeToFit();
        let footer_y = y + ((FOOTER_HEIGHT - self.footer.frame().size.height) / 2.0).round();
        self.footer
            .setFrameOrigin(NSPoint::new(PANEL_INSET, footer_y));
        let mut right = width - PANEL_RIGHT;
        // Shown or hidden before the layout, so a hidden one takes no room.
        match input.auto_awake {
            Some(awake) => {
                self.auto.setHidden(false);
                self.auto.set_on(awake.auto);
            }
            None => self.auto.setHidden(true),
        }
        let auto: &NSView = &self.auto;
        for button in [&*self.quit, &*self.stats, &*self.settings]
            .into_iter()
            .map(|b| -> &NSView { b })
            .chain([auto])
        {
            if button.isHidden() {
                continue;
            }
            let size = button.frame().size;
            right -= size.width;
            button.setFrameOrigin(NSPoint::new(
                right,
                y + ((FOOTER_HEIGHT - size.height) / 2.0).round(),
            ));
            right -= 4.0;
        }
        y += FOOTER_HEIGHT;

        let size = NSSize::new(width, y.ceil());
        if self.root.frame().size != size {
            self.root.setFrameSize(size);
            self.popover.setContentSize(size);
        }
    }

    /// A section's heading at `y` -- a project, Stacks, Closed -- spaced from
    /// whatever is above it. Returns the height it takes.
    fn heading(&self, mtm: MainThreadMarker, text: &str, y: f64) -> f64 {
        let first = y == 0.0;
        let heading = label(mtm, text, 11.0, &crate::controller::dim_color());
        heading.setFont(Some(&NSFont::systemFontOfSize_weight(
            11.0,
            sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold),
        )));
        heading.sizeToFit();
        heading.setFrameOrigin(NSPoint::new(
            CARD_MARGIN + 2.0,
            y + if first { 0.0 } else { 6.0 },
        ));
        self.list.addSubview(&heading);
        heading.frame().size.height + if first { 2.0 } else { 8.0 }
    }

    /// Rebuild the cards if what they show has changed. Returns the list's
    /// height.
    fn rebuild_list(
        &self,
        mtm: MainThreadMarker,
        input: &CompactInput,
        groups: &[Urgency],
        list_width: f64,
    ) -> f64 {
        let (filter, expanded, closed_open) = {
            let state = self.state.borrow();
            (state.filter, state.expanded.clone(), state.closed_open)
        };
        // Stacks and closed sessions are not sessions to filter: All only.
        let (stacks, closed): (&[StackView], &[Remembered]) = if filter == Filter::All {
            (input.stacks, input.closed)
        } else {
            (&[], &[])
        };
        // Unfolded, the newest few have cards, as in the menu; folded, none.
        let closed_cards = if closed_open {
            closed
                .get(..crate::controller::MAX_CLOSED)
                .unwrap_or(closed)
        } else {
            &[]
        };
        // Cold is left out of All unless it is to be listed; the Cold filter
        // always lists it.
        let cold_listed = input.tuning.cold_mode(Layout::Compact) == ColdMode::Show;
        let mut listed: Vec<(&OpenSession, Urgency)> = input
            .sessions
            .iter()
            .zip(groups)
            .map(|(session, group)| (session, *group))
            .filter(|(_, group)| {
                filter.admits(*group)
                    && (cold_listed || *group != Urgency::Cold || filter != Filter::All)
            })
            .collect();
        // Projects keep their place; within one, the sessions in the chosen
        // order (stable, so `Project` leaves them as they arrive).
        match input.tuning.sort {
            SortOrder::Project => {}
            SortOrder::Recent => listed.sort_by_key(|(s, _)| {
                (
                    s.project.clone(),
                    std::cmp::Reverse(sessions::last_touched(s)),
                )
            }),
            SortOrder::Started => {
                listed.sort_by_key(|(s, _)| (s.project.clone(), std::cmp::Reverse(s.started_ms)));
            }
        }
        let more = listed.len().saturating_sub(input.tuning.max_sessions);
        listed.truncate(input.tuning.max_sessions);
        let cards: Vec<CardModel> = listed
            .into_iter()
            .map(|(session, group)| {
                let holds_awake = input.auto_awake.is_some_and(|awake| awake.holds(session));
                let mut card = card_model(
                    session,
                    group,
                    holds_awake,
                    expanded.as_deref() == Some(session.session_id.as_str()),
                    input.now,
                    &input.tuning,
                );
                card.mark = input.marks.then_some(session.provider);
                card
            })
            .collect();
        let metrics = Metrics::of(input.tuning.density);
        let key = ListKey {
            width: list_width,
            metrics,
            more,
            filter,
            cards: cards.clone(),
            stacks: stacks.to_vec(),
            closed_count: closed.len(),
            closed: closed_cards.to_vec(),
            closed_ages: closed_cards
                .iter()
                .map(|c| crate::sticky::closed_ago(c.last_seen_ms, input.now))
                .collect(),
            closed_open,
            expanded: expanded.clone(),
        };
        if self.state.borrow().key.as_ref() == Some(&key) {
            return self.list.frame().size.height;
        }

        for view in self.list.subviews().to_vec() {
            view.removeFromSuperview();
        }
        let width = list_width - 2.0 * CARD_MARGIN;
        let mut y = 0.0;
        let mut project: Option<&str> = None;
        for card in &cards {
            if project != Some(card.session.project.as_str()) {
                project = Some(card.session.project.as_str());
                y += self.heading(mtm, &card.session.project, y);
            }
            let view = build_card(mtm, metrics, card, width);
            view.setFrameOrigin(NSPoint::new(CARD_MARGIN, y));
            self.list.addSubview(&view);
            y += view.frame().size.height + metrics.gap;
        }
        if more > 0 {
            let note = label(
                mtm,
                &format!("+{more} more"),
                DETAIL_FONT_SIZE,
                &crate::controller::dim_color(),
            );
            note.setFrameOrigin(NSPoint::new(CARD_MARGIN + 2.0, y + 2.0));
            self.list.addSubview(&note);
            y += note.frame().size.height + 6.0;
        }
        if cards.is_empty() {
            let text = if input.sessions.is_empty() {
                "No open sessions"
            } else {
                "None here"
            };
            let empty = label(mtm, text, DETAIL_FONT_SIZE, &crate::controller::dim_color());
            empty.setFrameOrigin(NSPoint::new(CARD_MARGIN + 2.0, 4.0));
            self.list.addSubview(&empty);
            y = empty.frame().size.height + 8.0;
        }
        if !stacks.is_empty() {
            y += self.heading(mtm, "Stacks", y);
            for stack in stacks {
                let view = build_stack_card(
                    mtm,
                    metrics,
                    stack,
                    expanded.as_deref() == Some(stack.key().as_str()),
                    width,
                );
                view.setFrameOrigin(NSPoint::new(CARD_MARGIN, y));
                self.list.addSubview(&view);
                y += view.frame().size.height + metrics.gap;
            }
        }
        if !closed.is_empty() {
            // No heading: the Closed card says what it is.
            y += if y == 0.0 { 0.0 } else { 8.0 };
            let fold = build_closed_fold(mtm, metrics, closed, closed_open, width);
            fold.setFrameOrigin(NSPoint::new(CARD_MARGIN, y));
            self.list.addSubview(&fold);
            y += fold.frame().size.height + metrics.gap;
            for entry in closed_cards {
                let mark = input.marks.then_some(entry.provider);
                let view = build_closed_card(
                    mtm,
                    metrics,
                    entry,
                    expanded.as_deref() == Some(closed_key(&entry.session_id).as_str()),
                    mark,
                    input.now,
                    width,
                );
                view.setFrameOrigin(NSPoint::new(CARD_MARGIN, y));
                self.list.addSubview(&view);
                y += view.frame().size.height + metrics.gap;
            }
            if closed_open {
                let all = small_button(
                    mtm,
                    "All closed sessions…",
                    Act::ShowClosedWindow,
                    Target::None,
                );
                all.setFrameOrigin(NSPoint::new(CARD_MARGIN, y + 2.0));
                self.list.addSubview(&all);
                y += all.frame().size.height + 6.0;
            }
        }
        self.list.setFrameSize(NSSize::new(list_width, y));

        self.state.borrow_mut().key = Some(key);
        y
    }
}

// --------------------------------------------------------------------------
// Cards
// --------------------------------------------------------------------------

/// Everything a card shows, so an unchanged card can be told apart from a
/// changed one without touching AppKit.
#[derive(Debug, Clone, PartialEq)]
struct CardModel {
    session: OpenSession,
    icon: sessions::Icon,
    ring: Option<(u8, Tone)>,
    title: Vec<Seg>,
    pills: Vec<Pill>,
    tint: Tone,
    expanded: bool,
    /// Whose session it is, before the pills.
    mark: Option<Source>,
}

fn card_model(
    session: &OpenSession,
    group: Urgency,
    holds_awake: bool,
    expanded: bool,
    now: i64,
    tuning: &Tuning,
) -> CardModel {
    let icon = sessions::state_glyph(session, now, holds_awake);
    let ring = sessions::context_ring(session, tuning);
    let mut title = vec![sessions::seg(&session.name, Tone::Plain)];
    // Unfolded, the PR line under the title says it.
    if let Some(pr) = session
        .gh
        .as_ref()
        .and_then(|g| g.pr_number)
        .filter(|_| !expanded)
    {
        title.push(sessions::seg(format!("  #{pr}"), Tone::Dim));
    }
    let failing = session
        .gh
        .as_ref()
        .and_then(|g| g.ci.as_ref())
        .is_some_and(|ci| ci.state == sl_core::gh::CiState::Failing);
    let mut pills = Vec::new();
    if session.remote_control.is_some() {
        pills.push(pill("rc", Tone::Green));
    }
    if session.activity == Activity::Busy {
        pills.push(pill(
            if holds_awake {
                "working · awake"
            } else {
                "working"
            },
            Tone::Green,
        ));
    } else {
        if failing {
            pills.push(pill("CI failed", Tone::Red));
        }
        match session.cache.and_then(|cache| cache.remaining_ms(now)) {
            Some(left) if left <= 0 => pills.push(pill("cold", Tone::Plain)),
            Some(left) if left <= tuning.needs_you_ms => pills.push(pill(
                format!("{} left", sl_core::format::fmt_elapsed(left)),
                sessions::cache_tone(left, tuning.needs_you_ms),
            )),
            Some(left) => pills.push(pill(sl_core::format::fmt_elapsed(left), Tone::Plain)),
            None if session.cache.is_some_and(|cache| cache.compacted) => {
                pills.push(pill("compacted", Tone::Plain));
            }
            // No prompt cache to time (a Codex thread): how long it has idled.
            None => {
                if let Some(since) = session.last_activity_ms.filter(|_| session.cache.is_none()) {
                    pills.push(pill(
                        format!(
                            "idle {}",
                            sl_core::format::fmt_elapsed((now - since).max(0))
                        ),
                        Tone::Plain,
                    ));
                }
            }
        }
    }
    let tint = match group {
        Urgency::NeedsYou if failing => Tone::Red,
        Urgency::NeedsYou => Tone::Orange,
        _ => Tone::Plain,
    };
    CardModel {
        session: session.clone(),
        icon,
        ring,
        title,
        pills,
        tint,
        expanded,
        mark: None,
    }
}

/// The pills of an unfolded card's PR line: state, review, CI and scores.
fn pr_pills(info: &sl_core::gh::GhInfo) -> Vec<Pill> {
    let (mut pills, ci) = crate::tray_model::pr_pills(info, PillsFor::Card);
    pills.extend(ci);
    if let Some(assessment) = &info.assessment {
        for (name, score, high_is_good) in [
            ("Risk", assessment.risk, false),
            ("Quality", assessment.quality, true),
        ] {
            if let Some(score) = score {
                pills.push(pill(
                    format!("{name} {}/{}", score.value, score.max),
                    sessions::score_tone(score, high_is_good),
                ));
            }
        }
    }
    pills
}

/// A card that asks for `act` about `target` when clicked anywhere but its
/// buttons.
fn card_shell(
    mtm: MainThreadMarker,
    tint: Tone,
    expanded: bool,
    target: Target,
    act: Act,
) -> CardView {
    let view = CardView::new(mtm, tint, expanded);
    crate::act::bind_to(&view, act, target);
    view
}

/// A card's header: the status icon, the title, and the pills on the right.
#[expect(clippy::too_many_arguments, reason = "each a part of the header")]
fn card_header(
    mtm: MainThreadMarker,
    m: Metrics,
    view: &CardView,
    icon: sessions::Icon,
    ring: Option<(u8, Tone)>,
    title_segs: &[Seg],
    pills: &[Pill],
    mark: Option<Source>,
    width: f64,
) {
    let mut right = width - CARD_PAD;
    for pill in pills.iter().rev() {
        let pill = pill_view(mtm, pill);
        right -= pill.frame().size.width;
        pill.setFrameOrigin(NSPoint::new(
            right,
            ((m.header - PILL_HEIGHT) / 2.0).round(),
        ));
        view.addSubview(&pill);
        right -= 6.0;
    }
    if let Some(image) = mark.and_then(|source| {
        crate::provider_marks::image(
            source,
            crate::menu_rows::MARK_SIZE,
            &crate::controller::dim_color(),
        )
    }) {
        let side = crate::menu_rows::MARK_SIZE;
        let mark = objc2_app_kit::NSImageView::imageViewWithImage(&image, mtm);
        right -= side;
        mark.setFrame(NSRect::new(
            NSPoint::new(right, ((m.header - side) / 2.0).round()),
            NSSize::new(side, side),
        ));
        view.addSubview(&mark);
        right -= 6.0;
    }
    let status = crate::menu_rows::StatusView::new(mtm, Some(icon), ring);
    let side = crate::menu_rows::STATUS_SIZE;
    status.setFrameOrigin(NSPoint::new(CARD_PAD, ((m.header - side) / 2.0).round()));
    view.addSubview(&status);
    let title = NSTextField::labelWithString(&NSString::new(), mtm);
    title.setAttributedStringValue(&rich_title(title_segs));
    title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    title.sizeToFit();
    let height = title.frame().size.height;
    title.setFrame(NSRect::new(
        NSPoint::new(DETAIL_X - LABEL_PAD, ((m.header - height) / 2.0).round()),
        NSSize::new((right - 4.0 - DETAIL_X).max(40.0), height),
    ));
    view.addSubview(&title);
}

/// What identifies a closed session's card across rebuilds.
fn closed_key(session_id: &str) -> String {
    format!("closed:{session_id}")
}

/// A stack's card: `Stack #746  6/10 merged`, the next PR as a pill; unfolded,
/// its PRs top first, each number a button that opens it.
fn build_stack_card(
    mtm: MainThreadMarker,
    m: Metrics,
    stack: &StackView,
    expanded: bool,
    width: f64,
) -> CardView {
    let key = stack.key();
    let view = card_shell(
        mtm,
        Tone::Plain,
        expanded,
        Target::Stack(key.clone()),
        Act::ToggleCard,
    );
    let name = stack
        .number
        .map_or_else(|| "PR chain".to_owned(), |n| format!("Stack #{n}"));
    let title = vec![
        sessions::seg(name, Tone::Plain),
        sessions::seg(
            format!("  {}/{} merged", stack.merged(), stack.entries.len()),
            Tone::Dim,
        ),
    ];
    let pills: Vec<Pill> = stack
        .next_open()
        .map(|e| pill(format!("next #{}", e.number), Tone::Plain))
        .into_iter()
        .collect();
    card_header(
        mtm,
        m,
        &view,
        sessions::Icon::Stack,
        None,
        &title,
        &pills,
        None,
        width,
    );
    let mut y = m.header;
    if expanded {
        let inner = width - DETAIL_X - CARD_PAD;
        // Top first, as GitHub lists them.
        for entry in stack.entries.iter().rev() {
            let (symbol, tone) = crate::tray_model::stack_mark(entry.state);
            let button = small_button(
                mtm,
                &format!("#{}", entry.number),
                Act::OpenStackPr,
                Target::StackPr {
                    stack: key.clone(),
                    pr: entry.number,
                },
            );
            let row_h = button.frame().size.height;
            // The menu's own marks, so both layouts draw a state alike.
            if let Some(image) =
                crate::controller::symbol_image_sized_pub(symbol, &tone_color(tone), 11.0)
            {
                let mark = objc2_app_kit::NSImageView::imageViewWithImage(&image, mtm);
                let size = image.size();
                mark.setFrame(NSRect::new(
                    NSPoint::new(DETAIL_X, y + ((row_h - size.height) / 2.0).round()),
                    size,
                ));
                view.addSubview(&mark);
            }
            button.setFrameOrigin(NSPoint::new(DETAIL_X + 16.0, y));
            view.addSubview(&button);
            let mut segs = Vec::new();
            if let Some(branch) = &entry.branch {
                segs.push(sessions::seg(
                    sessions::truncate_middle(branch, 30),
                    Tone::Dim,
                ));
            }
            match &entry.session {
                EntrySession::Live(name) => {
                    segs.push(sessions::seg(format!("  ● {name}"), Tone::Green));
                }
                EntrySession::Closed(name) => {
                    segs.push(sessions::seg(format!("  ◌ {name}"), Tone::Dim));
                }
                EntrySession::None => {}
            }
            let x = DETAIL_X + 16.0 + button.frame().size.width + 6.0;
            let text = NSTextField::labelWithString(&NSString::new(), mtm);
            text.setAttributedStringValue(&crate::controller::small_title(&segs, 11.5));
            text.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            text.sizeToFit();
            let h = text.frame().size.height;
            text.setFrame(NSRect::new(
                NSPoint::new(x - LABEL_PAD, y + ((row_h - h) / 2.0).round()),
                NSSize::new((DETAIL_X + inner - x).max(40.0), h),
            ));
            view.addSubview(&text);
            y += row_h + 2.0;
        }
        if stack.repo_url.is_some() && stack.next_open().is_some() {
            y += 4.0;
            let open = small_button(
                mtm,
                "Open stack on GitHub",
                Act::OpenStack,
                Target::Stack(key.clone()),
            );
            open.setFrameOrigin(NSPoint::new(DETAIL_X, y));
            view.addSubview(&open);
            y += open.frame().size.height;
        }
        y += CARD_PAD;
    }
    view.setFrameSize(NSSize::new(width, y));
    view
}

/// The Closed group's card: `Closed · 3`, nothing more.
fn build_closed_fold(
    mtm: MainThreadMarker,
    m: Metrics,
    closed: &[Remembered],
    open: bool,
    width: f64,
) -> CardView {
    let view = card_shell(
        mtm,
        Tone::Plain,
        false,
        Target::None,
        Act::ToggleClosedGroup,
    );
    let title = vec![sessions::seg(
        format!("Closed · {}", closed.len()),
        Tone::Plain,
    )];
    card_header(
        mtm,
        m,
        &view,
        sessions::Icon::Closed,
        None,
        &title,
        &[pill(if open { "hide" } else { "show" }, Tone::Plain)],
        None,
        width,
    );
    view.setFrameSize(NSSize::new(width, m.header));
    view
}

/// A closed session's card: its name, dimmed, and how long ago; unfolded,
/// where it was and what can still be done with it.
fn build_closed_card(
    mtm: MainThreadMarker,
    m: Metrics,
    entry: &Remembered,
    expanded: bool,
    mark: Option<Source>,
    now: i64,
    width: f64,
) -> CardView {
    const GAP: f64 = 6.0;
    let target = Target::Closed(entry.session_id.clone());
    let view = card_shell(mtm, Tone::Plain, expanded, target.clone(), Act::ToggleCard);
    card_header(
        mtm,
        m,
        &view,
        sessions::Icon::Closed,
        None,
        &[sessions::seg(&entry.name, Tone::Dim)],
        &[pill(
            crate::sticky::closed_ago(entry.last_seen_ms, now),
            Tone::Plain,
        )],
        mark,
        width,
    );
    let mut y = m.header;
    if expanded {
        let inner = width - DETAIL_X - CARD_PAD;
        let mut parts = vec![entry.project.clone()];
        parts.extend(entry.branch.clone());
        parts.extend(entry.pr_number.map(|pr| format!("#{pr}")));
        let line = label(
            mtm,
            &parts.join(" · "),
            11.5,
            &crate::controller::dim_color(),
        );
        line.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        line.setFrame(NSRect::new(
            NSPoint::new(DETAIL_X - LABEL_PAD, y),
            NSSize::new(
                line.frame().size.width.min(inner + 2.0 * LABEL_PAD),
                line.frame().size.height,
            ),
        ));
        view.addSubview(&line);
        y += line.frame().size.height + 6.0;
        let slot = (inner - 4.0 * GAP) / 5.0;
        let actions = [
            ("Zed", crate::zed_logo::SYMBOL, Act::OpenClosedZed),
            ("Copy resume command", "doc.on.doc", Act::CopyClosedId),
            ("Dismiss", "xmark.circle", Act::DismissClosed),
        ];
        for (i, (title, symbol, act)) in actions.into_iter().enumerate() {
            let button = action_button(mtm, title, symbol, act, target.clone());
            let x = (DETAIL_X + i as f64 * (slot + GAP)).round();
            let end = (DETAIL_X + i as f64 * (slot + GAP) + slot).round();
            button.setFrame(NSRect::new(
                NSPoint::new(x, y),
                NSSize::new(end - x, ACTION_HEIGHT),
            ));
            view.addSubview(&button);
        }
        y += ACTION_HEIGHT + CARD_PAD;
    }
    view.setFrameSize(NSSize::new(width, y));
    view
}

fn build_card(mtm: MainThreadMarker, m: Metrics, card: &CardModel, width: f64) -> CardView {
    let view = card_shell(
        mtm,
        card.tint,
        card.expanded,
        Target::Session(card.session.session_id.clone()),
        Act::ToggleCard,
    );
    card_header(
        mtm,
        m,
        &view,
        card.icon,
        card.ring,
        &card.title,
        &card.pills,
        card.mark,
        width,
    );

    let mut y = m.header;
    if card.expanded {
        y = add_details(mtm, &view, card, width, y);
    }
    view.setFrameSize(NSSize::new(width, y));
    view
}

/// An unfolded card's details under its header, from `y`. Returns where they
/// end.
fn add_details(
    mtm: MainThreadMarker,
    card: &CardView,
    model: &CardModel,
    width: f64,
    mut y: f64,
) -> f64 {
    const GAP: f64 = 6.0;
    let session = &model.session;
    let inner = width - DETAIL_X - CARD_PAD;
    let info = session.gh.as_ref();
    let pr = info.and_then(|i| i.pr_number);

    if let Some(linear) = &session.linear {
        let line = NSTextField::labelWithString(&NSString::new(), mtm);
        line.setAttributedStringValue(&crate::controller::small_title(
            &sessions::linear_segments(linear),
            11.5,
        ));
        line.sizeToFit();
        line.setFrameOrigin(NSPoint::new(DETAIL_X - LABEL_PAD, y));
        card.addSubview(&line);
        y += line.frame().size.height + 4.0;
    }
    if let Some(branch) = &session.branch {
        let line = label(mtm, branch, 11.0, &crate::controller::dim_color());
        line.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
            11.0,
            sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular),
        )));
        line.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        line.sizeToFit();
        line.setFrame(NSRect::new(
            NSPoint::new(DETAIL_X - LABEL_PAD, y),
            NSSize::new(
                line.frame().size.width.min(inner + 2.0 * LABEL_PAD),
                line.frame().size.height,
            ),
        ));
        card.addSubview(&line);
        y += line.frame().size.height + 6.0;
    }

    if let (Some(info), Some(number)) = (info, pr) {
        // `#753`, then the pills, wrapping onto a new line when they run out of room.
        let number = label(
            mtm,
            &format!("#{number}"),
            DETAIL_FONT_SIZE,
            &NSColor::labelColor(),
        );
        number.setFont(Some(&NSFont::systemFontOfSize_weight(
            DETAIL_FONT_SIZE,
            sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold),
        )));
        number.sizeToFit();
        number.setFrameOrigin(NSPoint::new(
            DETAIL_X - LABEL_PAD,
            y + ((PILL_HEIGHT - number.frame().size.height) / 2.0).round(),
        ));
        card.addSubview(&number);
        let mut x = DETAIL_X + number.frame().size.width - 2.0 * LABEL_PAD + 6.0;
        for pill in pr_pills(info) {
            let pill = pill_view(mtm, &pill);
            let w = pill.frame().size.width;
            if x + w > DETAIL_X + inner {
                x = DETAIL_X;
                y += PILL_HEIGHT + 4.0;
            }
            pill.setFrameOrigin(NSPoint::new(x, y));
            card.addSubview(&pill);
            x += w + 5.0;
        }
        y += PILL_HEIGHT + 6.0;
        if let Some(base) = sessions::base_segments(info) {
            let line = NSTextField::labelWithString(&NSString::new(), mtm);
            line.setAttributedStringValue(&crate::controller::small_title(&base, 11.5));
            line.setMaximumNumberOfLines(0);
            line.sizeToFit();
            line.setFrameOrigin(NSPoint::new(DETAIL_X - LABEL_PAD, y));
            card.addSubview(&line);
            y += line.frame().size.height + 6.0;
        }
    } else {
        let text = sessions::no_pr(session)
            .unwrap_or(sessions::NoPr::NoneYet)
            .text();
        let line = label(mtm, text, DETAIL_FONT_SIZE, &crate::controller::dim_color());
        line.setFrameOrigin(NSPoint::new(DETAIL_X - LABEL_PAD, y));
        card.addSubview(&line);
        y += line.frame().size.height + 6.0;
    }

    // Failed jobs, each with what can be done about it.
    let failed: &[sl_core::gh::FailedJob] = info
        .and_then(|i| i.ci.as_ref())
        .map_or(&[], |ci| &ci.failed);
    for job in failed.iter().take(MAX_JOBS) {
        let box_view = TintView::new(mtm, Tone::Red);
        let name = label(
            mtm,
            &format!("✗ {}", job.name),
            DETAIL_FONT_SIZE,
            &NSColor::labelColor(),
        );
        name.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        name.sizeToFit();
        name.setFrame(NSRect::new(
            NSPoint::new(8.0, 6.0),
            NSSize::new(
                (inner - 16.0).min(name.frame().size.width),
                name.frame().size.height,
            ),
        ));
        box_view.addSubview(&name);
        let is_actions_job = job
            .link
            .as_deref()
            .and_then(sl_core::gh::run_and_job_ids)
            .is_some();
        let mut x = 8.0;
        let row_y = 6.0 + name.frame().size.height + 4.0;
        let mut row_h: f64 = 0.0;
        for (shown, title, act) in [
            (is_actions_job, "Re-run…", Act::RetryJob),
            (is_actions_job, "Logs…", Act::ShowLogs),
            (job.link.is_some(), "Copy link", Act::CopyJobLink),
        ] {
            if !shown {
                continue;
            }
            let button = small_button(
                mtm,
                title,
                act,
                Target::Job {
                    session: session.session_id.clone(),
                    job: job.name.clone(),
                },
            );
            button.setFrameOrigin(NSPoint::new(x, row_y));
            box_view.addSubview(&button);
            x += button.frame().size.width + 4.0;
            row_h = row_h.max(button.frame().size.height);
        }
        let box_h = row_y + row_h + 6.0;
        box_view.setFrame(NSRect::new(
            NSPoint::new(DETAIL_X, y),
            NSSize::new(inner, box_h),
        ));
        card.addSubview(&box_view);
        y += box_h + 6.0;
    }

    // The actions, as the submenu has them.
    let mut actions: Vec<(&str, &str, Act)> = vec![("Zed", crate::zed_logo::SYMBOL, Act::OpenZed)];
    if pr.is_some() {
        actions.push(("PR", "arrow.up.right.square", Act::OpenPr));
        actions.push(("Copy review", "doc.on.doc", Act::CopyReview));
    }
    if session.linear.is_some() {
        actions.push(("Linear", "ticket", Act::OpenLinear));
    }
    actions.push(("Copy path", "folder", Act::CopyCwd));
    // Five slots across the column exactly, so the last ends where the pills
    // above do.
    let slot = (inner - 4.0 * GAP) / 5.0;
    for (i, (title, symbol, act)) in actions.into_iter().enumerate() {
        let button = action_button(
            mtm,
            title,
            symbol,
            act,
            Target::Session(session.session_id.clone()),
        );
        let x = (DETAIL_X + i as f64 * (slot + GAP)).round();
        let end = (DETAIL_X + i as f64 * (slot + GAP) + slot).round();
        button.setFrame(NSRect::new(
            NSPoint::new(x, y),
            NSSize::new(end - x, ACTION_HEIGHT),
        ));
        card.addSubview(&button);
    }
    y + ACTION_HEIGHT + CARD_PAD
}

/// A plain, non-selectable label.
fn label(mtm: MainThreadMarker, text: &str, size: f64, color: &NSColor) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(size)));
    label.setTextColor(Some(color));
    label.sizeToFit();
    label
}

/// The tint behind a card or pill of `tone`, from the palette the meters and
/// rings share, so a pill and a ring in the same state are the same colour.
/// Text on it stays in the label colour, so it reads on light and dark alike;
/// the tint carries the meaning.
fn tint_color(tone: Tone, strength: f64) -> Retained<NSColor> {
    match tone {
        Tone::Plain | Tone::Dim => NSColor::labelColor().colorWithAlphaComponent(strength * 0.45),
        other => crate::controller::meter_color(other).colorWithAlphaComponent(strength),
    }
}

fn pill_view(mtm: MainThreadMarker, pill: &Pill) -> TintView {
    tinted_pill(mtm, pill, false)
}

/// A pill as a view. `coloured_text`: the text in the pill's own colour, as
/// the menus have it; otherwise in the label colour.
pub(crate) fn tinted_pill(mtm: MainThreadMarker, pill: &Pill, coloured_text: bool) -> TintView {
    let view = TintView::new(mtm, pill.tone);
    view.ivars().radius.set(PILL_HEIGHT / 2.0);
    view.ivars()
        .strength
        .set(if coloured_text { 0.16 } else { 0.22 });
    let colour = if coloured_text && !matches!(pill.tone, Tone::Plain | Tone::Dim) {
        crate::controller::meter_color(pill.tone)
    } else {
        NSColor::labelColor()
    };
    let text = label(mtm, &pill.text, PILL_FONT, &colour);
    let size = text.frame().size;
    text.setFrameOrigin(NSPoint::new(
        7.0,
        ((PILL_HEIGHT - size.height) / 2.0).round(),
    ));
    view.addSubview(&text);
    view.setFrameSize(NSSize::new((size.width + 14.0).ceil(), PILL_HEIGHT));
    view
}

/// A small push button inside a card, asking for `act` about `target`.
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

/// An action under an unfolded card: an icon over a short title.
fn action_button(
    mtm: MainThreadMarker,
    title: &str,
    symbol: &str,
    act: Act,
    target: Target,
) -> Retained<NSButton> {
    let button = match action_icon(symbol, title) {
        Some(image) => sl_macos::controls::button_with_image(mtm, title, &image),
        None => sl_macos::controls::button(mtm, title),
    };
    crate::act::bind_to(&button, act, target);
    button.setBezelStyle(NSBezelStyle::FlexiblePush);
    button.setImagePosition(NSCellImagePosition::ImageAbove);
    button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    button
}

/// `symbol` (or the Zed logo) drawn centred in an [`ACTION_ICON_BOX`]
/// square, as a template the button tints.
fn action_icon(symbol: &str, title: &str) -> Option<Retained<NSImage>> {
    let icon = if symbol == crate::zed_logo::SYMBOL {
        crate::zed_logo::image(ACTION_LOGO_SIDE, &NSColor::blackColor(), true)?
    } else {
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(title)),
        )?;
        let config = objc2_app_kit::NSImageSymbolConfiguration::configurationWithPointSize_weight(
            ACTION_SYMBOL_POINTS,
            sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular),
        );
        image.imageWithSymbolConfiguration(&config).unwrap_or(image)
    };
    let draw = block2::RcBlock::new(move |bounds: NSRect| -> objc2::runtime::Bool {
        let size = icon.size();
        let origin = NSPoint::new(
            ((bounds.size.width - size.width) / 2.0).round(),
            ((bounds.size.height - size.height) / 2.0).round(),
        );
        sl_macos::draw::draw_image(&icon, NSRect::new(origin, size));
        objc2::runtime::Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(
        NSSize::new(ACTION_ICON_BOX, ACTION_ICON_BOX),
        false,
        &draw,
    );
    image.setTemplate(true);
    Some(image)
}

// --------------------------------------------------------------------------
// Views
// --------------------------------------------------------------------------

/// How a [`TintView`] fills itself.
pub(crate) struct TintLook {
    tone: Cell<Tone>,
    pub(crate) strength: Cell<f64>,
    pub(crate) radius: Cell<f64>,
}

/// A rounded fill in a tone: a pill, a card's backdrop, a separator.
#[derive(Clone)]
pub(crate) struct TintView(Canvas<TintLook>);

impl std::ops::Deref for TintView {
    type Target = Canvas<TintLook>;

    fn deref(&self) -> &Canvas<TintLook> {
        &self.0
    }
}

impl CanvasBehavior for TintLook {
    /// The colour is resolved here, at draw time, so it follows a switch
    /// between light and dark.
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        tint_color(self.tone.get(), self.strength.get()).setFill();
        let radius = self.radius.get();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(view.bounds(), radius, radius)
            .fill();
    }
}

impl TintView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        Canvas::from_view(view).map(Self)
    }

    pub(crate) fn ivars(&self) -> &TintLook {
        self.behavior()
    }

    pub(crate) fn describe(&self) -> String {
        format!("tint {:?}", self.ivars().tone.get())
    }

    pub(crate) fn new(mtm: MainThreadMarker, tone: Tone) -> Self {
        Self(Canvas::new(
            mtm,
            NSRect::ZERO,
            TintLook {
                tone: Cell::new(tone),
                strength: Cell::new(0.12),
                radius: Cell::new(7.0),
            },
        ))
    }
}

/// How a [`CardView`] looks.
pub(crate) struct CardLook {
    tint: Cell<Tone>,
    expanded: Cell<bool>,
}

/// A session's, stack's or closed session's card: a tinted, clickable
/// backdrop that folds or unfolds on a click anywhere but its buttons.
#[derive(Clone)]
pub(crate) struct CardView(ControlCanvas<CardLook>);

impl std::ops::Deref for CardView {
    type Target = ControlCanvas<CardLook>;

    fn deref(&self) -> &ControlCanvas<CardLook> {
        &self.0
    }
}

impl CanvasBehavior for CardLook {
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let bounds = view.bounds();
        let strength = if self.tint.get() == Tone::Plain {
            if self.expanded.get() { 0.16 } else { 0.1 }
        } else {
            0.12
        };
        tint_color(self.tint.get(), strength).setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, 8.0, 8.0).fill();
        if self.expanded.get() {
            let inset = NSRect::new(
                NSPoint::new(0.5, 0.5),
                NSSize::new(bounds.size.width - 1.0, bounds.size.height - 1.0),
            );
            let border = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inset, 8.0, 8.0);
            border.setLineWidth(1.0);
            NSColor::controlAccentColor()
                .colorWithAlphaComponent(0.6)
                .setStroke();
            border.stroke();
        }
    }

    /// A click anywhere on the card but its buttons folds or unfolds it.
    fn mouse_down(&self, _view: &NSControl, _event: &NSEvent) -> bool {
        true
    }

    /// The labels and pills on a card would otherwise take its clicks; only
    /// its buttons keep theirs.
    fn hit(&self) -> Hit {
        Hit::ButtonsElseSelf
    }

    fn accepts_first_mouse(&self) -> bool {
        true
    }

    /// The pointing hand over the card, as over anything that unfolds on a
    /// click; its buttons keep the arrow, as buttons do.
    fn cursor(&self) -> Option<Retained<objc2_app_kit::NSCursor>> {
        Some(objc2_app_kit::NSCursor::pointingHandCursor())
    }
}

impl CardView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        ControlCanvas::from_view(view).map(Self)
    }

    pub(crate) fn describe(&self) -> String {
        let look = self.behavior();
        format!(
            "card {:?}{}",
            look.tint.get(),
            if look.expanded.get() { " expanded" } else { "" }
        )
    }

    fn new(mtm: MainThreadMarker, tint: Tone, expanded: bool) -> Self {
        Self(ControlCanvas::new(
            mtm,
            NSRect::ZERO,
            CardLook {
                tint: Cell::new(tint),
                expanded: Cell::new(expanded),
            },
        ))
    }
}

/// For `--icon-preview`: the popover's content from `input`, with the session
/// `expanded` unfolded, in light mode, as a PNG at `path`.
pub(crate) fn preview(
    path: &str,
    input: &CompactInput,
    expanded: Option<&str>,
    closed_open: bool,
) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let popover = CompactPopover::new(mtm);
    // Light, so its dark text shows on the image's transparent background.
    popover.root.setAppearance(
        sl_macos::consts::appearance(sl_macos::consts::Appearance::Light).as_deref(),
    );
    popover.state.borrow_mut().expanded = expanded.map(str::to_owned);
    popover.state.borrow_mut().closed_open = closed_open;
    popover.update(input);
    crate::controller::write_png(&popover.root, path)
}
