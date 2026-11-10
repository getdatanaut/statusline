//! The Stats window: everything the snapshot knows, laid out as a native
//! window -- a toolbar with the range control, "Updated Ns ago" as the window
//! subtitle, and one scrolling view that draws the tiles, charts and tables in
//! system fonts and colours, in groups like System Settings'.
//!
//! The figures come from [`model`], which is plain Rust and tested; this file
//! lays them out and paints them. The window is built on first use and kept
//! for the life of the process (closing it only orders it out), so the range
//! and session tab picked stay picked; both are also kept in user defaults.

mod model;

use std::cell::{Cell, OnceCell, RefCell};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSApplication, NSAutoresizingMaskOptions,
    NSBezierPath, NSColor, NSControlSize, NSEvent, NSFont, NSFontWeight, NSGraphicsContext,
    NSLineBreakMode, NSMutableParagraphStyle, NSScrollView, NSSegmentedControl, NSShadow,
    NSStringDrawingOptions, NSTextAlignment, NSToolbar, NSToolbarDisplayMode, NSToolbarItem,
    NSTrackingAreaOptions, NSView, NSWindow, NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{MainThreadMarker, NSCopying, NSPoint, NSRect, NSSize, NSString};
use sl_macos::canvas::{Canvas, CanvasBehavior};

use crate::snapshot::{Snapshot, now_ms};
use crate::view_prefs::{self, Provider};
use model::{Bar, DEFAULT_RANGE, RANGES, Series, StatsModel, Tab};

const RANGE_ITEM: &str = "range";
/// The provider control: only while Codex is installed.
const PROVIDER_ITEM: &str = "provider";
const RANGE_DEFAULT: &str = "StatsRangeDays";
const TAB_DEFAULT: &str = "StatsSessionTab";

// Layout, in points.
const MARGIN: f64 = 20.0;
/// Between sections.
const GAP: f64 = 22.0;
/// Between boxes side by side.
const GUTTER: f64 = 12.0;
/// Inside a group box.
const PAD: f64 = 14.0;
const RADIUS: f64 = 10.0;
/// A section heading, and the space under it.
const HEADING: f64 = 26.0;
const TILE_HEIGHT: f64 = 78.0;
/// From this width the tiles go four across and the two lists side by side.
const WIDE: f64 = 760.0;
const LEGEND_ROW: f64 = 20.0;
const PLOT_HEIGHT: f64 = 200.0;
const AXIS_WIDTH: f64 = 46.0;
const AXIS_HEIGHT: f64 = 22.0;
/// The widest a day's column gets, however few days are shown.
const COLUMN_MAX: f64 = 24.0;
/// The gap between stacked segments, and a column's rounded top.
const SEGMENT_GAP: f64 = 2.0;
const COLUMN_RADIUS: f64 = 4.0;
const BAR_ROW: f64 = 38.0;
const BAR_HEIGHT: f64 = 6.0;
const STAT_ROW: f64 = 42.0;
const TABLE_HEAD: f64 = 26.0;
const TABLE_ROW: f64 = 26.0;
/// A table drops its optional columns until its text column is this wide.
const MIN_TEXT_COLUMN: f64 = 150.0;
/// How faint the other columns get while one is hovered.
const HOVER_DIM: f64 = 0.4;
const READOUT_WIDTH: f64 = 250.0;

/// The validated categorical palette (slots 1-4 and Other), light and dark.
/// Not the system colours: system blue is also the accent colour, and system
/// yellow fails contrast on a light background.
const PALETTE_LIGHT: [u32; 5] = [
    0x002a_78d6,
    0x00eb_6834,
    0x001b_af7a,
    0x00ed_a100,
    0x00b5_b3ab,
];
const PALETTE_DARK: [u32; 5] = [
    0x0039_87e5,
    0x00d9_5926,
    0x0019_9e70,
    0x00c9_8500,
    0x005d_5c57,
];

pub struct StatsWindow {
    window: Retained<NSWindow>,
    view: StatsView,
    subtitle: RefCell<String>,
    /// Whose usage is shown, for the title and the toolbar's control.
    provider: Cell<Provider>,
}

impl StatsWindow {
    /// The window; its toolbar's provider control, shown only when `codex`
    /// is installed, asks for `Act::StatsProvider`.
    pub fn new(
        mtm: MainThreadMarker,
        snapshot: &Snapshot,
        provider: Provider,
        codex: bool,
    ) -> Self {
        Self::build(mtm, snapshot, provider, codex, true)
    }

    /// For `--stats-preview`: the same window, but one that does not save its
    /// frame to user defaults.
    pub fn preview(
        mtm: MainThreadMarker,
        snapshot: &Snapshot,
        provider: Provider,
        width: Option<f64>,
    ) -> Self {
        let this = Self::build(
            mtm,
            snapshot,
            provider,
            view_prefs::codex_available(),
            false,
        );
        if let Some(width) = width {
            let frame = this.window.frame();
            this.window.setFrame_display(
                rect(frame.origin.x, frame.origin.y, width, frame.size.height),
                true,
            );
        }
        this
    }

    fn build(
        mtm: MainThreadMarker,
        snapshot: &Snapshot,
        provider: Provider,
        codex: bool,
        remember_frame: bool,
    ) -> Self {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(900.0, 780.0));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let window = sl_macos::window::new_window(mtm, frame, style);
        window.setTitle(&NSString::from_str(title_for(provider)));
        window.setMinSize(NSSize::new(560.0, 480.0));
        window.center();
        // Frame autosave always writes the standard defaults, so not under a suite.
        if remember_frame && crate::view_prefs::suite().is_none() {
            // Remembers where the reader left it, across launches.
            window.setFrameAutosaveName(&NSString::from_str("SLTrayStatsWindow"));
        }

        let view = StatsView::new(mtm, snapshot);
        if codex {
            let labels: Vec<&str> = Provider::ALL
                .iter()
                .map(|provider| provider.label())
                .collect();
            let control = sl_macos::controls::segmented(mtm, &labels);
            crate::act::bind(&control, crate::act::Act::StatsProvider);
            control.setSelectedSegment(
                isize::try_from(
                    Provider::ALL
                        .iter()
                        .position(|p| *p == provider)
                        .unwrap_or(0),
                )
                .unwrap_or(isize::MAX),
            );
            // Before the toolbar exists: it asks for its items as it is set.
            view.ivars().provider.set(control).ok();
        }
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        // The window's own background shows through, so it is the system's.
        scroll.setDrawsBackground(false);
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        scroll.setDocumentView(Some(&view));
        window.setContentView(Some(&scroll));

        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(mtm),
            &NSString::from_str("SLTrayStatsToolbar"),
        );
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
        let (items_of, item_of) = (view.weak(), view.weak());
        sl_macos::delegate::set_toolbar_delegate(
            &toolbar,
            sl_macos::delegate::ToolbarItems {
                identifiers: Box::new(move || {
                    toolbar_items(
                        StatsView::upgrade(&items_of)
                            .is_some_and(|view| view.ivars().provider.get().is_some()),
                    )
                }),
                item: Box::new(move |identifier| {
                    let view = StatsView::upgrade(&item_of)?;
                    view.range_item(identifier)
                        .or_else(|| view.provider_item(identifier))
                }),
            },
        );
        window.setToolbar(Some(&toolbar));
        window.setToolbarStyle(NSWindowToolbarStyle::Unified);
        // The clip view does not resize its document view, so the window's
        // own resizes, live ones included, are what reflow the content.
        let resized = view.weak();
        sl_macos::delegate::set_window_delegate(&window, move || {
            if let Some(view) = StatsView::upgrade(&resized) {
                view.relayout();
            }
        });

        view.relayout();
        let this = Self {
            window,
            view,
            subtitle: RefCell::new(String::new()),
            provider: Cell::new(provider),
        };
        this.update(snapshot, provider);
        this
    }

    /// Bring the window up in front, with `snapshot` of `provider`'s usage.
    pub fn show(&self, snapshot: &Snapshot, provider: Provider) {
        self.update(snapshot, provider);
        NSApplication::sharedApplication(self.window.mtm()).activate();
        self.window.makeKeyAndOrderFront(None);
    }

    /// Keep the subtitle's age ticking and hand the view `snapshot`, which
    /// redraws only if what it shows has changed.
    pub fn update(&self, snapshot: &Snapshot, provider: Provider) {
        let now = now_ms();
        if self.provider.replace(provider) != provider {
            self.window
                .setTitle(&NSString::from_str(title_for(provider)));
            if let Some(control) = self.view.ivars().provider.get() {
                control.setSelectedSegment(
                    isize::try_from(
                        Provider::ALL
                            .iter()
                            .position(|p| *p == provider)
                            .unwrap_or(0),
                    )
                    .unwrap_or(isize::MAX),
                );
            }
        }
        let subtitle = crate::tray_model::footer_text(snapshot, now);
        if *self.subtitle.borrow() != subtitle {
            self.window.setSubtitle(&NSString::from_str(&subtitle));
            *self.subtitle.borrow_mut() = subtitle;
        }
        self.view.set_snapshot(snapshot, now);
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// For `--stats-preview`: hover the chart's column `index`.
    pub fn preview_hover(&self, index: usize) {
        self.view.set_hover(Some(index));
    }

    /// For `--stats-preview`: the whole window, title bar and toolbar
    /// included, as a PNG at `path`.
    pub fn preview_png(&self, path: &str) -> bool {
        // Tall enough for everything, so one image shows the whole window.
        let content = self.window.contentLayoutRect().size;
        let chrome = self.window.frame().size.height - content.height;
        let full = self.view.ivars().layout.borrow().height;
        let width = self.window.frame().size.width;
        self.window.setFrame_display(
            rect(self.window.frame().origin.x, 0.0, width, full + chrome),
            true,
        );
        objc2_foundation::NSRunLoop::currentRunLoop()
            .runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.5));
        let Some(frame) = self
            .window
            .contentView()
            .and_then(|content| sl_macos::system::superview(&content))
        else {
            return false;
        };
        let bounds = frame.bounds();
        let Some(bitmap) = frame.bitmapImageRepForCachingDisplayInRect(bounds) else {
            return false;
        };
        frame.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
        let png = sl_macos::draw::png(&bitmap);
        let window =
            png.is_some_and(|png| png.writeToFile_atomically(&NSString::from_str(path), true));
        // And the whole content, which a window no taller than the screen
        // may not show all of.
        let bounds = self.view.bounds();
        let Some(bitmap) = self.view.bitmapImageRepForCachingDisplayInRect(bounds) else {
            return false;
        };
        self.view
            .cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
        let png = sl_macos::draw::png(&bitmap);
        let content = png.is_some_and(|png| {
            png.writeToFile_atomically(
                &NSString::from_str(&path.replace(".png", "-content.png")),
                true,
            )
        });
        window && content
    }
}

/// Where everything goes, for one width. Top-down: the view is flipped.
#[derive(Debug, Clone, Default)]
struct Layout {
    height: f64,
    tiles: Vec<NSRect>,
    chart_heading: f64,
    chart: NSRect,
    /// Where the columns stand: the chart less its legend and axes.
    plot: NSRect,
    /// The cost-by-model and tokens-by-kind lists: heading, box.
    lists: [(f64, NSRect); 2],
    sessions_heading: f64,
    sessions: NSRect,
    models_heading: f64,
    models: NSRect,
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(width.max(0.0), height.max(0.0)),
    )
}

fn layout(model: &StatsModel, width: f64) -> Layout {
    let inner = (width - 2.0 * MARGIN).max(0.0);
    let wide = width >= WIDE;
    let mut y = MARGIN;
    let mut out = Layout::default();

    let across = if wide { 4 } else { 2 };
    let tile_width = (inner - GUTTER * f64::from(across - 1)) / f64::from(across);
    for i in 0..model.tiles.len() {
        let (row, column) = (
            i / usize::try_from(across).unwrap_or(0),
            i % usize::try_from(across).unwrap_or(0),
        );
        out.tiles.push(rect(
            MARGIN + column as f64 * (tile_width + GUTTER),
            y + row as f64 * (TILE_HEIGHT + GUTTER),
            tile_width,
            TILE_HEIGHT,
        ));
    }
    let rows = model
        .tiles
        .len()
        .div_ceil(usize::try_from(across).unwrap_or(0)) as f64;
    y += rows * TILE_HEIGHT + (rows - 1.0).max(0.0) * GUTTER + GAP;

    out.chart_heading = y;
    y += HEADING;
    let legend_rows = legend_rows(model, inner - 2.0 * PAD).len().max(1) as f64;
    let legend_height = legend_rows * LEGEND_ROW + 8.0;
    out.chart = rect(
        MARGIN,
        y,
        inner,
        PAD + legend_height + PLOT_HEIGHT + AXIS_HEIGHT + PAD - 6.0,
    );
    out.plot = rect(
        MARGIN + PAD + AXIS_WIDTH,
        y + PAD + legend_height,
        inner - 2.0 * PAD - AXIS_WIDTH,
        PLOT_HEIGHT,
    );
    y += out.chart.size.height + GAP;

    let list_height = |bars: &[Bar]| 2.0 * PAD + bars.len().max(1) as f64 * BAR_ROW - 10.0;
    if wide {
        let left = ((inner - GUTTER) * 0.58).round();
        let height = list_height(&model.by_model).max(list_height(&model.by_kind));
        out.lists = [
            (y, rect(MARGIN, y + HEADING, left, height)),
            (
                y,
                rect(
                    MARGIN + left + GUTTER,
                    y + HEADING,
                    inner - left - GUTTER,
                    height,
                ),
            ),
        ];
        y += HEADING + height + GAP;
    } else {
        for (i, bars) in [&model.by_model, &model.by_kind].into_iter().enumerate() {
            let height = list_height(bars);
            out.lists[i] = (y, rect(MARGIN, y + HEADING, inner, height));
            y += HEADING + height + GAP;
        }
    }

    out.sessions_heading = y;
    y += HEADING;
    out.sessions = rect(
        MARGIN,
        y,
        inner,
        PAD + 2.0 * STAT_ROW
            + 10.0
            + TABLE_HEAD
            + model.sessions.len().max(1) as f64 * TABLE_ROW
            + PAD
            - 4.0,
    );
    y += out.sessions.size.height + GAP;

    out.models_heading = y;
    y += HEADING;
    out.models = rect(
        MARGIN,
        y,
        inner,
        PAD + TABLE_HEAD + model.models.len().max(1) as f64 * TABLE_ROW + PAD - 4.0,
    );
    y += out.models.size.height;

    out.height = y + MARGIN;
    out
}

/// The legend's entries, wrapped into rows that fit `width`: indices into
/// `model.legend`, with each entry's x offset.
fn legend_rows(model: &StatsModel, width: f64) -> Vec<Vec<(usize, f64)>> {
    if model.legend.len() < 2 {
        return Vec::new();
    }
    let font = font(12.0, Weight::Regular);
    let mut rows: Vec<Vec<(usize, f64)>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, entry) in model.legend.iter().enumerate() {
        let entry_width = 16.0 + text_width(&entry.name, &font);
        if x > 0.0 && x + entry_width > width {
            rows.push(Vec::new());
            x = 0.0;
        }
        if let Some(row) = rows.last_mut() {
            row.push((i, x));
        }
        x += entry_width + 16.0;
    }
    rows
}

pub(crate) struct Ivars {
    snapshot: RefCell<Snapshot>,
    model: RefCell<StatsModel>,
    layout: RefCell<Layout>,
    days: Cell<u32>,
    tab: Cell<Tab>,
    /// The chart column under the pointer.
    hover: Cell<Option<usize>>,
    /// In the toolbar.
    range: OnceCell<Retained<NSSegmentedControl>>,
    /// In the toolbar, when there is a provider to choose: Claude Code,
    /// Codex or All. Targets the controller, which owns the choice.
    provider: OnceCell<Retained<NSSegmentedControl>>,
    /// Beside the Sessions heading.
    tabs: OnceCell<Retained<NSSegmentedControl>>,
}

/// The Stats window's content: drawn by hand, following the pointer over
/// the chart.
#[derive(Clone)]
pub(crate) struct StatsView(Canvas<Ivars>);

impl std::ops::Deref for StatsView {
    type Target = Canvas<Ivars>;

    fn deref(&self) -> &Canvas<Ivars> {
        &self.0
    }
}

impl CanvasBehavior for Ivars {
    fn draw(&self, view: &NSView, dirty: NSRect) {
        if let Some(view) = StatsView::from_view(view) {
            view.draw(dirty);
        }
    }

    fn mouse_moved(&self, view: &NSView, event: &NSEvent) {
        let Some(view) = StatsView::from_view(view) else {
            return;
        };
        let point = view.convertPoint_fromView(event.locationInWindow(), None);
        let hover = {
            let layout = self.layout.borrow();
            let count = self.model.borrow().columns.len();
            column_at(&layout, count, point)
        };
        view.set_hover(hover);
    }

    fn mouse_exited(&self, view: &NSView, _event: &NSEvent) {
        if let Some(view) = StatsView::from_view(view) {
            view.set_hover(None);
        }
    }

    // The series colours are picked per appearance at draw time, and the
    // canvas redraws on an appearance change: nothing more to do.
}

fn toolbar_items(provider: bool) -> Vec<Retained<NSString>> {
    let flexible = sl_macos::consts::toolbar_flexible_space();
    let mut items = vec![flexible.copy()];
    if provider {
        items.push(NSString::from_str(PROVIDER_ITEM));
    }
    items.push(NSString::from_str(RANGE_ITEM));
    items
}

/// Which column `point` is over, if it is over the plot at all.
fn column_at(layout: &Layout, count: usize, point: NSPoint) -> Option<usize> {
    let plot = layout.plot;
    let inside = point.x >= plot.origin.x
        && point.x < plot.origin.x + plot.size.width
        && point.y >= layout.chart.origin.y
        && point.y < plot.origin.y + plot.size.height + AXIS_HEIGHT;
    if !inside || count == 0 {
        return None;
    }
    let band = plot.size.width / count as f64;
    Some(sl_core::num::trunc_usize((point.x - plot.origin.x) / band).min(count - 1))
}

impl StatsView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        Canvas::from_view(view).map(Self)
    }

    /// The view again, from [`Canvas::weak`], while it lives.
    fn upgrade(weak: &objc2::rc::Weak<sl_macos::canvas::CanvasView>) -> Option<Self> {
        Canvas::upgrade(weak).map(Self)
    }

    fn ivars(&self) -> &Ivars {
        self.behavior()
    }

    fn range_changed(&self, sender: &AnyObject) {
        let days = RANGES
            .get(usize::try_from(sl_macos::action::selected_segment(sender)).unwrap_or(0))
            .copied()
            .unwrap_or(DEFAULT_RANGE);
        self.ivars().days.set(days);
        crate::view_prefs::defaults().setInteger_forKey(
            isize::try_from(days).unwrap_or(isize::MAX),
            &NSString::from_str(RANGE_DEFAULT),
        );
        self.ivars().hover.set(None);
        self.rebuild(now_ms());
    }

    fn tab_changed(&self, sender: &AnyObject) {
        let segment = sl_macos::action::selected_segment(sender);
        let tab = if segment == 1 { Tab::Top } else { Tab::Latest };
        self.ivars().tab.set(tab);
        crate::view_prefs::defaults().setInteger_forKey(segment, &NSString::from_str(TAB_DEFAULT));
        self.rebuild(now_ms());
    }

    /// The toolbar's one item: the range control.
    fn range_item(&self, identifier: &NSString) -> Option<Retained<NSToolbarItem>> {
        if identifier.to_string() != RANGE_ITEM {
            return None;
        }
        let item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), identifier);
        item.setLabel(&NSString::from_str("Range"));
        item.setView(Some(self.ivars().range.get()?));
        Some(item)
    }

    /// The toolbar's provider item, when there is one.
    fn provider_item(&self, identifier: &NSString) -> Option<Retained<NSToolbarItem>> {
        if identifier.to_string() != PROVIDER_ITEM {
            return None;
        }
        let item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), identifier);
        item.setLabel(&NSString::from_str("Show"));
        item.setView(Some(self.ivars().provider.get()?));
        Some(item)
    }

    fn new(mtm: MainThreadMarker, snapshot: &Snapshot) -> Self {
        let defaults = crate::view_prefs::defaults();
        let stored =
            u32::try_from(defaults.integerForKey(&NSString::from_str(RANGE_DEFAULT))).unwrap_or(0);
        let days = if RANGES.contains(&stored) {
            stored
        } else {
            DEFAULT_RANGE
        };
        let tab = if defaults.integerForKey(&NSString::from_str(TAB_DEFAULT)) == 1 {
            Tab::Top
        } else {
            Tab::Latest
        };
        let model = StatsModel::build(snapshot, days, tab, now_ms());
        let this = Self(Canvas::new(
            mtm,
            rect(0.0, 0.0, 900.0, 800.0),
            Ivars {
                snapshot: RefCell::new(snapshot.clone()),
                model: RefCell::new(model),
                layout: RefCell::new(Layout::default()),
                days: Cell::new(days),
                tab: Cell::new(tab),
                hover: Cell::new(None),
                range: OnceCell::new(),
                provider: OnceCell::new(),
                tabs: OnceCell::new(),
            },
        ));

        let labels: Vec<String> = RANGES.iter().map(|days| format!("{days} Days")).collect();
        let range = sl_macos::controls::segmented(
            mtm,
            &labels.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let weak = this.weak();
        sl_macos::action::bind(&range, move |sender| {
            if let Some(view) = Self::upgrade(&weak) {
                view.range_changed(sender);
            }
        });
        range.setSelectedSegment(
            isize::try_from(RANGES.iter().position(|d| *d == days).unwrap_or(1))
                .unwrap_or(isize::MAX),
        );
        this.ivars().range.set(range).ok();

        let tabs = sl_macos::controls::segmented(mtm, &["Latest", "Most Expensive"]);
        let weak = this.weak();
        sl_macos::action::bind(&tabs, move |sender| {
            if let Some(view) = Self::upgrade(&weak) {
                view.tab_changed(sender);
            }
        });
        tabs.setControlSize(NSControlSize::Small);
        tabs.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        tabs.sizeToFit();
        tabs.setSelectedSegment(isize::from(tab == Tab::Top));
        this.addSubview(&tabs);
        this.ivars().tabs.set(tabs).ok();

        // `InVisibleRect` keeps the area matched to the view as it resizes, so
        // it never needs replacing.
        let options = NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::ActiveInActiveApp
            | NSTrackingAreaOptions::InVisibleRect;
        // `self` implements `mouseMoved:`/`mouseExited:`.
        sl_macos::system::track_view(&this, options);
        this
    }

    /// What the view shows, in brief (see [`crate::inspect`]).
    pub(crate) fn describe(&self) -> String {
        let ivars = self.ivars();
        let model = ivars.model.borrow();
        format!(
            "stats {}d {:?} columns={} width={:.0} hover={:?}",
            ivars.days.get(),
            ivars.tab.get(),
            model.columns.len(),
            self.frame().size.width,
            ivars.hover.get()
        )
    }

    /// Show `snapshot` as of `now`, if that changes anything on screen.
    fn set_snapshot(&self, snapshot: &Snapshot, now: i64) {
        if *self.ivars().snapshot.borrow() != *snapshot {
            *self.ivars().snapshot.borrow_mut() = snapshot.clone();
        }
        self.rebuild(now);
    }

    /// Recompute the model; lay out and redraw only if it changed.
    fn rebuild(&self, now: i64) {
        let ivars = self.ivars();
        let model = StatsModel::build(
            &ivars.snapshot.borrow(),
            ivars.days.get(),
            ivars.tab.get(),
            now,
        );
        if *ivars.model.borrow() == model {
            return;
        }
        if ivars.hover.get().is_some_and(|i| i >= model.columns.len()) {
            ivars.hover.set(None);
        }
        *ivars.model.borrow_mut() = model;
        self.relayout();
        self.setNeedsDisplay(true);
    }

    /// Fit the view to its scroll view's width and the model's height, and
    /// put the session tabs beside their heading.
    fn relayout(&self) {
        let ivars = self.ivars();
        // SAFETY: reading the view's superview, the scroll view's clip view.
        let (width, min_height) = sl_macos::system::superview(self)
            .map_or((self.frame().size.width, 0.0), |clip| {
                (clip.bounds().size.width, clip.bounds().size.height)
            });
        let layout = layout(&ivars.model.borrow(), width);
        self.setFrameSize(NSSize::new(width, layout.height.max(min_height)));
        if let Some(tabs) = ivars.tabs.get() {
            let size = tabs.frame().size;
            tabs.setFrameOrigin(NSPoint::new(
                MARGIN + layout.sessions.size.width - size.width,
                layout.sessions_heading - 3.0,
            ));
        }
        *ivars.layout.borrow_mut() = layout;
        self.setNeedsDisplay(true);
    }

    fn set_hover(&self, hover: Option<usize>) {
        if self.ivars().hover.get() != hover {
            self.ivars().hover.set(hover);
            self.setNeedsDisplayInRect(self.ivars().layout.borrow().chart);
        }
    }

    fn draw(&self, dirty: NSRect) {
        let model = self.ivars().model.borrow();
        let layout = self.ivars().layout.borrow();
        let palette = if is_dark(&self.effectiveAppearance()) {
            PALETTE_DARK
        } else {
            PALETTE_LIGHT
        };
        let paint = Paint { palette };
        let wanted = |r: NSRect| intersects(r, dirty);

        for (tile, area) in model.tiles.iter().zip(&layout.tiles) {
            if wanted(*area) {
                group(*area);
                let x = area.origin.x + PAD;
                let width = area.size.width - 2.0 * PAD;
                draw_text(
                    tile.label,
                    rect(x, area.origin.y + 11.0, width, 16.0),
                    &font(12.0, Weight::Regular),
                    &NSColor::secondaryLabelColor(),
                    Align::Left,
                );
                draw_text(
                    &tile.value,
                    rect(x, area.origin.y + 27.0, width, 30.0),
                    &digits(22.0, Weight::Semibold),
                    &NSColor::labelColor(),
                    Align::Left,
                );
                draw_text(
                    &tile.detail,
                    rect(x, area.origin.y + 55.0, width, 16.0),
                    &digits(11.0, Weight::Regular),
                    &NSColor::secondaryLabelColor(),
                    Align::Left,
                );
            }
        }

        if wanted(rect(
            MARGIN,
            layout.chart_heading,
            layout.chart.size.width,
            HEADING + layout.chart.size.height,
        )) {
            heading(
                "Daily cost",
                None,
                layout.chart_heading,
                layout.chart.size.width,
            );
            draw_text(
                &model.range_summary,
                rect(MARGIN, layout.chart_heading, layout.chart.size.width, 18.0),
                &digits(12.0, Weight::Regular),
                &NSColor::secondaryLabelColor(),
                Align::Right,
            );
            group(layout.chart);
            paint.chart(&model, &layout, self.ivars().hover.get());
        }

        let list_titles = [
            ("Cost by model", format!("last {} days", model.days)),
            ("Tokens by kind", "all time".to_owned()),
        ];
        for (i, bars) in [&model.by_model, &model.by_kind].into_iter().enumerate() {
            let (heading_y, area) = layout.lists[i];
            if wanted(rect(
                area.origin.x,
                heading_y,
                area.size.width,
                HEADING + area.size.height,
            )) {
                heading_at(
                    list_titles[i].0,
                    Some(&list_titles[i].1),
                    area.origin.x,
                    heading_y,
                    area.size.width,
                );
                group(area);
                paint.bars(bars, area);
            }
        }

        if wanted(rect(
            MARGIN,
            layout.sessions_heading,
            layout.sessions.size.width,
            HEADING + layout.sessions.size.height,
        )) {
            heading(
                "Sessions",
                None,
                layout.sessions_heading,
                layout.sessions.size.width,
            );
            group(layout.sessions);
            Paint::sessions(&model, layout.sessions);
        }

        if wanted(rect(
            MARGIN,
            layout.models_heading,
            layout.models.size.width,
            HEADING + layout.models.size.height,
        )) {
            heading(
                "Models",
                Some("all time"),
                layout.models_heading,
                layout.models.size.width,
            );
            group(layout.models);
            paint.models(&model, layout.models);
        }
    }
}

fn intersects(a: NSRect, b: NSRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

/// The window's title for whose usage it shows.
fn title_for(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code Usage",
        Provider::Codex => "Codex Usage",
        Provider::All => "Usage",
    }
}

fn is_dark(appearance: &NSAppearance) -> bool {
    sl_macos::consts::is_dark(appearance)
}

// --------------------------------------------------------------------------
// Painting
// --------------------------------------------------------------------------

struct Paint {
    palette: [u32; 5],
}

impl Paint {
    fn series(&self, series: Series) -> Retained<NSColor> {
        let hex = match series {
            Series::Slot(i) => self.palette[i.min(3)],
            Series::Other => self.palette[4],
        };
        hex_color(hex)
    }

    fn chart(&self, model: &StatsModel, layout: &Layout, hover: Option<usize>) {
        let area = layout.chart;
        let plot = layout.plot;
        let small = font(12.0, Weight::Regular);
        let axis = digits(11.0, Weight::Regular);

        for row in legend_rows(model, area.size.width - 2.0 * PAD)
            .iter()
            .enumerate()
        {
            let (r, entries) = row;
            for (i, x) in entries {
                let entry = &model.legend[*i];
                let left = area.origin.x + PAD + x;
                let top = area.origin.y + PAD + r as f64 * LEGEND_ROW;
                fill_round(
                    rect(left, top + 3.0, 10.0, 10.0),
                    2.5,
                    &self.series(entry.series),
                );
                draw_text(
                    &entry.name,
                    rect(left + 16.0, top, 400.0, 16.0),
                    &small,
                    &NSColor::secondaryLabelColor(),
                    Align::Left,
                );
            }
        }

        let y_of =
            |value: f64| plot.origin.y + plot.size.height - value / model.top * plot.size.height;
        let mut value = 0.0;
        while value <= model.top + 1e-9 {
            let y = y_of(value).round() + 0.5;
            let color = if value == 0.0 {
                NSColor::tertiaryLabelColor()
            } else {
                NSColor::separatorColor()
            };
            color.setStroke();
            NSBezierPath::setDefaultLineWidth(1.0);
            NSBezierPath::strokeLineFromPoint_toPoint(
                NSPoint::new(plot.origin.x, y),
                NSPoint::new(plot.origin.x + plot.size.width, y),
            );
            draw_text(
                &model::axis_money(value),
                rect(area.origin.x + PAD - 4.0, y - 8.0, AXIS_WIDTH - 4.0, 14.0),
                &axis,
                &NSColor::secondaryLabelColor(),
                Align::Right,
            );
            value += model.step;
        }

        let count = model.columns.len();
        if count == 0 {
            return;
        }
        let band = plot.size.width / count as f64;
        let bar = (band * 0.7).clamp(2.0, COLUMN_MAX);
        let label_every =
            count.div_ceil(sl_core::num::trunc_usize((plot.size.width / 64.0).floor()).max(1));
        for (i, column) in model.columns.iter().enumerate() {
            let center = plot.origin.x + band * (i as f64 + 0.5);
            let alpha = if hover.is_some_and(|h| h != i) {
                HOVER_DIM
            } else {
                1.0
            };
            let drawn: Vec<usize> = (0..column.parts.len())
                .filter(|k| column.parts[*k] > 0.0)
                .collect();
            let mut base = 0.0;
            for (n, k) in drawn.iter().enumerate() {
                let part = column.parts[*k];
                let (bottom, top) = (y_of(base), y_of(base + part));
                // A gap of the surface between stacked segments.
                let height = bottom - top - if n > 0 { SEGMENT_GAP } else { 0.0 };
                if height > 0.0 {
                    let radius = if n == drawn.len() - 1 {
                        COLUMN_RADIUS.min(bar / 2.0).min(height)
                    } else {
                        0.0
                    };
                    let color = self
                        .series(model.legend[*k].series)
                        .colorWithAlphaComponent(alpha);
                    column_path(center - bar / 2.0, top, bar, height, radius, &color);
                }
                base += part;
            }
            if (count - 1 - i).is_multiple_of(label_every) {
                draw_text(
                    &column.short,
                    rect(
                        center - 40.0,
                        plot.origin.y + plot.size.height + 5.0,
                        80.0,
                        14.0,
                    ),
                    &axis,
                    &NSColor::secondaryLabelColor(),
                    Align::Center,
                );
            }
        }

        if let Some(i) = hover.filter(|i| *i < count) {
            self.readout(
                model,
                i,
                plot.origin.x + band * (i as f64 + 0.5),
                area,
                plot.origin.y,
            );
        }
    }

    /// The hovered day: its date, total, and each model's share, costliest on
    /// top as the column stacks them.
    fn readout(&self, model: &StatsModel, i: usize, center: f64, area: NSRect, top: f64) {
        let column = &model.columns[i];
        let parts: Vec<usize> = (0..column.parts.len())
            .rev()
            .filter(|k| column.parts[*k] > 0.0)
            .collect();
        let height = 10.0 + 16.0 + 22.0 + (parts.len().max(1) as f64) * 18.0 + 6.0;
        let right = area.origin.x + area.size.width - 8.0;
        let x = if center + 12.0 + READOUT_WIDTH > right {
            center - 12.0 - READOUT_WIDTH
        } else {
            center + 12.0
        };
        // On the plot, under the legend, which it would otherwise cover.
        let frame = rect(x.max(area.origin.x + 8.0), top, READOUT_WIDTH, height);

        NSGraphicsContext::saveGraphicsState_class();
        let shadow = NSShadow::new();
        shadow.setShadowColor(Some(&NSColor::shadowColor().colorWithAlphaComponent(0.25)));
        shadow.setShadowBlurRadius(10.0);
        shadow.setShadowOffset(NSSize::new(0.0, -2.0));
        shadow.set();
        fill_round(frame, 8.0, &NSColor::windowBackgroundColor());
        NSGraphicsContext::restoreGraphicsState_class();
        let border =
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inset(frame, 0.5), 8.0, 8.0);
        NSColor::separatorColor().setStroke();
        border.setLineWidth(1.0);
        border.stroke();

        let (x, width) = (frame.origin.x + 12.0, READOUT_WIDTH - 24.0);
        let mut y = frame.origin.y + 10.0;
        draw_text(
            &column.long,
            rect(x, y, width, 16.0),
            &font(11.0, Weight::Regular),
            &NSColor::secondaryLabelColor(),
            Align::Left,
        );
        y += 16.0;
        draw_text(
            &model::money(column.total),
            rect(x, y, width, 20.0),
            &digits(15.0, Weight::Semibold),
            &NSColor::labelColor(),
            Align::Left,
        );
        y += 22.0;
        if parts.is_empty() {
            draw_text(
                "No usage",
                rect(x, y, width, 16.0),
                &font(12.0, Weight::Regular),
                &NSColor::secondaryLabelColor(),
                Align::Left,
            );
        }
        for k in parts {
            fill_round(
                rect(x, y + 4.0, 8.0, 8.0),
                4.0,
                &self.series(model.legend[k].series),
            );
            draw_text(
                &model::money(column.parts[k]),
                rect(x + 14.0, y, 72.0, 16.0),
                &digits(12.0, Weight::Semibold),
                &NSColor::labelColor(),
                Align::Left,
            );
            draw_text(
                &model.legend[k].name,
                rect(x + 88.0, y, width - 88.0, 16.0),
                &font(12.0, Weight::Regular),
                &NSColor::secondaryLabelColor(),
                Align::Left,
            );
            y += 18.0;
        }
    }

    fn bars(&self, bars: &[Bar], area: NSRect) {
        let (x, width) = (area.origin.x + PAD, area.size.width - 2.0 * PAD);
        if bars.is_empty() {
            draw_text(
                "Nothing in this range",
                rect(x, area.origin.y + PAD + 4.0, width, 18.0),
                &font(13.0, Weight::Regular),
                &NSColor::secondaryLabelColor(),
                Align::Center,
            );
            return;
        }
        let max = bars.iter().map(|bar| bar.value).fold(0.0, f64::max);
        let (name_font, note_font, value_font) = (
            font(13.0, Weight::Regular),
            digits(11.0, Weight::Regular),
            digits(13.0, Weight::Regular),
        );
        for (i, bar) in bars.iter().enumerate() {
            let y = area.origin.y + PAD + i as f64 * BAR_ROW;
            let value_width = text_width(&bar.label, &value_font);
            draw_text(
                &bar.label,
                rect(x + width - value_width - 2.0, y, value_width + 2.0, 18.0),
                &value_font,
                &NSColor::labelColor(),
                Align::Right,
            );
            let name_room = width - value_width - 12.0;
            let name_width = text_width(&bar.name, &name_font).min(name_room);
            draw_text(
                &bar.name,
                rect(x, y, name_room, 18.0),
                &name_font,
                &NSColor::labelColor(),
                Align::Left,
            );
            if let Some(note) = &bar.note {
                draw_text(
                    note,
                    rect(
                        x + name_width + 6.0,
                        y + 2.0,
                        (name_room - name_width - 6.0).max(0.0),
                        16.0,
                    ),
                    &note_font,
                    &NSColor::secondaryLabelColor(),
                    Align::Left,
                );
            }
            let color = bar
                .series
                .map_or_else(NSColor::systemGrayColor, |series| self.series(series));
            let length = if max > 0.0 {
                (width * bar.value / max).max(BAR_HEIGHT / 2.0)
            } else {
                0.0
            };
            fill_round(
                rect(x, y + 21.0, length, BAR_HEIGHT),
                (BAR_HEIGHT / 2.0).min(length / 2.0),
                &color,
            );
        }
    }

    fn sessions(model: &StatsModel, area: NSRect) {
        let (x, width) = (area.origin.x + PAD, area.size.width - 2.0 * PAD);
        let column = width / 3.0;
        for (i, (key, value)) in model.session_stats.iter().enumerate() {
            let (cx, cy) = (
                x + (i % 3) as f64 * column,
                area.origin.y + PAD + (i / 3) as f64 * STAT_ROW,
            );
            draw_text(
                key,
                rect(cx, cy, column - 12.0, 14.0),
                &font(11.0, Weight::Regular),
                &NSColor::secondaryLabelColor(),
                Align::Left,
            );
            draw_text(
                value,
                rect(cx, cy + 15.0, column - 12.0, 20.0),
                &digits(15.0, Weight::Semibold),
                &NSColor::labelColor(),
                Align::Left,
            );
        }
        let top = area.origin.y + PAD + 2.0 * STAT_ROW + 10.0;
        let columns = [
            Col {
                title: "Project",
                width: 0.0,
                right: false,
                optional: false,
            },
            Col {
                title: "Session",
                width: 82.0,
                right: false,
                optional: true,
            },
            Col {
                title: "Cost",
                width: 84.0,
                right: true,
                optional: false,
            },
            Col {
                title: "Tokens",
                width: 70.0,
                right: true,
                optional: true,
            },
            Col {
                title: "Messages",
                width: 78.0,
                right: true,
                optional: true,
            },
            Col {
                title: "Last Active",
                width: 86.0,
                right: true,
                optional: false,
            },
        ];
        let rows: Vec<Vec<&str>> = model
            .sessions
            .iter()
            .map(|s| {
                vec![
                    s.project.as_str(),
                    s.session.as_str(),
                    s.cost.as_str(),
                    s.tokens.as_str(),
                    s.messages.as_str(),
                    s.last_active.as_str(),
                ]
            })
            .collect();
        table(
            x,
            top,
            width,
            &columns,
            &rows,
            "No sessions yet",
            |row, column, cell| {
                // The session id is an id, not prose: monospaced and quieter.
                (column == 1)
                    .then(|| {
                        draw_text(
                            rows[row][1],
                            cell,
                            &mono(11.0),
                            &NSColor::secondaryLabelColor(),
                            Align::Left,
                        )
                    })
                    .is_some()
            },
        );
    }

    fn models(&self, model: &StatsModel, area: NSRect) {
        let (x, width) = (area.origin.x + PAD, area.size.width - 2.0 * PAD);
        let columns = [
            Col {
                title: "Model",
                width: 0.0,
                right: false,
                optional: false,
            },
            Col {
                title: "Tier",
                width: 80.0,
                right: false,
                optional: true,
            },
            Col {
                title: "Cost",
                width: 90.0,
                right: true,
                optional: false,
            },
            Col {
                title: "Tokens",
                width: 74.0,
                right: true,
                optional: true,
            },
            Col {
                title: "Output",
                width: 74.0,
                right: true,
                optional: true,
            },
        ];
        let rows: Vec<Vec<&str>> = model
            .models
            .iter()
            .map(|m| {
                vec![
                    m.model.as_str(),
                    m.tier,
                    m.cost.as_str(),
                    m.tokens.as_str(),
                    m.output.as_str(),
                ]
            })
            .collect();
        let name_font = font(12.0, Weight::Regular);
        table(
            x,
            top_of(area),
            width,
            &columns,
            &rows,
            "No models recorded yet",
            |row, column, cell| match column {
                0 => {
                    let entry = &model.models[row];
                    fill_round(
                        rect(cell.origin.x, cell.origin.y + 5.0, 8.0, 8.0),
                        4.0,
                        &self.series(entry.series),
                    );
                    let name = rect(
                        cell.origin.x + 16.0,
                        cell.origin.y,
                        cell.size.width - 16.0,
                        cell.size.height,
                    );
                    draw_text(
                        &entry.model,
                        name,
                        &name_font,
                        &NSColor::labelColor(),
                        Align::Left,
                    );
                    if entry.missing_pricing {
                        let used = text_width(&entry.model, &name_font) + 8.0;
                        let note = rect(
                            name.origin.x + used,
                            cell.origin.y,
                            (name.size.width - used).max(0.0),
                            cell.size.height,
                        );
                        draw_text(
                            "⚠ no pricing — cost reads $0",
                            note,
                            &font(11.0, Weight::Regular),
                            &NSColor::systemOrangeColor(),
                            Align::Left,
                        );
                    }
                    true
                }
                1 => draw_text(
                    rows[row][1],
                    cell,
                    &name_font,
                    &NSColor::secondaryLabelColor(),
                    Align::Left,
                )
                .is_some(),
                _ => false,
            },
        );
    }
}

fn top_of(area: NSRect) -> f64 {
    area.origin.y + PAD - 4.0
}

/// A table column. `width` 0 is the one text column that takes what is left.
struct Col {
    title: &'static str,
    width: f64,
    right: bool,
    /// Left out, last first, when the table is too narrow for its text column.
    optional: bool,
}

/// Header, rows, separators between them. `custom` may draw a cell itself and
/// return true; otherwise the cell is plain text, numbers in monospaced digits.
fn table(
    x: f64,
    top: f64,
    width: f64,
    columns: &[Col],
    rows: &[Vec<&str>],
    empty: &str,
    custom: impl Fn(usize, usize, NSRect) -> bool,
) {
    const SPACING: f64 = 12.0;
    let mut shown: Vec<usize> = (0..columns.len()).collect();
    let fixed = |shown: &[usize]| {
        shown
            .iter()
            .map(|i| {
                columns[*i].width
                    + if columns[*i].width > 0.0 {
                        SPACING
                    } else {
                        0.0
                    }
            })
            .sum::<f64>()
    };
    while width - fixed(&shown) < MIN_TEXT_COLUMN {
        let Some(drop) = shown.iter().rposition(|i| columns[*i].optional) else {
            break;
        };
        shown.remove(drop);
    }
    let flex = (width - fixed(&shown)).max(40.0);
    let mut lefts = Vec::with_capacity(shown.len());
    let mut cursor = x;
    for i in &shown {
        let w = if columns[*i].width > 0.0 {
            columns[*i].width
        } else {
            flex
        };
        lefts.push((*i, cursor, w));
        cursor += w + SPACING;
    }

    let head = font(11.0, Weight::Semibold);
    for (i, left, w) in &lefts {
        let column = &columns[*i];
        draw_text(
            column.title,
            rect(*left, top + 6.0, *w, 14.0),
            &head,
            &NSColor::secondaryLabelColor(),
            if column.right {
                Align::Right
            } else {
                Align::Left
            },
        );
    }
    separator(x, top + TABLE_HEAD - 0.5, width);

    let (text, numbers) = (font(12.0, Weight::Regular), digits(12.0, Weight::Regular));
    if rows.is_empty() {
        draw_text(
            empty,
            rect(x, top + TABLE_HEAD + 5.0, width, 16.0),
            &text,
            &NSColor::secondaryLabelColor(),
            Align::Center,
        );
        return;
    }
    for (r, row) in rows.iter().enumerate() {
        let y = top + TABLE_HEAD + r as f64 * TABLE_ROW;
        for (i, left, w) in &lefts {
            let cell = rect(*left, y + 5.0, *w, 16.0);
            if custom(r, *i, cell) {
                continue;
            }
            let column = &columns[*i];
            let font = if column.right { &numbers } else { &text };
            draw_text(
                row[*i],
                cell,
                font,
                &NSColor::labelColor(),
                if column.right {
                    Align::Right
                } else {
                    Align::Left
                },
            );
        }
        if r + 1 < rows.len() {
            separator(x, y + TABLE_ROW - 0.5, width);
        }
    }
}

fn separator(x: f64, y: f64, width: f64) {
    NSColor::separatorColor().setFill();
    NSBezierPath::fillRect(rect(x, y.floor(), width, 1.0));
}

/// A section heading over a box: bold title, then an optional quieter note.
fn heading(title: &str, note: Option<&str>, y: f64, width: f64) {
    heading_at(title, note, MARGIN, y, width);
}

fn heading_at(title: &str, note: Option<&str>, x: f64, y: f64, width: f64) {
    let bold = font(13.0, Weight::Semibold);
    draw_text(
        title,
        rect(x + 2.0, y, width, 18.0),
        &bold,
        &NSColor::labelColor(),
        Align::Left,
    );
    if let Some(note) = note {
        let used = text_width(title, &bold) + 4.0;
        draw_text(
            &format!("· {note}"),
            rect(x + 2.0 + used, y, (width - used).max(0.0), 18.0),
            &font(13.0, Weight::Regular),
            &NSColor::secondaryLabelColor(),
            Align::Left,
        );
    }
}

/// A rounded group, as System Settings draws them: a faint fill, no border.
fn group(area: NSRect) {
    fill_round(area, RADIUS, &NSColor::quaternarySystemFillColor());
}

fn fill_round(area: NSRect, radius: f64, color: &NSColor) {
    if area.size.width <= 0.0 || area.size.height <= 0.0 {
        return;
    }
    color.setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(area, radius, radius).fill();
}

/// A column with a rounded top and a square foot, in the flipped view.
fn column_path(x: f64, top: f64, width: f64, height: f64, radius: f64, color: &NSColor) {
    let bottom = top + height;
    let path = NSBezierPath::bezierPath();
    path.moveToPoint(NSPoint::new(x, bottom));
    path.lineToPoint(NSPoint::new(x, top + radius));
    path.appendBezierPathWithArcFromPoint_toPoint_radius(
        NSPoint::new(x, top),
        NSPoint::new(x + radius, top),
        radius,
    );
    path.lineToPoint(NSPoint::new(x + width - radius, top));
    path.appendBezierPathWithArcFromPoint_toPoint_radius(
        NSPoint::new(x + width, top),
        NSPoint::new(x + width, top + radius),
        radius,
    );
    path.lineToPoint(NSPoint::new(x + width, bottom));
    path.closePath();
    color.setFill();
    path.fill();
}

fn inset(area: NSRect, by: f64) -> NSRect {
    rect(
        area.origin.x + by,
        area.origin.y + by,
        area.size.width - 2.0 * by,
        area.size.height - 2.0 * by,
    )
}

fn hex_color(hex: u32) -> Retained<NSColor> {
    let channel = |shift: u32| f64::from((hex >> shift) & 0xff) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(channel(16), channel(8), channel(0), 1.0)
}

// --------------------------------------------------------------------------
// Text
// --------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Semibold,
}

impl Weight {
    fn value(self) -> NSFontWeight {
        sl_macos::consts::font_weight(match self {
            Self::Regular => sl_macos::consts::Weight::Regular,
            Self::Semibold => sl_macos::consts::Weight::Semibold,
        })
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

fn font(size: f64, weight: Weight) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, weight.value())
}

/// The system font with digits of one width, so figures line up and do not
/// shift as they change.
fn digits(size: f64, weight: Weight) -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(size, weight.value())
}

fn mono(size: f64) -> Retained<NSFont> {
    NSFont::monospacedSystemFontOfSize_weight(size, Weight::Regular.value())
}

/// The paragraph style for one line in `align`, cut short with an ellipsis.
fn paragraph(align: Align) -> Retained<NSMutableParagraphStyle> {
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    paragraph.setAlignment(match align {
        Align::Left => NSTextAlignment::Left,
        Align::Center => NSTextAlignment::Center,
        Align::Right => NSTextAlignment::Right,
    });
    paragraph
}

/// One line of `text` in `area`, cut short with an ellipsis if it does not
/// fit. `Some(())` so a table's cell painter can report that it drew.
#[expect(
    clippy::unnecessary_wraps,
    reason = "a table's cell painters return whether they drew, and this is one"
)]
fn draw_text(text: &str, area: NSRect, font: &NSFont, color: &NSColor, align: Align) -> Option<()> {
    if area.size.width <= 0.0 {
        return Some(());
    }
    let paragraph = paragraph(align);
    let attrs = sl_macos::text::Attrs {
        font: Some(font),
        color: Some(color),
        paragraph: Some(&paragraph),
        ..Default::default()
    };
    let options = NSStringDrawingOptions::UsesLineFragmentOrigin
        | NSStringDrawingOptions::TruncatesLastVisibleLine;
    sl_macos::text::draw_text(text, area, &attrs, options);
    Some(())
}

fn text_width(text: &str, font: &NSFont) -> f64 {
    let paragraph = paragraph(Align::Left);
    sl_macos::text::text_width(
        text,
        &sl_macos::text::Attrs {
            font: Some(font),
            color: Some(&NSColor::labelColor()),
            paragraph: Some(&paragraph),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(columns: usize) -> StatsModel {
        let mut snapshot = Snapshot::empty(0);
        snapshot.history.truncate(columns);
        StatsModel::build(&snapshot, 90, Tab::Latest, 0)
    }

    #[test]
    fn the_pointer_picks_the_column_under_it_and_nothing_outside_the_plot() {
        let layout = Layout {
            chart: rect(20.0, 100.0, 600.0, 300.0),
            plot: rect(80.0, 140.0, 500.0, 200.0),
            ..Layout::default()
        };
        let count = model(10).columns.len();
        assert_eq!(count, 10);
        assert_eq!(
            column_at(&layout, count, NSPoint::new(80.0, 200.0)),
            Some(0)
        );
        assert_eq!(
            column_at(&layout, count, NSPoint::new(579.0, 200.0)),
            Some(9)
        );
        assert_eq!(
            column_at(&layout, count, NSPoint::new(330.0, 200.0)),
            Some(5)
        );
        assert_eq!(
            column_at(&layout, count, NSPoint::new(79.0, 200.0)),
            None,
            "left of the plot, over the axis"
        );
        assert_eq!(
            column_at(&layout, count, NSPoint::new(330.0, 50.0)),
            None,
            "above the chart"
        );
        assert_eq!(column_at(&layout, 0, NSPoint::new(330.0, 200.0)), None);
    }

    #[test]
    fn rects_overlap_only_when_they_share_area() {
        let a = rect(0.0, 0.0, 10.0, 10.0);
        assert!(intersects(a, rect(5.0, 5.0, 10.0, 10.0)));
        assert!(!intersects(a, rect(10.0, 0.0, 10.0, 10.0)));
    }
}
