//! The Settings window, opened by the gear in either layout's footer, in four
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
//! Integrations
//!   GitHub      the account, the token field, Create token…, Remove, Save
//!   Linear      the account, the key field, Create key…, Remove, Save
//! ```
//!
//! A window rather than a menu: a menu bar app cannot reliably open a second
//! menu from a click in its first, and a window can hold more than choices.
//! Every control acts at once; there is nothing to apply or save, but for
//! the GitHub token and the Linear key, each saved with its own button.

use std::rc::Rc;

use crate::act::Act;
use crate::integrations::Service;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBox, NSBoxType, NSButton, NSColor, NSControlSize,
    NSControlStateValueOff, NSControlStateValueOn, NSFont, NSImage, NSImageView,
    NSMutableParagraphStyle, NSPopUpButton, NSSecureTextField, NSSlider, NSSplitViewController,
    NSSplitViewItem, NSSwitch, NSTabView, NSTabViewType, NSTextAlignment, NSTextField,
    NSTitlePosition, NSToolbar, NSView, NSViewController, NSWindow, NSWindowStyleMask,
    NSWindowTitleVisibility, NSWindowToolbarStyle,
};
use objc2_foundation::{MainThreadMarker, NSAttributedString, NSPoint, NSRect, NSSize, NSString};

use crate::meters::FlippedView;
use crate::view_prefs::{
    self, BarWindow, ColdMode, Density, Layout, MeterStyle, NOTIFY_KINDS, NotifyPrefs, PollPrefs,
    SortOrder, Tuning,
};

const WIDTH: f64 = 460.0;
/// A tab's content is a little narrower than the window.
const PAGE_WIDTH: f64 = WIDTH;
/// The sidebar's width, and a row's height in it.
const SIDEBAR_WIDTH: f64 = 176.0;
const SIDEBAR_ROW: f64 = 32.0;
/// The bar above a page: the traffic lights on the sidebar's side, the page's
/// name on the other.
const TOP_BAR: f64 = 56.0;
/// A sidebar row's distance from the sidebar's edges.
const SIDEBAR_INSET: f64 = 10.0;
/// The cards' left edge and width in a page.
const CARD_X: f64 = 20.0;
const CARD_WIDTH: f64 = PAGE_WIDTH - 2.0 * CARD_X;
/// A row's text and control sit this far in from the card's edge.
const ROW_INSET: f64 = 14.0;
const ROW_PAD: f64 = 9.0;
const MIN_ROW: f64 = 38.0;
const POPUP_WIDTH: f64 = 130.0;
/// A token or key field on the Integrations page.
const TOKEN_WIDTH: f64 = 200.0;
/// An account line over it.
const ACCOUNT_WIDTH: f64 = 280.0;

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
    /// Switch the sidebar and the page to the one at this index.
    show_page: Rc<dyn Fn(usize)>,
    layout: Retained<NSPopUpButton>,
    meters: Retained<NSPopUpButton>,
    spend: Retained<NSSwitch>,
    linear: Retained<NSSwitch>,
    bar_window: Retained<NSPopUpButton>,
    bar_badge: Retained<NSSwitch>,
    usage: Retained<NSPopUpButton>,
    pr: Retained<NSPopUpButton>,
    /// `None` when the tray may not hold the Mac awake at all.
    auto: Option<Retained<NSSwitch>>,
    /// `None` with `auto`.
    rc: Option<Retained<NSSwitch>>,
    sort: Retained<NSPopUpButton>,
    cold: Retained<NSPopUpButton>,
    max_sessions: Retained<NSSlider>,
    /// The number beside the slider.
    max_sessions_label: Retained<NSTextField>,
    density: Retained<NSPopUpButton>,
    needs_you: Retained<NSPopUpButton>,
    cache_lead: Retained<NSPopUpButton>,
    ring: Retained<NSPopUpButton>,
    notify: Vec<Retained<NSSwitch>>,
    /// Keep the GitHub and Linear account lines in step with
    /// [`crate::integrations`].
    _timers: [sl_macos::timer::RepeatingTimer; 2],
}

/// `service`'s section of the Integrations page: whose secret is stored, a
/// field to paste a new one into, and Create…, Remove and Save. The timer it
/// returns keeps the account line up to date while the window lives.
fn integration_section(
    page: &mut Page,
    mtm: MainThreadMarker,
    service: Service,
    placeholder: &str,
    note: &str,
) -> sl_macos::timer::RepeatingTimer {
    let secret = service.secret();
    page.heading(service.name());
    page.begin_card();
    let account = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    account.setAlignment(NSTextAlignment::Right);
    account.setTextColor(Some(&NSColor::secondaryLabelColor()));
    // Wider than the field: `GitHub refused the token: expired or revoked`.
    account.setFrameSize(NSSize::new(ACCOUNT_WIDTH, 18.0));
    page.row("Account", None, &account);
    let field = NSSecureTextField::new(mtm);
    field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    field.setFrameSize(NSSize::new(TOKEN_WIDTH, 22.0));
    let title: String = secret
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .chain(secret.chars().skip(1))
        .collect();
    page.row(&title, Some(note), &field);
    let create = sl_macos::controls::button(mtm, &format!("Create {secret}…"));
    let remove = sl_macos::controls::button(mtm, "Remove");
    let save = sl_macos::controls::button(mtm, "Save");
    let mut x = 0.0;
    let mut buttons_height: f64 = 0.0;
    for button in [&create, &remove, &save] {
        button.sizeToFit();
        button.setFrameOrigin(NSPoint::new(x, 0.0));
        x += button.frame().size.width + 8.0;
        buttons_height = buttons_height.max(button.frame().size.height);
    }
    let holder = NSView::initWithFrame(
        mtm.alloc(),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(x - 8.0, buttons_height)),
    );
    holder.addSubview(&create);
    holder.addSubview(&remove);
    holder.addSubview(&save);
    page.row("", None, &holder);
    page.end_card();
    {
        let field = field.clone();
        sl_macos::action::bind(&save, move |_| {
            let typed = field.stringValue().to_string();
            field.setStringValue(&NSString::from_str(""));
            crate::integrations::save(service, &typed);
        });
    }
    sl_macos::action::bind(&remove, move |_| crate::integrations::remove(service));
    sl_macos::action::bind(&create, move |_| {
        crate::actions::open_url(service.new_secret_url());
    });
    let mut shown = None;
    sl_macos::timer::RepeatingTimer::common_modes(0.5, 0.25, move || {
        let (count, standing) = crate::integrations::standing(service);
        if shown != Some(count) {
            shown = Some(count);
            account.setStringValue(&NSString::from_str(&standing.text(service)));
        }
    })
}

/// Lays grouped cards down one column of a tab, as System Settings does: a
/// rounded card per group, a row per setting with its title (and a grey line
/// under it) on the left and its control on the right.
struct Page {
    mtm: MainThreadMarker,
    content: FlippedView,
    y: f64,
    card: Option<Card>,
}

/// The card being filled: its box, where it starts, and its rows so far.
struct Card {
    shape: Retained<NSBox>,
    top: f64,
    rows: usize,
}

impl Page {
    fn new(mtm: MainThreadMarker) -> Self {
        let content = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PAGE_WIDTH, 300.0)),
        );
        Self {
            mtm,
            content,
            y: 10.0,
            card: None,
        }
    }

    fn heading(&mut self, text: &str) {
        let label = NSTextField::labelWithString(&NSString::from_str(text), self.mtm);
        label.setFont(Some(&NSFont::systemFontOfSize_weight(
            NSFont::systemFontSize(),
            sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold),
        )));
        label.sizeToFit();
        label.setFrameOrigin(NSPoint::new(CARD_X + 4.0, self.y));
        self.content.addSubview(&label);
        self.y += label.frame().size.height + 8.0;
    }

    /// Start a card; its rows follow, then [`Self::end_card`].
    fn begin_card(&mut self) {
        let shape = rounded_box(self.mtm, 10.0, &NSColor::quaternarySystemFillColor());
        // First, so every row is drawn over it.
        self.content.addSubview(&shape);
        self.card = Some(Card {
            shape,
            top: self.y,
            rows: 0,
        });
    }

    fn end_card(&mut self) {
        if let Some(card) = self.card.take() {
            card.shape.setFrame(NSRect::new(
                NSPoint::new(CARD_X, card.top),
                NSSize::new(CARD_WIDTH, self.y - card.top),
            ));
            self.y += 26.0;
        }
    }

    /// One row: `title` and an optional grey `note` under it, `control` at the
    /// right edge, centred on the row.
    fn row(&mut self, title: &str, note: Option<&str>, control: &NSView) {
        if let Some(card) = &mut self.card {
            if card.rows > 0 {
                let line = NSBox::initWithFrame(
                    NSBox::alloc(self.mtm),
                    NSRect::new(
                        NSPoint::new(CARD_X + ROW_INSET, self.y),
                        NSSize::new(CARD_WIDTH - 2.0 * ROW_INSET, 1.0),
                    ),
                );
                line.setBoxType(NSBoxType::Separator);
                self.content.addSubview(&line);
            }
            card.rows += 1;
        }
        let control_size = control.frame().size;
        let text_width = CARD_WIDTH - 2.0 * ROW_INSET - control_size.width - 12.0;

        let label = NSTextField::labelWithString(&NSString::from_str(title), self.mtm);
        label.sizeToFit();
        let title_height = label.frame().size.height;
        let mut text_height = title_height;
        let mut note_label = None;
        if let Some(note) = note {
            let small = NSTextField::wrappingLabelWithString(&NSString::from_str(note), self.mtm);
            small.setTextColor(Some(&NSColor::secondaryLabelColor()));
            small.setFont(Some(&NSFont::systemFontOfSize(
                NSFont::smallSystemFontSize(),
            )));
            small.setPreferredMaxLayoutWidth(text_width);
            let fitted = small.sizeThatFits(NSSize::new(text_width, 1000.0));
            small.setFrameSize(NSSize::new(text_width, fitted.height));
            text_height += 2.0 + fitted.height;
            note_label = Some(small);
        }
        let height = (text_height + 2.0 * ROW_PAD).max(MIN_ROW);
        let top = self.y + ((height - text_height) / 2.0).round();
        label.setFrameOrigin(NSPoint::new(CARD_X + ROW_INSET, top));
        self.content.addSubview(&label);
        if let Some(small) = note_label {
            small.setFrameOrigin(NSPoint::new(CARD_X + ROW_INSET, top + title_height + 2.0));
            self.content.addSubview(&small);
        }
        control.setFrameOrigin(NSPoint::new(
            CARD_X + CARD_WIDTH - ROW_INSET - control_size.width,
            self.y + ((height - control_size.height) / 2.0).round(),
        ));
        self.content.addSubview(control);
        self.y += height;
    }

    /// A row of choices as a pop-up menu, asking for `act`.
    fn choice(
        &mut self,
        name: &str,
        note: Option<&str>,
        labels: &[&str],
        act: Act,
    ) -> Retained<NSPopUpButton> {
        let popup = sl_macos::controls::popup(self.mtm, labels);
        crate::act::bind(&popup, act);
        popup.setBordered(false);
        popup.setAlignment(NSTextAlignment::Right);
        popup.sizeToFit();
        popup.setFrameSize(NSSize::new(POPUP_WIDTH, popup.frame().size.height));
        self.row(name, note, &popup);
        popup
    }

    /// A row with a slider of `positions` stops, the chosen value beside it
    /// (set by `sync`).
    fn range(
        &mut self,
        name: &str,
        positions: usize,
        act: Act,
    ) -> (Retained<NSSlider>, Retained<NSTextField>) {
        const LABEL_WIDTH: f64 = 26.0;
        let slider = sl_macos::controls::slider(self.mtm, positions);
        crate::act::bind(&slider, act);
        let value = NSTextField::labelWithString(&NSString::from_str(""), self.mtm);
        value.setAlignment(NSTextAlignment::Right);
        let height = slider.frame().size.height.max(18.0);
        let slider_width = POPUP_WIDTH - LABEL_WIDTH - 6.0;
        slider.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(slider_width, height),
        ));
        value.setFrame(NSRect::new(
            NSPoint::new(slider_width + 6.0, 0.0),
            NSSize::new(LABEL_WIDTH, height),
        ));
        let holder = NSView::initWithFrame(
            self.mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(POPUP_WIDTH, height)),
        );
        holder.addSubview(&slider);
        holder.addSubview(&value);
        self.row(name, None, &holder);
        (slider, value)
    }

    /// A row with an on/off switch, asking for `act`.
    fn toggle(&mut self, title: &str, act: Act, tag: isize) -> Retained<NSSwitch> {
        let switch = sl_macos::controls::switch(self.mtm);
        crate::act::bind(&switch, act);
        switch.setTag(tag);
        switch.setAccessibilityLabel(Some(&NSString::from_str(title)));
        switch.setControlSize(NSControlSize::Mini);
        switch.sizeToFit();
        self.row(title, None, &switch);
        switch
    }

    /// The page's height, once everything is on it.
    fn finish(&self) -> f64 {
        self.y + 6.0
    }
}

/// A rounded rectangle filled with `fill`, no border.
fn rounded_box(mtm: MainThreadMarker, radius: f64, fill: &NSColor) -> Retained<NSBox> {
    let shape = NSBox::initWithFrame(NSBox::alloc(mtm), NSRect::ZERO);
    shape.setBoxType(NSBoxType::Custom);
    shape.setTitlePosition(NSTitlePosition::NoTitle);
    shape.setBorderWidth(0.0);
    shape.setCornerRadius(radius);
    shape.setContentViewMargins(NSSize::new(0.0, 0.0));
    shape.setFillColor(fill);
    shape
}

/// A sidebar row's title, indented past its icon; white on the selected row.
fn sidebar_title(name: &str, selected: bool) -> Retained<NSAttributedString> {
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setFirstLineHeadIndent(36.0);
    paragraph.setAlignment(NSTextAlignment::Left);
    let color = if selected {
        NSColor::whiteColor()
    } else {
        NSColor::labelColor()
    };
    sl_macos::text::attributed(
        name,
        &sl_macos::text::Attrs {
            font: Some(&NSFont::systemFontOfSize_weight(
                NSFont::systemFontSize(),
                sl_macos::consts::font_weight(if selected {
                    sl_macos::consts::Weight::Semibold
                } else {
                    sl_macos::consts::Weight::Regular
                }),
            )),
            color: Some(&color),
            paragraph: Some(&paragraph),
            ..Default::default()
        },
    )
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
        general.begin_card();
        let layout = general.choice(
            "Layout",
            None,
            &[Layout::Regular.label(), Layout::Compact.label()],
            Act::SettingsLayout,
        );
        let meters = general.choice(
            "Meters",
            None,
            &[MeterStyle::Bars.label(), MeterStyle::Rings.label()],
            Act::SettingsMeters,
        );
        let spend = general.toggle("Show spend under the meters", Act::ToggleSpend, 0);
        let linear = general.toggle(
            "Show the Linear ticket a branch is named for",
            Act::ToggleLinear,
            0,
        );
        general.end_card();

        general.heading("Menu bar icon");
        general.begin_card();
        let bar_window = general.choice(
            "Meter",
            None,
            &strs(&labels_of(&BarWindow::ALL, |w| w.label().to_owned())),
            Act::SettingsBarWindow,
        );
        let bar_badge = general.toggle("Show what the sessions are doing", Act::ToggleBarBadge, 0);
        general.end_card();

        general.heading("Refresh");
        general.begin_card();
        // The sessions are read when their records change. Usage too, but no
        // more often than chosen here; only GitHub has to be asked.
        let every = |page: &mut Page, name: &str, choices: &[u64], act: Act| {
            page.choice(
                name,
                None,
                &strs(&labels_of(choices, view_prefs::secs_label)),
                act,
            )
        };
        let usage = every(
            &mut general,
            "Usage",
            &view_prefs::USAGE_EVERY,
            Act::SettingsUsage,
        );
        let pr = every(
            &mut general,
            "PR & CI",
            &view_prefs::PR_EVERY,
            Act::SettingsPr,
        );
        general.end_card();

        let awake = awake_enabled.then(|| {
            general.heading("Keep awake");
            general.begin_card();
            let auto = general.toggle(
                "Auto caffeinate while a session is working",
                Act::ToggleAutoAwake,
                0,
            );
            let rc = general.toggle(
                "Keep the Mac awake for sessions with /rc on",
                Act::ToggleRcAwake,
                0,
            );
            general.end_card();
            (auto, rc)
        });
        let (auto, rc) = awake.map_or((None, None), |(auto, rc)| (Some(auto), Some(rc)));

        // ---- Sessions
        let mut list = Page::new(mtm);
        list.heading("List");
        list.begin_card();
        let sort = list.choice(
            "Sort by",
            Some("Within a group. Needs you, Working, Idle and Cold stay in that order."),
            &strs(&labels_of(&SortOrder::ALL, |o| o.label().to_owned())),
            Act::SettingsSort,
        );
        let cold = list.choice(
            "Cold",
            Some("Sessions whose cache has expired. In the popover they are left out of All and the Cold filter lists them."),
            &strs(&labels_of(&ColdMode::ALL, |m| m.label().to_owned())),
            Act::SettingsCold,
        );
        let (max_sessions, max_sessions_label) = list.range(
            "Show up to",
            view_prefs::MAX_SESSION_CHOICES.len(),
            Act::SettingsMaxSessions,
        );
        let density = list.choice(
            "Cards",
            None,
            &strs(&labels_of(&Density::ALL, |d| d.label().to_owned())),
            Act::SettingsDensity,
        );
        list.end_card();

        list.heading("Flags");
        list.begin_card();
        let needs_you = list.choice(
            "Needs you",
            Some("An idle session whose cache has this long or less left."),
            &strs(&labels_of(&view_prefs::NEEDS_YOU_MINS, |m| {
                format!("{m} min")
            })),
            Act::SettingsNeedsYou,
        );
        let cache_lead = list.choice(
            "Warn",
            Some("How long before a one-hour cache expires to notify. A five-minute cache is warned one minute before."),
            &strs(&labels_of(&view_prefs::CACHE_LEAD_MINS, |m| {
                format!("{m} min")
            })),
            Act::SettingsCacheLead,
        );
        let ring = list.choice(
            "Context ring",
            Some("Where it turns orange, then red. The status line's own warning stays at 85 %."),
            &strs(&labels_of(&view_prefs::RING_POINTS, |(note, warn)| {
                format!("{note} / {warn} %")
            })),
            Act::SettingsRing,
        );
        list.end_card();

        // ---- Notifications
        let mut alerts = Page::new(mtm);
        alerts.heading("Notify me when");
        alerts.begin_card();
        let notify = NOTIFY_KINDS
            .iter()
            .enumerate()
            .map(|(i, (_, label, _))| {
                alerts.toggle(
                    label,
                    Act::ToggleNotify,
                    isize::try_from(i).unwrap_or(isize::MAX),
                )
            })
            .collect();
        alerts.end_card();

        // ---- Integrations
        let mut links = Page::new(mtm);
        let github_timer = integration_section(
            &mut links,
            mtm,
            Service::GitHub,
            "ghp_… or github_pat_…",
            "A classic token with the repo and read:org scopes. Kept in your login Keychain.",
        );
        let linear_timer = integration_section(
            &mut links,
            mtm,
            Service::Linear,
            "lin_api_…",
            "A personal API key for one workspace; read access is enough. Kept in your login Keychain.",
        );

        let height = [
            general.finish(),
            list.finish(),
            alerts.finish(),
            links.finish(),
        ]
        .into_iter()
        .fold(0.0, f64::max);
        let tabs = NSTabView::initWithFrame(
            NSTabView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, TOP_BAR), NSSize::new(WIDTH, height)),
        );
        tabs.setTabViewType(NSTabViewType::NoTabsNoBorder);
        let pages = [
            ("General", "gearshape", general),
            ("Sessions", "rectangle.stack", list),
            ("Notifications", "bell", alerts),
            ("Integrations", "key", links),
        ];
        let mut sidebar = Vec::new();
        for (name, symbol, page) in pages {
            page.content.setFrameSize(NSSize::new(PAGE_WIDTH, height));
            let item = sl_macos::system::tab_view_item();
            item.setLabel(&NSString::from_str(name));
            item.setView(Some(&page.content));
            tabs.addTabViewItem(&item);
            sidebar.push((name, symbol));
        }

        let total = height + TOP_BAR;
        // The sidebar's rows and the page, each the view of one split item: the
        // split view controller gives the sidebar its system material.
        let side = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIDEBAR_WIDTH, total)),
        );
        let main = FlippedView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, total)),
        );
        // The page's name, in the top bar, level with the traffic lights.
        let title = NSTextField::labelWithString(&NSString::from_str("General"), mtm);
        title.setFont(Some(&NSFont::systemFontOfSize_weight(
            15.0,
            sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold),
        )));
        title.setFrame(NSRect::new(
            NSPoint::new(CARD_X + 4.0, 16.0),
            NSSize::new(WIDTH - 2.0 * CARD_X, 20.0),
        ));
        main.addSubview(&title);
        // The selected row's pill sits under its button.
        let pill = rounded_box(mtm, 10.0, &NSColor::controlAccentColor());
        side.addSubview(&pill);
        let tile_colors = [
            NSColor::systemGrayColor(),
            NSColor::systemBlueColor(),
            NSColor::systemRedColor(),
            NSColor::systemGreenColor(),
        ];
        let buttons: Vec<Retained<NSButton>> = sidebar
            .iter()
            .enumerate()
            .map(|(i, (name, symbol))| {
                let row_y = TOP_BAR + SIDEBAR_ROW * i as f64;
                // The icon: a coloured tile with a white symbol, under the button.
                let tile = rounded_box(mtm, 5.0, &tile_colors[i % tile_colors.len()]);
                tile.setFrame(NSRect::new(
                    NSPoint::new(SIDEBAR_INSET + 9.0, row_y + (SIDEBAR_ROW - 20.0) / 2.0),
                    NSSize::new(20.0, 20.0),
                ));
                side.addSubview(&tile);
                if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    None,
                ) {
                    let glyph = NSImageView::imageViewWithImage(&image, mtm);
                    glyph.setContentTintColor(Some(&NSColor::whiteColor()));
                    glyph.setFrame(NSRect::new(
                        NSPoint::new(SIDEBAR_INSET + 12.5, row_y + (SIDEBAR_ROW - 13.0) / 2.0),
                        NSSize::new(13.0, 13.0),
                    ));
                    side.addSubview(&glyph);
                }
                // The whole row is the button; its title is indented past the icon.
                let button = sl_macos::controls::button(mtm, name);
                button.setBordered(false);
                button.setAlignment(NSTextAlignment::Left);
                button.setFrame(NSRect::new(
                    NSPoint::new(SIDEBAR_INSET, row_y),
                    NSSize::new(SIDEBAR_WIDTH - 2.0 * SIDEBAR_INSET, SIDEBAR_ROW),
                ));
                side.addSubview(&button);
                button
            })
            .collect();
        main.addSubview(&tabs);
        let show_page: Rc<dyn Fn(usize)> = {
            let tabs = tabs.clone();
            let buttons = buttons.clone();
            let pill = pill.clone();
            let title = title.clone();
            let names: Vec<&str> = sidebar.iter().map(|(name, _)| *name).collect();
            Rc::new(move |index| {
                title.setStringValue(&NSString::from_str(names.get(index).copied().unwrap_or("")));
                tabs.selectTabViewItemAtIndex(isize::try_from(index).unwrap_or(0));
                for (i, button) in buttons.iter().enumerate() {
                    if i == index {
                        pill.setFrame(button.frame());
                    }
                    button.setAttributedTitle(&sidebar_title(
                        names.get(i).copied().unwrap_or(""),
                        i == index,
                    ));
                }
            })
        };
        for (i, button) in buttons.iter().enumerate() {
            let show = Rc::clone(&show_page);
            sl_macos::action::bind(button, move |_| show(i));
        }
        show_page(0);

        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(SIDEBAR_WIDTH + WIDTH, height + TOP_BAR),
        );
        let window = sl_macos::window::new_window(
            mtm,
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::FullSizeContentView,
        );
        window.setTitle(&NSString::from_str("Statusline Tray Settings"));
        // An empty toolbar: it is what gives the title bar the taller System
        // Settings height, with the traffic lights centred in it.
        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(mtm),
            &NSString::from_str("statusline-tray-settings"),
        );
        window.setToolbar(Some(&toolbar));
        window.setToolbarStyle(NSWindowToolbarStyle::Unified);
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        let side_controller = NSViewController::new(mtm);
        side_controller.setView(&side);
        let main_controller = NSViewController::new(mtm);
        main_controller.setView(&main);
        let split = NSSplitViewController::new(mtm);
        let side_item = NSSplitViewItem::sidebarWithViewController(&side_controller);
        side_item.setMinimumThickness(SIDEBAR_WIDTH);
        side_item.setMaximumThickness(SIDEBAR_WIDTH);
        side_item.setCanCollapse(false);
        let main_item = NSSplitViewItem::splitViewItemWithViewController(&main_controller);
        main_item.setMinimumThickness(WIDTH);
        split.addSplitViewItem(&side_item);
        split.addSplitViewItem(&main_item);
        window.setContentViewController(Some(&split));
        window.setContentSize(NSSize::new(SIDEBAR_WIDTH + WIDTH, total));
        window.center();
        Self {
            window,
            show_page,
            layout,
            meters,
            spend,
            linear,
            bar_window,
            bar_badge,
            usage,
            pr,
            auto,
            rc,
            sort,
            cold,
            max_sessions,
            max_sessions_label,
            density,
            needs_you,
            cache_lead,
            ring,
            notify,
            _timers: [github_timer, linear_timer],
        }
    }

    /// Bring the window forward, its controls showing the settings in use.
    pub(crate) fn show(&self, state: &SettingsState) {
        self.sync(state);
        crate::integrations::refresh();
        NSApplication::sharedApplication(self.window.mtm()).activate();
        self.window.makeKeyAndOrderFront(None);
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// Show the settings in use. Auto caffeinate is also switched by the cup
    /// in the footer, so this runs on every repaint while the window is up.
    pub(crate) fn sync(&self, state: &SettingsState) {
        let SettingsState {
            layout,
            meters,
            spend,
            poll,
            auto,
            notify,
            tuning,
        } = state;
        let index =
            |found: Option<usize>| isize::try_from(found.unwrap_or(0)).unwrap_or(isize::MAX);
        self.layout
            .selectItemAtIndex(index(Layout::ALL.iter().position(|l| l == layout)));
        self.meters
            .selectItemAtIndex(index(MeterStyle::ALL.iter().position(|m| m == meters)));
        let mark = |on: bool| {
            if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            }
        };
        self.spend.setState(mark(*spend));
        self.linear.setState(mark(view_prefs::load_switch(
            view_prefs::Switch::ShowLinear,
        )));
        self.bar_window.selectItemAtIndex(index(
            BarWindow::ALL.iter().position(|w| *w == tuning.bar_window),
        ));
        self.bar_badge.setState(mark(tuning.bar_badge));
        let select = |control: &NSPopUpButton, choices: &[u64], secs: u64| {
            control.selectItemAtIndex(index(choices.iter().position(|c| *c == secs)));
        };
        select(&self.usage, &view_prefs::USAGE_EVERY, poll.usage_secs);
        select(&self.pr, &view_prefs::PR_EVERY, poll.pr_secs);
        if let Some(button) = &self.auto {
            button.setState(mark(*auto));
        }
        if let Some(button) = &self.rc {
            button.setState(mark(crate::view_prefs::load_switch(
                crate::view_prefs::Switch::RcAwake,
            )));
        }
        self.sort
            .selectItemAtIndex(index(SortOrder::ALL.iter().position(|o| *o == tuning.sort)));
        self.cold.selectItemAtIndex(index(
            ColdMode::ALL
                .iter()
                .position(|m| *m == tuning.cold_mode(*layout)),
        ));
        let position = view_prefs::MAX_SESSION_CHOICES
            .iter()
            .position(|c| *c == tuning.max_sessions as u64);
        self.max_sessions
            .setDoubleValue(f64::from(u32::try_from(position.unwrap_or(0)).unwrap_or(0)));
        self.max_sessions_label
            .setStringValue(&NSString::from_str(&tuning.max_sessions.to_string()));
        self.density.selectItemAtIndex(index(
            Density::ALL.iter().position(|d| *d == tuning.density),
        ));
        select(
            &self.needs_you,
            &view_prefs::NEEDS_YOU_MINS,
            u64::try_from(tuning.needs_you_ms / 60_000).unwrap_or(0),
        );
        select(
            &self.cache_lead,
            &view_prefs::CACHE_LEAD_MINS,
            tuning.cache_lead_mins,
        );
        self.ring.selectItemAtIndex(index(
            view_prefs::RING_POINTS
                .iter()
                .position(|(note, warn)| *note == tuning.ring_note && *warn == tuning.ring_warn),
        ));
        for (button, on) in self.notify.iter().zip(notify.as_array()) {
            button.setState(mark(on));
        }
    }
}

/// For `--icon-preview`: each tab of the window's content, dark, as a PNG at
/// `<path minus .png>-<tab>.png`.
pub(crate) fn preview(path: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let settings = SettingsWindow::new(mtm, true);
    let state = SettingsState {
        layout: Layout::Regular,
        meters: MeterStyle::Bars,
        spend: true,
        poll: view_prefs::load_poll(),
        auto: true,
        notify: NotifyPrefs {
            ci_recovered: false,
            pr_merged: false,
            stack_rebase: false,
            ..NotifyPrefs::from_array([true; 8])
        },
        tuning: Tuning::default(),
    };
    settings.sync(&state);
    let Some(content) = settings.window.contentView() else {
        return false;
    };
    let dark = sl_macos::consts::appearance(sl_macos::consts::Appearance::Dark);
    objc2_app_kit::NSAppearanceCustomization::setAppearance(&*settings.window, dark.as_deref());
    let backdrop = crate::menu_rows::Backdrop::new(mtm, content.frame());
    let backdrop_view: &NSView = &backdrop;
    objc2_app_kit::NSAppearanceCustomization::setAppearance(backdrop_view, dark.as_deref());
    backdrop.addSubview(&content);
    let stem = path.strip_suffix(".png").unwrap_or(path);
    ["general", "sessions", "notifications", "integrations"]
        .iter()
        .enumerate()
        .all(|(i, name)| {
            (settings.show_page)(i);
            crate::controller::write_png(&backdrop, &format!("{stem}-{name}.png"))
        })
}
