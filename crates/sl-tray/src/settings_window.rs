//! The Settings window, opened by the gear in either layout's footer, in three
//! tabs:
//!
//! ```text
//! General
//!   Appearance  Layout, Meters, spend line, Linear ticket
//!   Menu bar    which meter, the sessions badge
//!   Refresh     Usage (at most every), PR & CI
//!   Keep awake  Auto caffeinate, /rc sessions
//! Sessions
//!   List        Sort, Cold, Show up to, Cards
//!   Flags       Needs you, Cache warning, Context ring
//! Notifications
//!   one switch per kind
//! ```
//!
//! A window rather than a menu: a menu bar app cannot reliably open a second
//! menu from a click in its first, and a window can hold more than choices.
//! Every control acts at once; there is nothing to apply or save.

use objc2::rc::Retained;
use crate::act::Act;
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSApplication, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSFont, NSSegmentedControl,
    NSTabView, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::meters::FlippedView;
use crate::view_prefs::{self, BarWindow, ColdMode, Density, Layout, MeterStyle, NOTIFY_KINDS, NotifyPrefs, PollPrefs, SortOrder, Tuning};

const WIDTH: f64 = 440.0;
const MARGIN: f64 = 20.0;
/// Where controls start, after the row labels.
const CONTROL_X: f64 = 112.0;
/// What the tab strip takes above and beside a tab's content.
const TAB_CHROME: f64 = 44.0;

/// Everything the controls show: the settings in use.
pub(crate) struct SettingsState {
    pub layout: Layout,
    pub meters: MeterStyle,
    pub spend: bool,
    pub poll: PollPrefs,
    pub auto: bool,
    pub notify: NotifyPrefs,
    pub tuning: Tuning,
}

pub(crate) struct SettingsWindow {
    window: Retained<NSWindow>,
    layout: Retained<NSSegmentedControl>,
    meters: Retained<NSSegmentedControl>,
    spend: Retained<NSButton>,
    linear: Retained<NSButton>,
    bar_window: Retained<NSSegmentedControl>,
    bar_badge: Retained<NSButton>,
    usage: Retained<NSSegmentedControl>,
    pr: Retained<NSSegmentedControl>,
    /// `None` when the tray may not hold the Mac awake at all.
    auto: Option<Retained<NSButton>>,
    /// `None` with `auto`.
    rc: Option<Retained<NSButton>>,
    sort: Retained<NSSegmentedControl>,
    cold: Retained<NSSegmentedControl>,
    max_sessions: Retained<NSSegmentedControl>,
    density: Retained<NSSegmentedControl>,
    needs_you: Retained<NSSegmentedControl>,
    cache_lead: Retained<NSSegmentedControl>,
    ring: Retained<NSSegmentedControl>,
    notify: Vec<Retained<NSButton>>,
}

/// Lays controls down one column of a tab.
struct Page {
    mtm: MainThreadMarker,
    content: FlippedView,
    y: f64,
}

impl Page {
    fn new(mtm: MainThreadMarker) -> Self {
        let content = FlippedView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH - 2.0 * 12.0, 300.0)));
        Self { mtm, content, y: 8.0 }
    }

    fn heading(&mut self, text: &str) {
        let label = NSTextField::labelWithString(&NSString::from_str(text), self.mtm);
        label.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold))));
        label.sizeToFit();
        label.setFrameOrigin(NSPoint::new(MARGIN - 8.0, self.y));
        self.content.addSubview(&label);
        self.y += label.frame().size.height + 8.0;
    }

    fn row_label(&self, text: &str, y: f64, height: f64) {
        let label = NSTextField::labelWithString(&NSString::from_str(text), self.mtm);
        label.setTextColor(Some(&crate::controller::dim_color()));
        label.sizeToFit();
        label.setFrameOrigin(NSPoint::new(MARGIN + 4.0, y + ((height - label.frame().size.height) / 2.0).round()));
        self.content.addSubview(&label);
    }

    /// A labelled row of segments, asking for `act`.
    fn segmented(&mut self, name: &str, labels: &[&str], act: Act) -> Retained<NSSegmentedControl> {
        let control = sl_macos::controls::segmented(self.mtm, labels);
        crate::act::bind(&control, act);
        control.setFrameSize(NSSize::new(WIDTH - 2.0 * 12.0 - CONTROL_X - MARGIN + 8.0, control.frame().size.height));
        control.setSegmentDistribution(objc2_app_kit::NSSegmentDistribution::Fill);
        control.setFrameOrigin(NSPoint::new(CONTROL_X, self.y));
        self.content.addSubview(&control);
        let height = control.frame().size.height;
        self.row_label(name, self.y, height);
        self.y += height + 8.0;
        control
    }

    fn checkbox(&mut self, title: &str, act: Act, tag: isize) -> Retained<NSButton> {
        let button = sl_macos::controls::checkbox(self.mtm, title);
        crate::act::bind(&button, act);
        button.setTag(tag);
        button.sizeToFit();
        button.setFrameOrigin(NSPoint::new(MARGIN + 2.0, self.y));
        self.content.addSubview(&button);
        self.y += button.frame().size.height + 6.0;
        button
    }

    /// A line of small grey text under a group.
    fn note(&mut self, text: &str) {
        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), self.mtm);
        label.setTextColor(Some(&crate::controller::dim_color()));
        label.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        let width = WIDTH - 2.0 * 12.0 - 2.0 * MARGIN - 4.0;
        label.setPreferredMaxLayoutWidth(width);
        // `sizeToFit` would keep it on one line; ask for the height at this width.
        let fitted = label.sizeThatFits(NSSize::new(width, 1000.0));
        label.setFrameSize(NSSize::new(width, fitted.height));
        label.setFrameOrigin(NSPoint::new(MARGIN + 4.0, self.y));
        self.content.addSubview(&label);
        self.y += label.frame().size.height + 6.0;
    }

    fn gap(&mut self, points: f64) {
        self.y += points;
    }

    /// The page's height, once everything is on it.
    fn finish(&self) -> f64 {
        self.y + 6.0
    }
}

fn labels_of<T: Copy>(items: &[T], label: impl Fn(T) -> String) -> Vec<String> {
    items.iter().map(|item| label(*item)).collect()
}

fn strs(labels: &[String]) -> Vec<&str> {
    labels.iter().map(String::as_str).collect()
}

impl SettingsWindow {
    /// The window, its controls bound to their acts.
    /// `awake_enabled`: whether the tray may hold the Mac awake at all; the
    /// Keep awake section is left out when it may not.
    pub(crate) fn new(mtm: MainThreadMarker, awake_enabled: bool) -> Self {
        // ---- General
        let mut general = Page::new(mtm);
        general.heading("Appearance");
        let layout = general.segmented("Layout", &[Layout::Regular.label(), Layout::Compact.label()], Act::SettingsLayout);
        let meters = general.segmented("Meters", &[MeterStyle::Bars.label(), MeterStyle::Rings.label()], Act::SettingsMeters);
        general.gap(2.0);
        let spend = general.checkbox("Show spend under the meters", Act::ToggleSpend, 0);
        let linear = general.checkbox("Show the Linear ticket a branch is named for", Act::ToggleLinear, 0);
        general.gap(10.0);

        general.heading("Menu bar icon");
        let bar_window = general.segmented("Meter", &strs(&labels_of(&BarWindow::ALL, |w| w.label().to_owned())), Act::SettingsBarWindow);
        general.gap(2.0);
        let bar_badge = general.checkbox("Show what the sessions are doing", Act::ToggleBarBadge, 0);
        general.gap(10.0);

        general.heading("Refresh");
        // The sessions are read when their records change. Usage too, but no
        // more often than chosen here; only GitHub has to be asked.
        let every = |page: &mut Page, name: &str, choices: &[u64], act: Act| page.segmented(name, &strs(&labels_of(choices, view_prefs::secs_label)), act);
        let usage = every(&mut general, "Usage", &view_prefs::USAGE_EVERY, Act::SettingsUsage);
        let pr = every(&mut general, "PR & CI", &view_prefs::PR_EVERY, Act::SettingsPr);
        general.gap(10.0);

        let awake = awake_enabled.then(|| {
            general.heading("Keep awake");
            let auto = general.checkbox("Auto caffeinate while a session is working", Act::ToggleAutoAwake, 0);
            let rc = general.checkbox("Keep the Mac awake for sessions with /rc on", Act::ToggleRcAwake, 0);
            (auto, rc)
        });
        let (auto, rc) = awake.map_or((None, None), |(auto, rc)| (Some(auto), Some(rc)));

        // ---- Sessions
        let mut list = Page::new(mtm);
        list.heading("List");
        let sort = list.segmented("Sort by", &strs(&labels_of(&SortOrder::ALL, |o| o.label().to_owned())), Act::SettingsSort);
        list.note("Within a group. Needs you, Working, Idle and Cold stay in that order.");
        let cold = list.segmented("Cold", &strs(&labels_of(&ColdMode::ALL, |m| m.label().to_owned())), Act::SettingsCold);
        list.note("Sessions whose cache has expired. In the popover they are left out of All and the Cold filter lists them.");
        let max_sessions = list.segmented("Show up to", &strs(&labels_of(&view_prefs::MAX_SESSION_CHOICES, |n| n.to_string())), Act::SettingsMaxSessions);
        let density = list.segmented("Cards", &strs(&labels_of(&Density::ALL, |d| d.label().to_owned())), Act::SettingsDensity);
        list.gap(10.0);

        list.heading("Flags");
        let needs_you = list.segmented("Needs you", &strs(&labels_of(&view_prefs::NEEDS_YOU_MINS, |m| format!("{m} min"))), Act::SettingsNeedsYou);
        list.note("An idle session whose cache has this long or less left.");
        let cache_lead = list.segmented("Warn", &strs(&labels_of(&view_prefs::CACHE_LEAD_MINS, |m| format!("{m} min"))), Act::SettingsCacheLead);
        list.note("How long before a one-hour cache expires to notify. A five-minute cache is warned one minute before.");
        let ring = list.segmented("Context ring", &strs(&labels_of(&view_prefs::RING_POINTS, |(note, warn)| format!("{note} / {warn} %"))), Act::SettingsRing);
        list.note("Where it turns orange, then red. The status line's own warning stays at 85 %.");

        // ---- Notifications
        let mut alerts = Page::new(mtm);
        alerts.heading("Notify me when");
        let notify = NOTIFY_KINDS.iter().enumerate().map(|(i, (_, label, _))| alerts.checkbox(label, Act::ToggleNotify, isize::try_from(i).unwrap_or(isize::MAX))).collect();

        let height = [general.finish(), list.finish(), alerts.finish()].into_iter().fold(0.0, f64::max);
        let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height + TAB_CHROME)));
        for (name, page) in [("General", general), ("Sessions", list), ("Notifications", alerts)] {
            page.content.setFrameSize(NSSize::new(WIDTH - 2.0 * 12.0, height));
            let item = sl_macos::system::tab_view_item();
            item.setLabel(&NSString::from_str(name));
            item.setView(Some(&page.content));
            tabs.addTabViewItem(&item);
        }

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height + TAB_CHROME));
        let window = sl_macos::window::new_window(mtm, frame, NSWindowStyleMask::Titled | NSWindowStyleMask::Closable);
        window.setTitle(&NSString::from_str("Statusline Tray Settings"));
        window.setContentView(Some(&tabs));
        window.center();
        Self { window, layout, meters, spend, linear, bar_window, bar_badge, usage, pr, auto, rc, sort, cold, max_sessions, density, needs_you, cache_lead, ring, notify }
    }

    /// Bring the window forward, its controls showing the settings in use.
    pub(crate) fn show(&self, state: &SettingsState) {
        self.sync(state);
        NSApplication::sharedApplication(self.window.mtm()).activate();
        self.window.makeKeyAndOrderFront(None);
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// Show the settings in use. Auto caffeinate is also switched by the cup
    /// in the footer, so this runs on every repaint while the window is up.
    pub(crate) fn sync(&self, state: &SettingsState) {
        let SettingsState { layout, meters, spend, poll, auto, notify, tuning } = state;
        let index = |found: Option<usize>| isize::try_from(found.unwrap_or(0)).unwrap_or(isize::MAX);
        self.layout.setSelectedSegment(index(Layout::ALL.iter().position(|l| l == layout)));
        self.meters.setSelectedSegment(index(MeterStyle::ALL.iter().position(|m| m == meters)));
        let mark = |on: bool| if on { NSControlStateValueOn } else { NSControlStateValueOff };
        self.spend.setState(mark(*spend));
        self.linear.setState(mark(view_prefs::load_switch(view_prefs::Switch::ShowLinear)));
        self.bar_window.setSelectedSegment(index(BarWindow::ALL.iter().position(|w| *w == tuning.bar_window)));
        self.bar_badge.setState(mark(tuning.bar_badge));
        let select = |control: &NSSegmentedControl, choices: &[u64], secs: u64| control.setSelectedSegment(index(choices.iter().position(|c| *c == secs)));
        select(&self.usage, &view_prefs::USAGE_EVERY, poll.usage_secs);
        select(&self.pr, &view_prefs::PR_EVERY, poll.pr_secs);
        if let Some(button) = &self.auto {
            button.setState(mark(*auto));
        }
        if let Some(button) = &self.rc {
            button.setState(mark(crate::view_prefs::load_switch(crate::view_prefs::Switch::RcAwake)));
        }
        self.sort.setSelectedSegment(index(SortOrder::ALL.iter().position(|o| *o == tuning.sort)));
        self.cold.setSelectedSegment(index(ColdMode::ALL.iter().position(|m| *m == tuning.cold_mode(*layout))));
        select(&self.max_sessions, &view_prefs::MAX_SESSION_CHOICES, tuning.max_sessions as u64);
        self.density.setSelectedSegment(index(Density::ALL.iter().position(|d| *d == tuning.density)));
        select(&self.needs_you, &view_prefs::NEEDS_YOU_MINS, u64::try_from(tuning.needs_you_ms / 60_000).unwrap_or(0));
        select(&self.cache_lead, &view_prefs::CACHE_LEAD_MINS, tuning.cache_lead_mins);
        self.ring.setSelectedSegment(index(view_prefs::RING_POINTS.iter().position(|(note, warn)| *note == tuning.ring_note && *warn == tuning.ring_warn)));
        for (button, on) in self.notify.iter().zip(notify.as_array()) {
            button.setState(mark(on));
        }
    }
}

/// For `--icon-preview`: each tab of the window's content, dark, as a PNG at
/// `<path minus .png>-<tab>.png`.
pub(crate) fn preview(path: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let settings = SettingsWindow::new(mtm, true);
    let state = SettingsState {
        layout: Layout::Regular,
        meters: MeterStyle::Bars,
        spend: true,
        poll: view_prefs::load_poll(),
        auto: true,
        notify: NotifyPrefs { ci_recovered: false, pr_merged: false, stack_rebase: false, ..NotifyPrefs::from_array([true; 8]) },
        tuning: Tuning::default(),
    };
    settings.sync(&state);
    let Some(content) = settings.window.contentView() else { return false };
    let dark = sl_macos::consts::appearance(sl_macos::consts::Appearance::Dark);
    objc2_app_kit::NSAppearanceCustomization::setAppearance(&*settings.window, dark.as_deref());
    let backdrop = crate::menu_rows::Backdrop::new(mtm, content.frame());
    let backdrop_view: &NSView = &backdrop;
    objc2_app_kit::NSAppearanceCustomization::setAppearance(backdrop_view, dark.as_deref());
    backdrop.addSubview(&content);
    let Ok(tabs) = content.downcast::<NSTabView>() else { return false };
    let stem = path.strip_suffix(".png").unwrap_or(path);
    ["general", "sessions", "notifications"].iter().enumerate().all(|(i, name)| {
        tabs.selectTabViewItemAtIndex(isize::try_from(i).unwrap_or(isize::MAX));
        crate::controller::write_png(&backdrop, &format!("{stem}-{name}.png"))
    })
}
