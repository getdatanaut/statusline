//! The usage meters: one per rate-limit window, side by side, drawn as bars or
//! as rings ([`MeterStyle`]), with the 5-hour window's spend under both.
//!
//! The same [`LimitsPanel`] is the top of the menu (Regular) and of the
//! popover (Compact); each surface owns its own instance, since a view has one
//! parent.
//!
//! Bars:
//!
//! ```text
//! Session 5h                7% → 33%   Weekly 7d               27% → 53%
//! ██▒▒▒▒▒▒|▒▒░░░░░░░░░░░░░░░░░░░░░░░   ████████▒▒▒▒▒▒▒▒|░░░░░░░░░░░░░░░░░
//! 14% behind pace   resets in 3h 56m   24% behind pace   resets Fri 05:00
//! $21.50 so far · $12.45/hr · $60.10 projected
//! ```
//!
//! The solid fill is what is used, the faint one where it is heading by the
//! reset, the tick where an even pace would be now. Rings draw the same three
//! things around a circle, the numbers beside it.

use std::fmt::Write as _;
use std::cell::RefCell;

use objc2::rc::Retained;
use sl_macos::canvas::{Canvas, CanvasBehavior, ControlCanvas};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSControl, NSEvent, NSLineCapStyle, NSTextField, NSView,
};
use objc2_foundation::{MainThreadMarker, NSMutableAttributedString, NSPoint, NSRect, NSSize, NSString};

use crate::controller::{fill_pill, meter_color, styled};
use crate::sessions::{self, Tone};
use crate::view_prefs::MeterStyle;

/// Left inset, so the text lines up with the menu rows below.
pub(crate) const PANEL_INSET: f64 = 17.0;
pub(crate) const PANEL_RIGHT: f64 = 14.0;
pub(crate) const PANEL_MIN_WIDTH: f64 = 300.0;
/// Least space between a left-hand and a right-hand label on one line.
pub(crate) const PANEL_GAP: f64 = 16.0;
/// The pace, reset, spend and footer lines: a step below the menu font.
pub(crate) const DETAIL_FONT_SIZE: f64 = 12.0;
const PANEL_BOTTOM: f64 = 6.0;

const TITLE_Y: f64 = 4.0;
const BAR_Y: f64 = 25.0;
const BAR_H: f64 = 6.0;
/// The pace tick: thin, and standing proud of the bar on both sides.
const TICK_W: f64 = 2.0;
const TICK_OVERHANG: f64 = 3.0;
const PACE_Y: f64 = 35.0;
const BARS_SPEND_Y: f64 = 60.0;
/// A text field draws its text this far inside its frame; frames are placed
/// this much further out so the text itself meets the bars' ends.
pub(crate) const LABEL_PAD: f64 = 2.0;

const RING_Y: f64 = 6.0;
const RING_DIAMETER: f64 = 56.0;
const RING_WIDTH: f64 = 6.0;
/// Between the ring and the text beside it.
const RING_GAP: f64 = 12.0;
/// The lines beside a ring: title, projection, pace, reset.
const RING_LINES_Y: [f64; 4] = [3.0, 22.0, 38.0, 54.0];
const RING_PERCENT_SIZE: f64 = 14.0;
const RINGS_SPEND_Y: f64 = 76.0;
/// Window names and percentages: the mock's 13 points.
const TITLE_SIZE: f64 = 13.0;

/// One window's meter, as plain data: everything [`WindowPanel::paint`]
/// shows, worked out by the controller so it can be tested without a menu bar.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowModel {
    pub used: f64,
    /// Where usage is heading by the reset, if the burn so far holds.
    pub projected: Option<f64>,
    /// How far through the window the clock is, capped at 100.
    pub elapsed: Option<f64>,
    /// Usage minus elapsed, in percentage points: positive is ahead of pace.
    pub pace: Option<f64>,
    pub tone: Tone,
    pub resets: Option<String>,
    pub spend: Option<String>,
    /// The window has reset since its reading, so `used` is the old window's:
    /// shown as unknown, not as a number about a window that is over.
    pub expired: bool,
}

/// One meter as the panel lays it out: which window, and its numbers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MeterSlot {
    /// `Session`, `Weekly`, `Codex`: in bold.
    pub name: String,
    /// `5h`, `7d`: dimmed beside the name.
    pub span: String,
    pub model: WindowModel,
    /// The reading behind it is old.
    pub dimmed: bool,
}

/// What a [`MeterView`] draws.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Meter {
    used: f64,
    projected: Option<f64>,
    elapsed: Option<f64>,
    /// The usage colour; `None` until the first paint.
    tone: Option<Tone>,
    style: MeterStyle,
}

/// A percentage as a fraction of the track, 0 to 1.
fn share(percent: f64) -> f64 {
    (percent / 100.0).clamp(0.0, 1.0)
}


/// One window's meter: a bar or a ring, drawn by [`MeterLook`].
#[derive(Clone)]
pub(crate) struct MeterView(Canvas<MeterLook>);

/// What a [`MeterView`] holds: the meter it last drew.
pub(crate) struct MeterLook(RefCell<Meter>);

impl CanvasBehavior for MeterLook {
    /// Colours are semantic and resolved here, at draw time, so a switch
    /// between light and dark needs nothing but a redraw.
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let meter = *self.0.borrow();
        match meter.style {
            MeterStyle::Bars => draw_bar(view, meter),
            MeterStyle::Rings => draw_ring(meter),
        }
    }
}

impl std::ops::Deref for MeterView {
    type Target = Canvas<MeterLook>;

    fn deref(&self) -> &Canvas<MeterLook> {
        &self.0
    }
}

impl MeterView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        Canvas::from_view(view).map(Self)
    }

    pub(crate) fn describe(&self) -> String {
        let meter = *self.behavior().0.borrow();
        let tone = meter.tone.map_or_else(|| "unpainted".to_owned(), |tone| format!("{tone:?}"));
        let mut out = format!("meter {:?} {:.0}% {tone}", meter.style, meter.used);
        if let Some(projected) = meter.projected {
            write!(out, " → {projected:.0}%").ok();
        }
        if let Some(elapsed) = meter.elapsed {
            write!(out, " clock {elapsed:.0}%").ok();
        }
        out
    }

    fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        Self(Canvas::new(mtm, frame, MeterLook(RefCell::new(Meter::default()))))
    }

    fn set_meter(&self, meter: Meter) {
        let look = &self.behavior().0;
        if *look.borrow() != meter {
            *look.borrow_mut() = meter;
            self.setNeedsDisplay(true);
        }
    }
}

/// A bar meter across `view`.
fn draw_bar(view: &NSView, meter: Meter) {
    {
        let width = (view.bounds().size.width - PANEL_INSET - PANEL_RIGHT).max(0.0);
        fill_pill(PANEL_INSET, BAR_Y, width, BAR_H, &NSColor::labelColor().colorWithAlphaComponent(0.12));
        if let Some(tone) = meter.tone {
            let color = meter_color(tone);
            if let Some(projected) = meter.projected {
                fill_pill(PANEL_INSET, BAR_Y, width * share(projected), BAR_H, &color.colorWithAlphaComponent(0.3));
            }
            fill_pill(PANEL_INSET, BAR_Y, width * share(meter.used), BAR_H, &color);
        }
        if let Some(elapsed) = meter.elapsed {
            let x = (PANEL_INSET + width * share(elapsed) - TICK_W / 2.0).round();
            NSColor::labelColor().colorWithAlphaComponent(0.85).setFill();
            let tick = NSRect::new(NSPoint::new(x, BAR_Y - TICK_OVERHANG), NSSize::new(TICK_W, BAR_H + 2.0 * TICK_OVERHANG));
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(tick, 1.0, 1.0).fill();
        }
    }
}

/// A point on a circle, `fraction` of the way round clockwise from the top,
/// in a flipped (y-down) view.
fn on_circle(center: NSPoint, radius: f64, fraction: f64) -> NSPoint {
    let angle = std::f64::consts::TAU * fraction;
    NSPoint::new(center.x + radius * angle.sin(), center.y - radius * angle.cos())
}

/// An arc from the top, clockwise, `fraction` of the way round.
fn stroke_arc(center: NSPoint, radius: f64, fraction: f64, color: &NSColor) {
    if fraction <= 0.0 {
        return;
    }
    let path = NSBezierPath::bezierPath();
    // In a flipped view angles grow clockwise, so -90 degrees is the top and
    // a counter-clockwise sweep in AppKit's terms turns clockwise on screen.
    let start = -90.0;
    path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(center, radius, start, start + 360.0 * fraction.min(0.9999), false);
    path.setLineWidth(RING_WIDTH);
    path.setLineCapStyle(if fraction >= 0.9999 { NSLineCapStyle::Butt } else { NSLineCapStyle::Round });
    color.setStroke();
    path.stroke();
}

/// The ring: a faint track, where usage is heading, what is used, and a tick
/// across the ring where an even pace would be.
fn draw_ring(meter: Meter) {
    let center = NSPoint::new(PANEL_INSET + RING_DIAMETER / 2.0, RING_Y + RING_DIAMETER / 2.0);
    let radius = (RING_DIAMETER - RING_WIDTH) / 2.0;
    let track = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(NSPoint::new(center.x - radius, center.y - radius), NSSize::new(2.0 * radius, 2.0 * radius)));
    track.setLineWidth(RING_WIDTH);
    NSColor::labelColor().colorWithAlphaComponent(0.12).setStroke();
    track.stroke();
    if let Some(tone) = meter.tone {
        let color = meter_color(tone);
        if let Some(projected) = meter.projected {
            stroke_arc(center, radius, share(projected), &color.colorWithAlphaComponent(0.3));
        }
        stroke_arc(center, radius, share(meter.used), &color);
    }
    if let Some(elapsed) = meter.elapsed {
        let reach = RING_WIDTH / 2.0 + 2.0;
        let tick = NSBezierPath::bezierPath();
        tick.moveToPoint(on_circle(center, radius - reach, share(elapsed)));
        tick.lineToPoint(on_circle(center, radius + reach, share(elapsed)));
        tick.setLineWidth(TICK_W);
        tick.setLineCapStyle(NSLineCapStyle::Round);
        NSColor::labelColor().colorWithAlphaComponent(0.85).setStroke();
        tick.stroke();
    }
}

/// One rate-limit window: its name, its meter and its numbers. One half of a
/// [`LimitsPanel`]. The labels are built once and only ever updated.
struct WindowPanel {
    view: MeterView,
    title: Retained<NSTextField>,
    /// Bars: `7% → 33%` at the top right. Rings: `7%` inside the ring.
    percent: Retained<NSTextField>,
    /// Rings only: `→ 33% at reset`.
    projection: Retained<NSTextField>,
    pace: Retained<NSTextField>,
    resets: Retained<NSTextField>,
}

impl WindowPanel {
    /// A panel to be titled by [`set_title`](Self::set_title) when painted.
    fn new(mtm: MainThreadMarker, parent: &NSView) -> Self {
        let view = MeterView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_MIN_WIDTH, BARS_SPEND_Y)));
        let label = || {
            let text = NSTextField::labelWithString(&NSString::new(), mtm);
            view.addSubview(&text);
            text
        };
        let (title, percent, projection, pace, resets) = (label(), label(), label(), label(), label());
        view.setHidden(true);
        parent.addSubview(&view);
        Self { view, title, percent, projection, pace, resets }
    }

    /// `name` in bold and `span` (`5h`) beside it, dimmed.
    fn set_title(&self, name: &str, span: &str) {
        let (regular, semibold) = (sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular), sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold));
        let text = NSMutableAttributedString::from_attributed_nsstring(&styled(name, &NSColor::labelColor(), TITLE_SIZE, semibold));
        text.appendAttributedString(&styled(&format!(" {span}"), &crate::controller::dim_color(), TITLE_SIZE, regular));
        self.title.setAttributedStringValue(&text);
    }

    fn labels(&self) -> [&NSTextField; 5] {
        [&self.title, &self.percent, &self.projection, &self.pace, &self.resets]
    }

    /// Set the text and the meter. Returns the width the panel needs and how
    /// far down its last line ends.
    fn paint(&self, model: &WindowModel, style: MeterStyle) -> (f64, f64) {
        let (regular, bold) = (sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular), sl_macos::consts::font_weight(sl_macos::consts::Weight::Bold));
        let secondary = crate::controller::dim_color();
        let used = if model.expired { "—".to_owned() } else { format!("{:.0}%", model.used) };
        match style {
            MeterStyle::Bars => {
                let percent = NSMutableAttributedString::from_attributed_nsstring(&styled(&used, &NSColor::labelColor(), TITLE_SIZE, bold));
                if let Some(projected) = model.projected {
                    percent.appendAttributedString(&styled(&format!(" → {projected:.0}%"), &secondary, TITLE_SIZE, regular));
                }
                self.percent.setAttributedStringValue(&percent);
                self.projection.setStringValue(&NSString::new());
            }
            MeterStyle::Rings => {
                self.percent.setAttributedStringValue(&styled(&used, &NSColor::labelColor(), RING_PERCENT_SIZE, bold));
                let projection = model.projected.map(|projected| format!("→ {projected:.0}% at reset")).unwrap_or_default();
                self.projection.setAttributedStringValue(&styled(&projection, &secondary, DETAIL_FONT_SIZE, regular));
            }
        }
        self.pace.setAttributedStringValue(&styled(&model.pace.map(sessions::pace_text).unwrap_or_default(), &meter_color(model.tone), DETAIL_FONT_SIZE, regular));
        self.resets.setAttributedStringValue(&styled(model.resets.as_deref().unwrap_or_default(), &secondary, DETAIL_FONT_SIZE, regular));
        for label in self.labels() {
            label.sizeToFit();
        }
        let meter = if model.expired {
            Meter { style, ..Meter::default() }
        } else {
            Meter { used: model.used, projected: model.projected, elapsed: model.elapsed, tone: Some(model.tone), style }
        };
        self.view.set_meter(meter);

        let width_of = |label: &NSTextField| label.frame().size.width;
        match style {
            MeterStyle::Bars => {
                let content = (width_of(&self.title) + PANEL_GAP + width_of(&self.percent)).max(width_of(&self.pace) + PANEL_GAP + width_of(&self.resets));
                (PANEL_INSET + content + PANEL_RIGHT, PACE_Y + self.pace.frame().size.height)
            }
            MeterStyle::Rings => {
                let text = [&self.title, &self.projection, &self.pace, &self.resets].into_iter().map(|label| width_of(label)).fold(0.0, f64::max);
                let bottom = (RING_Y + RING_DIAMETER).max(RING_LINES_Y[3] + self.resets.frame().size.height);
                (PANEL_INSET + RING_DIAMETER + RING_GAP + text + PANEL_RIGHT, bottom)
            }
        }
    }

    /// Put the panel at `x`, `width` wide and `height` tall, resizing with its
    /// parent by `mask`. Bars keep their right-hand labels against the right
    /// edge; rings keep everything beside the ring.
    fn place(&self, x: f64, width: f64, height: f64, mask: NSAutoresizingMaskOptions, style: MeterStyle) {
        self.view.setFrame(NSRect::new(NSPoint::new(x, 0.0), NSSize::new(width, height)));
        self.view.setAutoresizingMask(mask);
        let size_of = |label: &NSTextField| label.frame().size;
        let pinned = NSAutoresizingMaskOptions::ViewMinXMargin;
        let free = NSAutoresizingMaskOptions::empty();
        let put = |label: &NSTextField, x: f64, y: f64, mask: NSAutoresizingMaskOptions| {
            label.setFrameOrigin(NSPoint::new(x.round(), y.round()));
            label.setAutoresizingMask(mask);
        };
        match style {
            MeterStyle::Bars => {
                // Text edges, not field edges, on the bar's ends.
                put(&self.title, PANEL_INSET - LABEL_PAD, TITLE_Y, free);
                put(&self.percent, width - PANEL_RIGHT - size_of(&self.percent).width + LABEL_PAD, TITLE_Y, pinned);
                put(&self.pace, PANEL_INSET - LABEL_PAD, PACE_Y, free);
                put(&self.resets, width - PANEL_RIGHT - size_of(&self.resets).width + LABEL_PAD, PACE_Y, pinned);
                self.projection.setHidden(true);
            }
            MeterStyle::Rings => {
                let percent = size_of(&self.percent);
                put(&self.percent, PANEL_INSET + (RING_DIAMETER - percent.width) / 2.0, RING_Y + (RING_DIAMETER - percent.height) / 2.0, free);
                let text_x = PANEL_INSET + RING_DIAMETER + RING_GAP - LABEL_PAD;
                for (label, y) in [&self.title, &self.projection, &self.pace, &self.resets].into_iter().zip(RING_LINES_Y) {
                    put(label, text_x, y, free);
                }
                self.projection.setHidden(false);
            }
        }
        self.view.setHidden(false);
    }
}


/// A plain container whose frames read top-down.
#[derive(Clone)]
pub(crate) struct FlippedView(Canvas<Flipped>);

/// [`FlippedView`]'s behaviour: flipped, and nothing else.
pub(crate) struct Flipped;

impl CanvasBehavior for Flipped {}

impl std::ops::Deref for FlippedView {
    type Target = Canvas<Flipped>;

    fn deref(&self) -> &Canvas<Flipped> {
        &self.0
    }
}

impl FlippedView {
    pub(crate) fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        Self(Canvas::new(mtm, frame, Flipped))
    }
}

/// The most meters one panel shows: Claude Code's two windows and Codex's.
const MAX_METERS: usize = 3;

/// The page marks under the meters: their row, size and spacing. Each page
/// is a tool's limits, marked with its logo: Claude Code's, then Codex's (the
/// controller's `METER_PAGES` order).
const DOTS_HEIGHT: f64 = 20.0;
const DOT_SIZE: f64 = 12.0;
const DOT_GAP: f64 = 12.0;
const PAGE_MARKS: [crate::view_prefs::Source; 2] = [crate::view_prefs::Source::Claude, crate::view_prefs::Source::Codex];

/// What the meters view draws of its own: the page marks.
#[derive(Default)]
pub(crate) struct MetersLook {
    /// `(pages, current)`, or `None` for no marks.
    dots: std::cell::Cell<Option<(usize, usize)>>,
}


/// The meters panel: their own container, which draws the page marks under
/// them and takes clicks on those marks.
#[derive(Clone)]
pub(crate) struct MetersView(ControlCanvas<MetersLook>);

impl std::ops::Deref for MetersView {
    type Target = ControlCanvas<MetersLook>;

    fn deref(&self) -> &ControlCanvas<MetersLook> {
        &self.0
    }
}

impl CanvasBehavior for MetersLook {
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let Some((pages, current)) = self.dots.get() else { return };
        for (i, rect) in dot_rects(view.bounds(), pages).into_iter().enumerate() {
            let color = NSColor::labelColor().colorWithAlphaComponent(if i == current { 0.9 } else { 0.3 });
            if let Some(source) = PAGE_MARKS.get(i) {
                crate::menu_rows::draw_mark_sized(*source, rect, &color);
            } else {
                color.setFill();
                NSBezierPath::bezierPathWithOvalInRect(rect).fill();
            }
        }
    }

    /// A click on a dot shows its page: its index goes on the tag, and the
    /// click is sent.
    fn mouse_down(&self, view: &NSControl, event: &NSEvent) -> bool {
        let Some((pages, current)) = self.dots.get() else { return false };
        let point = view.convertPoint_fromView(event.locationInWindow(), None);
        // Generous targets: the dot's own column, the whole row tall.
        let hit = dot_rects(view.bounds(), pages)
            .into_iter()
            .position(|rect| (point.x - (rect.origin.x + DOT_SIZE / 2.0)).abs() <= DOT_GAP && point.y >= view.bounds().size.height - DOTS_HEIGHT);
        let Some(page) = hit.filter(|page| *page != current) else { return false };
        view.setTag(isize::try_from(page).unwrap_or(isize::MAX));
        true
    }

    fn accepts_first_mouse(&self) -> bool {
        true
    }
}

impl MetersView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        ControlCanvas::from_view(view).map(Self)
    }

    /// `(pages, current)` of the page dots, if there are any.
    pub(crate) fn pages(&self) -> Option<(usize, usize)> {
        self.behavior().dots.get()
    }

    pub(crate) fn describe(&self) -> String {
        match self.behavior().dots.get() {
            Some((pages, current)) => format!("meters page {}/{pages}", current + 1),
            None => "meters".to_owned(),
        }
    }

    fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        Self(ControlCanvas::new(mtm, frame, MetersLook::default()))
    }
}

/// Where each of `pages` dots sits, centred along the bottom of `bounds`.
fn dot_rects(bounds: NSRect, pages: usize) -> Vec<NSRect> {
    let row = pages as f64 * DOT_SIZE + pages.saturating_sub(1) as f64 * DOT_GAP;
    let x = ((bounds.size.width - row) / 2.0).round();
    let y = (bounds.size.height - DOTS_HEIGHT + (DOTS_HEIGHT - DOT_SIZE) / 2.0 - 2.0).round();
    (0..pages).map(|i| NSRect::new(NSPoint::new(x + i as f64 * (DOT_SIZE + DOT_GAP), y), NSSize::new(DOT_SIZE, DOT_SIZE))).collect()
}

/// The windows side by side, each an equal share of the width (all of it,
/// alone), and under them the 5-hour window's spend.
pub(crate) struct LimitsPanel {
    pub(crate) view: MetersView,
    /// Retitled every paint: which windows show depends on the provider.
    panels: Vec<WindowPanel>,
    spend: Retained<NSTextField>,
    /// What was last painted, so an unchanged tick does no AppKit work.
    last: RefCell<Option<PanelKey>>,
}

/// What the meters panel was painted from: an equal one paints the same.
#[derive(PartialEq)]
struct PanelKey {
    slots: Vec<SlotKey>,
    dots: Option<(usize, usize)>,
    style: MeterStyle,
    min_width: f64,
}

/// What one meter draws: its texts exactly as shown, and its fractions to a
/// tenth of a percent -- under half a point of the widest bar, or of a ring's
/// arc, so a bar or tick redraws at most that late. Keyed on the model's own
/// numbers, the clock's tick would repaint the panel every second; this way
/// a 5-hour window's moves the key every 18 s, a week's every 10 minutes.
#[derive(Debug, PartialEq)]
struct SlotKey {
    name: String,
    span: String,
    dimmed: bool,
    tone: Tone,
    expired: bool,
    /// The used, projected and pace texts.
    texts: [String; 3],
    resets: Option<String>,
    spend: Option<String>,
    /// Used, projected and elapsed, in tenths of a percent.
    at: [Option<i64>; 3],
}

impl SlotKey {
    fn of(slot: &MeterSlot) -> Self {
        let model = &slot.model;
        let tenths = |percent: f64| sl_core::num::trunc_i64((percent * 10.0).round());
        let shown = |percent: Option<f64>| percent.map(|percent| format!("{percent:.0}")).unwrap_or_default();
        Self {
            name: slot.name.clone(),
            span: slot.span.clone(),
            dimmed: slot.dimmed,
            tone: model.tone,
            expired: model.expired,
            texts: [shown(Some(model.used)), shown(model.projected), model.pace.map(sessions::pace_text).unwrap_or_default()],
            resets: model.resets.clone(),
            spend: model.spend.clone(),
            at: [Some(tenths(model.used)), model.projected.map(tenths), model.elapsed.map(tenths)],
        }
    }
}

impl LimitsPanel {
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let view = MetersView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_MIN_WIDTH, BARS_SPEND_Y)));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let panels = (0..MAX_METERS).map(|_| WindowPanel::new(mtm, &view)).collect();
        let spend = NSTextField::labelWithString(&NSString::new(), mtm);
        view.addSubview(&spend);
        // A click on a page dot asks for that page, its index as the tag.
        crate::act::bind(&view, crate::act::Act::ShowMeterPage);
        Self { view, panels, spend, last: RefCell::new(None) }
    }

    /// Show `slots` side by side, each dimmed while its reading is old, and
    /// under them `dots` -- `(pages, current)` -- when there is more than one
    /// page to show. The panel sizes itself to what it needs, at least
    /// `min_width` wide. False when there is nothing to show at all, so the
    /// caller can hide it.
    pub(crate) fn paint(&self, slots: &[MeterSlot], dots: Option<(usize, usize)>, style: MeterStyle, min_width: f64) -> bool {
        let slots = &slots[..slots.len().min(MAX_METERS)];
        if slots.is_empty() && dots.is_none() {
            return false;
        }
        let key = PanelKey { slots: slots.iter().map(SlotKey::of).collect(), dots, style, min_width };
        if self.last.borrow().as_ref() == Some(&key) {
            return true;
        }

        let mut shown: Vec<(&WindowPanel, f64, f64)> = Vec::new();
        for (i, panel) in self.panels.iter().enumerate() {
            match slots.get(i) {
                Some(slot) => {
                    panel.set_title(&slot.name, &slot.span);
                    let (width, bottom) = panel.paint(&slot.model, style);
                    panel.view.setAlphaValue(if slot.dimmed { 0.5 } else { 1.0 });
                    shown.push((panel, width, bottom));
                }
                None => panel.view.setHidden(true),
            }
        }

        let regular = sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular);
        let secondary = crate::controller::dim_color();
        let spend = slots.iter().find_map(|slot| slot.model.spend.as_deref());
        self.spend.setAttributedStringValue(&styled(spend.unwrap_or_default(), &secondary, DETAIL_FONT_SIZE, regular));
        self.spend.sizeToFit();
        let has_line = spend.is_some();
        let line = if has_line { PANEL_INSET + self.spend.frame().size.width + PANEL_RIGHT } else { 0.0 };

        let spend_y = match style {
            MeterStyle::Bars => BARS_SPEND_Y,
            MeterStyle::Rings => RINGS_SPEND_Y,
        };
        let columns = shown.len().max(1) as f64;
        let column_need = shown.iter().map(|(_, width, _)| *width).fold(0.0, f64::max);
        let width = (column_need * columns).max(line).max(min_width);
        let panels_bottom = shown.iter().map(|(_, _, bottom)| *bottom).fold(0.0, f64::max) + PANEL_BOTTOM;
        let line_height = self.spend.frame().size.height;
        let height = if has_line { spend_y + line_height + PANEL_BOTTOM } else { panels_bottom };
        let height = height + if dots.is_some() { DOTS_HEIGHT } else { 0.0 };
        self.view.setFrameSize(NSSize::new(width, height));
        self.view.behavior().dots.set(dots);
        NSView::setNeedsDisplay(&self.view, true);

        // Placed after the resize, against the width the view actually has.
        let width = self.view.frame().size.width;
        let column = (width / columns).floor();
        let last = shown.len().saturating_sub(1);
        for (i, (panel, _, _)) in shown.iter().enumerate() {
            // Shares stay shares as the menu stretches the row: each column
            // shares the growth with the margins on its far sides.
            let mask = match (shown.len(), i) {
                (1, _) => NSAutoresizingMaskOptions::ViewWidthSizable,
                (_, 0) => NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxXMargin,
                (_, i) if i == last => NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinXMargin,
                _ => NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin,
            };
            let x = column * i as f64;
            let w = if i == last { width - x } else { column };
            panel.place(x, w, panels_bottom.min(spend_y), mask, style);
        }
        self.spend.setFrameOrigin(NSPoint::new(PANEL_INSET - LABEL_PAD, spend_y));
        self.spend.setHidden(spend.is_none());

        *self.last.borrow_mut() = Some(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(used: f64, elapsed: f64) -> MeterSlot {
        let model = WindowModel { used, projected: Some(used * 2.0), elapsed: Some(elapsed), pace: Some(used - elapsed), tone: Tone::Green, resets: Some("resets in 3h".into()), spend: None, expired: false };
        MeterSlot { name: "Session".into(), span: "5h".into(), model, dimmed: false }
    }

    /// A clock that moved less than the meter can show leaves the key as it
    /// was; anything drawn differently changes it.
    #[test]
    fn a_meter_is_keyed_on_what_it_draws() {
        let key = |slot: &MeterSlot| SlotKey::of(slot);
        // One second of a five-hour window: 0.0056 %.
        assert_eq!(key(&slot(40.2, 50.0)), key(&slot(40.2, 50.0 + 100.0 / 18_000.0)), "the tick has not moved");
        assert_ne!(key(&slot(40.2, 50.0)), key(&slot(40.2, 50.1)), "a tenth of a percent: it may have");
        assert_ne!(key(&slot(40.2, 50.0)), key(&slot(40.3, 50.0)), "the bar's length");
        // Inside one tenth, across the point where the text rounds up.
        let (below, above) = (slot(40.49, 50.0), slot(40.5001, 50.0));
        assert_eq!(key(&below).at, key(&above).at);
        assert_ne!(key(&below), key(&above), "the text changed: 40% -> 41%");
        let mut dimmed = slot(40.2, 50.0);
        dimmed.dimmed = true;
        assert_ne!(key(&slot(40.2, 50.0)), key(&dimmed));
        let mut later = slot(40.2, 50.0);
        later.model.resets = Some("resets in 2h 59m".into());
        assert_ne!(key(&slot(40.2, 50.0)), key(&later), "the reset line");
    }

    #[test]
    fn points_on_the_ring_go_clockwise_from_the_top_in_a_flipped_view() {
        let center = NSPoint::new(10.0, 10.0);
        let at = |fraction: f64| {
            let p = on_circle(center, 5.0, fraction);
            ((p.x * 1000.0).round() / 1000.0, (p.y * 1000.0).round() / 1000.0)
        };
        assert_eq!(at(0.0), (10.0, 5.0), "top: y grows downwards");
        assert_eq!(at(0.25), (15.0, 10.0), "a quarter round is the right-hand side");
        assert_eq!(at(0.5), (10.0, 15.0));
    }
}
