//! The Regular menu's custom rows, drawn as views so they can lay out the way
//! a text title cannot:
//!
//! ```text
//! NEEDS YOU · 2
//! ( ◔ )  DN-479 / M4                                      7m left  ›
//!        datanaut · jakub/dn-479-ontology-m4-base · idle 52m
//! ( ❄ )  Cold · 3                                                  ›
//!        DN-478 / M3.4 #3 · DN-531 / M4 · DN-565 — caches expired
//! ```
//!
//! A view in a menu item draws its own highlight: [`RowView`] asks its item
//! whether it is highlighted each time it draws, and recolours its text to
//! match.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;

use objc2::rc::Retained;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSControl, NSEvent, NSImage, NSLineBreakMode,
    NSTextField, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSMutableAttributedString, NSPoint, NSRect, NSSize, NSString,
};

use crate::controller::{context_ring_sized, styled, tone_color};
use crate::meters::DETAIL_FONT_SIZE;
use crate::sessions::{Icon, Seg, SegSize, Tone};
use crate::view_prefs::Source;
use sl_macos::canvas::{Canvas, CanvasBehavior, ControlCanvas};

/// A row's natural width; the menu stretches every row to its widest.
pub(crate) const ROW_WIDTH: f64 = 520.0;
const ROW_HEIGHT: f64 = 44.0;
/// Where the icon's centre sits, and where the text starts after it.
const ICON_CENTER_X: f64 = 28.0;
const ICON_SIZE: f64 = 26.0;
const TEXT_X: f64 = 50.0;
/// Room kept at the right for the chevron.
const CHEVRON_ROOM: f64 = 30.0;
/// The highlight's inset from the menu's edges, as the system's own.
const HIGHLIGHT_INSET: f64 = 5.0;
/// Row titles: the mock's 13 points, regular, a step under the menu font.
const TEXT_SIZE: f64 = 13.0;
/// A provider's mark beside the right-hand text, and the gap between them.
pub(crate) const MARK_SIZE: f64 = 13.0;
const MARK_GAP: f64 = 6.0;

/// What a row shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct RowContent {
    pub icon: Option<Icon>,
    /// A ring round the icon: how full the session's context is, as filled
    /// steps and a colour. The icon then shrinks to sit inside it.
    pub ring: Option<(u8, Tone)>,
    pub title: Vec<Seg>,
    pub detail: Vec<Seg>,
    /// Right-hand column, before the chevron.
    pub right: Option<(String, Tone)>,
    /// Whose session it is, as a mark just before `right`: shown where Claude
    /// Code's and Codex's sessions are listed together.
    pub mark: Option<Source>,
    /// `Some(true)`: a chevron pointing down (an open fold), `Some(false)`:
    /// right (a submenu, or a closed fold).
    pub chevron: Option<bool>,
    /// A faint fill behind the row, for one that stands for several.
    pub tinted: bool,
    /// The title in the secondary text colour, for a row about something no
    /// longer running. Still clickable.
    pub dimmed: bool,
}

pub(crate) struct RowParts {
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    right: Retained<NSTextField>,
    content: RefCell<RowContent>,
    /// The highlight the labels were last coloured for.
    highlighted: Cell<Option<bool>>,
    /// For previews, which have no menu item to ask.
    force_highlight: Cell<bool>,
}

/// A row of the Regular menu: status icon, title, detail line, right-hand text, chevron.
#[derive(Clone)]
pub(crate) struct RowView(ControlCanvas<RowParts>);

impl std::ops::Deref for RowView {
    type Target = ControlCanvas<RowParts>;

    fn deref(&self) -> &ControlCanvas<RowParts> {
        &self.0
    }
}

impl RowView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        ControlCanvas::from_view(view).map(Self)
    }

    fn ivars(&self) -> &RowParts {
        self.behavior()
    }
}

impl CanvasBehavior for RowParts {
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let highlighted = self.force_highlight.get()
            || view
                .enclosingMenuItem()
                .is_some_and(|item| item.isHighlighted());
        if self.highlighted.replace(Some(highlighted)) != Some(highlighted) {
            self.colour_labels(highlighted);
        }
        let bounds = view.bounds();
        let content = self.content.borrow();
        let plate = NSRect::new(
            NSPoint::new(HIGHLIGHT_INSET, 1.0),
            NSSize::new(
                bounds.size.width - 2.0 * HIGHLIGHT_INSET,
                bounds.size.height - 2.0,
            ),
        );
        if highlighted {
            NSColor::selectedContentBackgroundColor().setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(plate, 5.0, 5.0).fill();
        } else if content.tinted {
            NSColor::labelColor()
                .colorWithAlphaComponent(0.05)
                .setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(plate, 5.0, 5.0).fill();
        }
        draw_status(
            content.icon,
            content.ring,
            NSPoint::new(ICON_CENTER_X, bounds.size.height / 2.0),
            highlighted,
        );
        if let Some(source) = content.mark {
            let color = if highlighted {
                NSColor::selectedMenuItemTextColor()
            } else {
                crate::controller::dim_color()
            };
            let x = mark_x(bounds.size.width, &self.right, content.right.is_some());
            draw_mark(
                source,
                NSPoint::new(x, ((bounds.size.height - MARK_SIZE) / 2.0).round()),
                &color,
            );
        }
        if let Some(down) = content.chevron {
            let color = if highlighted {
                NSColor::selectedMenuItemTextColor()
            } else {
                crate::controller::dim_color()
            };
            draw_chevron(
                down,
                NSPoint::new(bounds.size.width - 18.0, bounds.size.height / 2.0),
                &color,
            );
        }
    }

    /// A row with an action (the Cold fold) sends it; a session row has
    /// none, and its submenu opens on hover.
    fn mouse_down(&self, _view: &NSControl, _event: &NSEvent) -> bool {
        true
    }

    fn accepts_first_mouse(&self) -> bool {
        true
    }
}

impl RowView {
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let label = || {
            let text = NSTextField::labelWithString(&NSString::new(), mtm);
            text.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            text
        };
        let parts = RowParts {
            title: label(),
            detail: label(),
            right: label(),
            content: RefCell::new(RowContent::default()),
            highlighted: Cell::new(None),
            force_highlight: Cell::new(false),
        };
        let this = Self(ControlCanvas::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ROW_WIDTH, ROW_HEIGHT)),
            parts,
        ));
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let parts = this.ivars();
        parts
            .title
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        parts
            .detail
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        parts
            .right
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        for label in [&parts.title, &parts.detail, &parts.right] {
            this.addSubview(label);
        }
        this
    }

    /// For previews: draw as if the pointer were on the row.
    pub(crate) fn force_highlight(&self) {
        self.ivars().force_highlight.set(true);
    }

    /// What the row shows, as one line of text (see [`crate::inspect`]).
    pub(crate) fn describe(&self) -> String {
        crate::inspect::row(&self.ivars().content.borrow())
    }

    /// The row's title, to name it by.
    pub(crate) fn label(&self) -> String {
        self.ivars()
            .content
            .borrow()
            .title
            .iter()
            .map(|seg| seg.text.as_str())
            .collect()
    }

    /// Show `content`; nothing happens if it is what is already shown.
    pub(crate) fn set(&self, content: RowContent) {
        if *self.ivars().content.borrow() == content {
            return;
        }
        *self.ivars().content.borrow_mut() = content;
        let highlighted = self.ivars().highlighted.get().unwrap_or(false);
        self.ivars().colour_labels(highlighted);
        self.layout_labels();
        NSView::setNeedsDisplay(self, true);
    }

    fn layout_labels(&self) {
        let parts = self.ivars();
        let width = self.bounds().size.width;
        parts.right.sizeToFit();
        let right = parts.right.frame().size;
        let right_x = width - CHEVRON_ROOM - right.width;
        parts.right.setFrameOrigin(NSPoint::new(
            right_x,
            ((ROW_HEIGHT - right.height) / 2.0).round(),
        ));
        let content = parts.content.borrow();
        let column_x = if content.mark.is_some() {
            mark_x(width, &parts.right, content.right.is_some())
        } else {
            right_x
        };
        drop(content);
        let text_width = (column_x - 12.0 - TEXT_X).max(60.0);
        let has_detail = !parts.content.borrow().detail.is_empty();
        for (label, y) in [
            (&parts.title, if has_detail { 5.0 } else { 13.0 }),
            (&parts.detail, 24.0),
        ] {
            label.sizeToFit();
            let height = label.frame().size.height;
            label.setFrame(NSRect::new(
                NSPoint::new(TEXT_X, y),
                NSSize::new(text_width, height),
            ));
        }
        parts.detail.setHidden(!has_detail);
    }
}

impl RowParts {
    /// Set the labels' text, in their own colours or, under the highlight,
    /// all in the highlight's text colour.
    fn colour_labels(&self, highlighted: bool) {
        let parts = self;
        let content = parts.content.borrow();
        let regular = sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular);
        let colour = |tone: Tone| {
            if highlighted {
                NSColor::selectedMenuItemTextColor()
            } else {
                tone_color(tone)
            }
        };
        let line = |segs: &[Seg], size: f64, weight| {
            let text = NSMutableAttributedString::new();
            for seg in segs {
                let size = if seg.size == SegSize::Smaller {
                    size - 1.5
                } else {
                    size
                };
                text.appendAttributedString(&styled(&seg.text, &colour(seg.tone), size, weight));
            }
            text
        };
        let title = if content.dimmed {
            // Dim's colour, every segment at the title's own size.
            let text = NSMutableAttributedString::new();
            for seg in &content.title {
                let tone = if seg.tone == Tone::Plain {
                    Tone::Dim
                } else {
                    seg.tone
                };
                text.appendAttributedString(&styled(&seg.text, &colour(tone), TEXT_SIZE, regular));
            }
            text
        } else {
            line(&content.title, TEXT_SIZE, regular)
        };
        parts.title.setAttributedStringValue(&title);
        parts
            .detail
            .setAttributedStringValue(&line(&content.detail, DETAIL_FONT_SIZE, regular));
        let (right, tone) = content
            .right
            .clone()
            .unwrap_or_else(|| (String::new(), Tone::Dim));
        parts.right.setAttributedStringValue(&styled(
            &right,
            &colour(tone),
            DETAIL_FONT_SIZE,
            regular,
        ));
    }
}

/// Where a row's provider mark starts: just left of the right-hand text, or
/// where that text would end when there is none.
fn mark_x(width: f64, right: &NSTextField, has_right: bool) -> f64 {
    let edge = if has_right {
        right.frame().origin.x - MARK_GAP + crate::meters::LABEL_PAD
    } else {
        width - CHEVRON_ROOM
    };
    (edge - MARK_SIZE).round()
}

/// `source`'s mark, [`MARK_SIZE`] square, with its top-left at `origin`.
pub(crate) fn draw_mark(source: Source, origin: NSPoint, color: &NSColor) {
    draw_mark_sized(
        source,
        NSRect::new(origin, NSSize::new(MARK_SIZE, MARK_SIZE)),
        color,
    );
}

/// `source`'s mark filling `rect`, in a flipped view.
pub(crate) fn draw_mark_sized(source: Source, rect: NSRect, color: &NSColor) {
    let Some(image) = crate::provider_marks::image(source, rect.size.width, color) else {
        return;
    };
    // `respectFlipped`: the row is flipped, the image draws itself the right
    // way up in it.
    sl_macos::draw::draw_image(&image, rect);
}

/// A status icon centred on `center`: a cache ring at full size, any other
/// symbol a little smaller, as the mock has them. Under the highlight a calm
/// ring turns white: its blue would vanish into the highlight's.
fn draw_icon(icon: Icon, center: NSPoint, highlighted: bool) {
    // Line icons, as the mock draws them, for the states it shows.
    match icon {
        Icon::Busy => return draw_spinner(center, highlighted),
        Icon::Failed => return draw_failed(center),
        _ => {}
    }
    let line = |name: &str, colour: Retained<NSColor>| {
        let colour = if highlighted {
            NSColor::selectedMenuItemTextColor()
        } else {
            colour
        };
        crate::controller::symbol_image_sized_pub(name, &colour, 15.0)
    };
    let image: Option<Retained<NSImage>> = match icon {
        // Brown, as the menu bar's own cup: coffee, not the accent.
        Icon::Caffeinated => line("cup.and.saucer", NSColor::systemBrownColor()),
        Icon::Cold => line("snowflake", NSColor::systemCyanColor()),
        Icon::Idle => line("moon", crate::controller::dim_color()),
        Icon::Closed => line("clock.arrow.circlepath", crate::controller::dim_color()),
        Icon::Stack => line("square.stack.3d.up", crate::controller::dim_color()),
        Icon::Expiring => line("hourglass", crate::controller::meter_color(Tone::Orange)),
        Icon::Busy | Icon::Failed => None,
    };
    let Some(image) = image else { return };
    let size = image.size();
    let rect = NSRect::new(
        NSPoint::new(
            (center.x - size.width / 2.0).round(),
            (center.y - size.height / 2.0).round(),
        ),
        size,
    );
    // `respectFlipped`: the row is flipped, the image is not.
    sl_macos::draw::draw_image(&image, rect);
}

/// A session's status, centred on `center`: its context ring with the state
/// glyph inside, or the state icon alone before there is a reading. Shared by
/// the menu's rows and the popover's cards, so the two look alike.
pub(crate) fn draw_status(
    icon: Option<Icon>,
    ring: Option<(u8, Tone)>,
    center: NSPoint,
    highlighted: bool,
) {
    match (ring, icon) {
        (Some((used, tone)), icon) => {
            draw_image_centered(
                &cache_ring_or_context(used, tone, highlighted),
                center,
                NSSize::new(ICON_SIZE, ICON_SIZE),
            );
            if let Some(icon) = icon {
                draw_glyph_in_ring(icon, center, highlighted);
            }
        }
        (None, Some(icon)) => draw_icon(icon, center, highlighted),
        (None, None) => {}
    }
}

/// [`draw_status`] as a view of its own, [`STATUS_SIZE`] square.
pub(crate) struct StatusLook {
    icon: Cell<Option<Icon>>,
    ring: Cell<Option<(u8, Tone)>>,
}

/// The side of a [`StatusView`].
pub(crate) const STATUS_SIZE: f64 = ICON_SIZE;

/// A session's status on its own: the icon in its context ring.
#[derive(Clone)]
pub(crate) struct StatusView(Canvas<StatusLook>);

impl std::ops::Deref for StatusView {
    type Target = Canvas<StatusLook>;

    fn deref(&self) -> &Canvas<StatusLook> {
        &self.0
    }
}

impl StatusView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        Canvas::from_view(view).map(Self)
    }

    fn ivars(&self) -> &StatusLook {
        self.behavior()
    }
}

impl CanvasBehavior for StatusLook {
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let bounds = view.bounds();
        draw_status(
            self.icon.get(),
            self.ring.get(),
            NSPoint::new(bounds.size.width / 2.0, bounds.size.height / 2.0),
            false,
        );
    }
}

impl StatusView {
    pub(crate) fn describe(&self) -> String {
        let look = self.ivars();
        let mut out = String::from("status");
        if let Some(icon) = look.icon.get() {
            write!(out, " [{icon:?}]").ok();
        }
        if let Some((used, tone)) = look.ring.get() {
            write!(out, " ring {used}/{tone:?}").ok();
        }
        out
    }

    pub(crate) fn new(mtm: MainThreadMarker, icon: Option<Icon>, ring: Option<(u8, Tone)>) -> Self {
        Self(Canvas::new(
            mtm,
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(STATUS_SIZE, STATUS_SIZE),
            ),
            StatusLook {
                icon: Cell::new(icon),
                ring: Cell::new(ring),
            },
        ))
    }
}

/// The context ring, white under the highlight when it would otherwise be
/// the highlight's own blue.
fn cache_ring_or_context(used: u8, tone: Tone, highlighted: bool) -> Retained<NSImage> {
    context_ring_sized(
        used,
        tone,
        ICON_SIZE,
        2.6,
        highlighted && tone == Tone::Plain,
    )
}

fn draw_image_centered(image: &NSImage, center: NSPoint, size: NSSize) {
    let rect = NSRect::new(
        NSPoint::new(
            (center.x - size.width / 2.0).round(),
            (center.y - size.height / 2.0).round(),
        ),
        size,
    );
    sl_macos::draw::draw_image(image, rect);
}

/// A state glyph small enough to sit inside the context ring.
fn draw_glyph_in_ring(icon: Icon, center: NSPoint, highlighted: bool) {
    let (name, colour) = match icon {
        Icon::Busy => ("bolt.fill", NSColor::systemBlueColor()),
        Icon::Caffeinated => ("cup.and.saucer.fill", NSColor::systemBrownColor()),
        Icon::Expiring => ("hourglass", crate::controller::meter_color(Tone::Orange)),
        Icon::Cold => ("snowflake", NSColor::systemCyanColor()),
        Icon::Failed => ("xmark", crate::controller::meter_color(Tone::Red)),
        _ => ("moon.fill", crate::controller::dim_color()),
    };
    let colour = if highlighted {
        NSColor::selectedMenuItemTextColor()
    } else {
        colour
    };
    if let Some(image) = crate::controller::symbol_image_sized_pub(name, &colour, 14.0) {
        // Fitted to one box, so a wide glyph (the cup) is no bigger than a
        // tall one (the moon) and none crowds the ring.
        const BOX: f64 = 13.0;
        let natural = image.size();
        let scale = (BOX / natural.width.max(natural.height).max(1.0)).min(1.0);
        draw_image_centered(
            &image,
            center,
            NSSize::new(natural.width * scale, natural.height * scale),
        );
    }
}

/// Working: a quarter arc on a faint ring, as a spinner at rest.
fn draw_spinner(center: NSPoint, highlighted: bool) {
    let radius = 9.0;
    let colour = if highlighted {
        NSColor::whiteColor()
    } else {
        NSColor::systemBlueColor()
    };
    let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(center.x - radius, center.y - radius),
        NSSize::new(2.0 * radius, 2.0 * radius),
    ));
    ring.setLineWidth(1.8);
    colour.colorWithAlphaComponent(0.3).setStroke();
    ring.stroke();
    let arc = NSBezierPath::bezierPath();
    // Flipped: -90 degrees is the top, and angles grow clockwise.
    arc.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
        center, radius, -90.0, 0.0, false,
    );
    arc.setLineWidth(1.8);
    arc.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
    colour.setStroke();
    arc.stroke();
}

/// Failed CI: a red cross in a faint red disc.
fn draw_failed(center: NSPoint) {
    let radius = 10.0;
    let red = crate::controller::meter_color(Tone::Red);
    red.colorWithAlphaComponent(0.18).setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(center.x - radius, center.y - radius),
        NSSize::new(2.0 * radius, 2.0 * radius),
    ))
    .fill();
    let cross = NSBezierPath::bezierPath();
    let arm = 3.5;
    cross.moveToPoint(NSPoint::new(center.x - arm, center.y - arm));
    cross.lineToPoint(NSPoint::new(center.x + arm, center.y + arm));
    cross.moveToPoint(NSPoint::new(center.x + arm, center.y - arm));
    cross.lineToPoint(NSPoint::new(center.x - arm, center.y + arm));
    cross.setLineWidth(1.8);
    cross.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
    red.setStroke();
    cross.stroke();
}

fn draw_chevron(down: bool, center: NSPoint, color: &NSColor) {
    let path = NSBezierPath::bezierPath();
    let (a, b, c) = if down {
        (
            NSPoint::new(center.x - 4.5, center.y - 2.0),
            NSPoint::new(center.x, center.y + 2.5),
            NSPoint::new(center.x + 4.5, center.y - 2.0),
        )
    } else {
        (
            NSPoint::new(center.x - 1.5, center.y - 3.5),
            NSPoint::new(center.x + 2.0, center.y),
            NSPoint::new(center.x - 1.5, center.y + 3.5),
        )
    };
    path.moveToPoint(a);
    path.lineToPoint(b);
    path.lineToPoint(c);
    path.setLineWidth(1.4);
    path.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
    path.setLineJoinStyle(objc2_app_kit::NSLineJoinStyle::Round);
    color.setStroke();
    path.stroke();
}

/// A group's caption: `NEEDS YOU · 2`, small and spaced, orange for the
/// group that wants attention.
pub(crate) fn caption_text(
    label: &str,
    count: usize,
    urgent: bool,
) -> Retained<objc2_foundation::NSAttributedString> {
    let semibold = sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold);
    let colour = if urgent {
        tone_color(Tone::Orange)
    } else {
        crate::controller::dim_color()
    };
    let text = NSMutableAttributedString::from_attributed_nsstring(&styled(
        &format!("{} · {count}", label.to_uppercase()),
        &colour,
        11.0,
        semibold,
    ));
    // Spaced out a little, as small capitals are.
    sl_macos::text::add_kern(&text, 0.7);
    Retained::into_super(text)
}

// --------------------------------------------------------------------------
// Submenus
// --------------------------------------------------------------------------

/// Height of an action row, as the system's own menu items.
const ACTION_HEIGHT: f64 = 26.0;
/// A submenu's natural width.
pub(crate) const SUBMENU_WIDTH: f64 = 320.0;
const ACTION_ICON_X: f64 = 24.0;
const ACTION_TEXT_X: f64 = 40.0;

/// What an action row shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ActionContent {
    /// An SF Symbol before the title.
    pub symbol: Option<&'static str>,
    /// The symbol's colour; `None` for the text's.
    pub symbol_tone: Option<Tone>,
    pub title: Vec<Seg>,
    /// Opens a submenu of its own.
    pub chevron: bool,
    pub enabled: bool,
}

pub(crate) struct ActionParts {
    title: Retained<NSTextField>,
    content: RefCell<ActionContent>,
    highlighted: Cell<Option<bool>>,
    force_highlight: Cell<bool>,
}

/// An action row of a submenu: a symbol and a title, run on a click.
#[derive(Clone)]
pub(crate) struct ActionView(ControlCanvas<ActionParts>);

impl std::ops::Deref for ActionView {
    type Target = ControlCanvas<ActionParts>;

    fn deref(&self) -> &ControlCanvas<ActionParts> {
        &self.0
    }
}

impl ActionView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        ControlCanvas::from_view(view).map(Self)
    }

    fn ivars(&self) -> &ActionParts {
        self.behavior()
    }
}

impl CanvasBehavior for ActionParts {
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let parts = self;
        let enabled = parts.content.borrow().enabled;
        let highlighted = enabled
            && (parts.force_highlight.get()
                || view
                    .enclosingMenuItem()
                    .is_some_and(|item| item.isHighlighted()));
        if parts.highlighted.replace(Some(highlighted)) != Some(highlighted) {
            self.colour_title(highlighted);
        }
        let bounds = view.bounds();
        if highlighted {
            let plate = NSRect::new(
                NSPoint::new(HIGHLIGHT_INSET, 0.0),
                NSSize::new(
                    bounds.size.width - 2.0 * HIGHLIGHT_INSET,
                    bounds.size.height,
                ),
            );
            NSColor::selectedContentBackgroundColor().setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(plate, 5.0, 5.0).fill();
        }
        let content = parts.content.borrow();
        let text_colour = if highlighted {
            NSColor::selectedMenuItemTextColor()
        } else if enabled {
            NSColor::labelColor()
        } else {
            NSColor::tertiaryLabelColor()
        };
        if let Some(symbol) = content.symbol {
            let colour = match content.symbol_tone {
                Some(tone) if !highlighted => tone_color(tone),
                _ => text_colour.clone(),
            };
            if let Some(image) = crate::controller::symbol_image_sized_pub(symbol, &colour, 12.5) {
                let size = image.size();
                let rect = NSRect::new(
                    NSPoint::new(
                        (ACTION_ICON_X - size.width / 2.0).round(),
                        ((bounds.size.height - size.height) / 2.0).round(),
                    ),
                    size,
                );
                sl_macos::draw::draw_image(&image, rect);
            }
        }
        if content.chevron {
            let colour = if highlighted {
                NSColor::selectedMenuItemTextColor()
            } else {
                crate::controller::dim_color()
            };
            draw_chevron(
                false,
                NSPoint::new(bounds.size.width - 16.0, bounds.size.height / 2.0),
                &colour,
            );
        }
    }

    /// A click runs the row's action, as a click on a menu item does.
    fn mouse_up(&self, _view: &NSControl, _event: &NSEvent) -> bool {
        self.content.borrow().enabled
    }

    fn accepts_first_mouse(&self) -> bool {
        true
    }
}

impl ActionView {
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let title = NSTextField::labelWithString(&NSString::new(), mtm);
        title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let parts = ActionParts {
            title,
            content: RefCell::new(ActionContent {
                enabled: true,
                ..ActionContent::default()
            }),
            highlighted: Cell::new(None),
            force_highlight: Cell::new(false),
        };
        let this = Self(ControlCanvas::new(
            mtm,
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(SUBMENU_WIDTH, ACTION_HEIGHT),
            ),
            parts,
        ));
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        this.ivars()
            .title
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        this.addSubview(&this.ivars().title);
        this
    }

    pub(crate) fn force_highlight(&self) {
        self.ivars().force_highlight.set(true);
    }

    pub(crate) fn describe(&self) -> String {
        crate::inspect::action(&self.ivars().content.borrow())
    }

    /// The row's title, to name it by.
    pub(crate) fn label(&self) -> String {
        self.ivars()
            .content
            .borrow()
            .title
            .iter()
            .map(|seg| seg.text.as_str())
            .collect()
    }

    /// Whether a click on it does anything.
    pub(crate) fn is_enabled(&self) -> bool {
        self.ivars().content.borrow().enabled
    }

    /// For previews: whether the row reads `title`.
    pub(crate) fn is_titled(&self, title: &str) -> bool {
        self.ivars()
            .content
            .borrow()
            .title
            .iter()
            .map(|seg| seg.text.as_str())
            .collect::<String>()
            == title
    }

    pub(crate) fn set(&self, content: ActionContent) {
        if *self.ivars().content.borrow() == content {
            return;
        }
        *self.ivars().content.borrow_mut() = content;
        self.ivars()
            .colour_title(self.ivars().highlighted.get().unwrap_or(false));
        let title = &self.ivars().title;
        title.sizeToFit();
        let height = title.frame().size.height;
        let right = if self.ivars().content.borrow().chevron {
            30.0
        } else {
            14.0
        };
        // One text column for every row of a submenu, icon or none.
        let x = ACTION_TEXT_X;
        // Wide enough for the whole title: the menu takes its width from
        // its widest row.
        let needed = (x + title.frame().size.width + right).ceil();
        if needed > self.frame().size.width {
            self.setFrameSize(NSSize::new(needed, ACTION_HEIGHT));
        }
        title.setFrame(NSRect::new(
            NSPoint::new(x, ((ACTION_HEIGHT - height) / 2.0).round()),
            NSSize::new((self.bounds().size.width - x - right).max(40.0), height),
        ));
        NSView::setNeedsDisplay(self, true);
    }
}

impl ActionParts {
    fn colour_title(&self, highlighted: bool) {
        let parts = self;
        let content = parts.content.borrow();
        let regular = sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular);
        let text = NSMutableAttributedString::new();
        for seg in &content.title {
            let colour = if highlighted {
                NSColor::selectedMenuItemTextColor()
            } else if !content.enabled {
                NSColor::tertiaryLabelColor()
            } else {
                tone_color(seg.tone)
            };
            // Score dots are drawn small, as the mock has them.
            let size = if seg.size == SegSize::Dots {
                8.0
            } else {
                TEXT_SIZE
            };
            text.appendAttributedString(&styled(&seg.text, &colour, size, regular));
        }
        parts.title.setAttributedStringValue(&text);
    }
}

/// What a submenu's header card shows:
///
/// ```text
/// ⑂ #753  [Ready for review]                 [CI ✓]
/// → jakub/dn-530-m22-…-dependency-edges
/// 5a540c4 · stacked on #752
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct HeaderContent {
    /// The symbol before the title, and its colour.
    pub symbol: Option<(&'static str, Tone)>,
    pub title: String,
    pub pills: Vec<crate::tray_model::Pill>,
    /// A pill at the right-hand end of the first line.
    pub end_pill: Option<crate::tray_model::Pill>,
    /// Lines under the first, dimmed.
    pub lines: Vec<Vec<Seg>>,
}

pub(crate) struct HeaderParts {
    content: RefCell<Option<HeaderContent>>,
}

/// A submenu's header card.
#[derive(Clone)]
pub(crate) struct HeaderView(Canvas<HeaderParts>);

impl std::ops::Deref for HeaderView {
    type Target = Canvas<HeaderParts>;

    fn deref(&self) -> &Canvas<HeaderParts> {
        &self.0
    }
}

impl HeaderView {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        Canvas::from_view(view).map(Self)
    }

    fn ivars(&self) -> &HeaderParts {
        self.behavior()
    }
}

impl CanvasBehavior for HeaderParts {}

impl HeaderView {
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let this = Self(Canvas::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SUBMENU_WIDTH, 40.0)),
            HeaderParts {
                content: RefCell::new(None),
            },
        ));
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }

    pub(crate) fn describe(&self) -> String {
        self.ivars()
            .content
            .borrow()
            .as_ref()
            .map_or_else(|| "header (empty)".to_owned(), crate::inspect::header)
    }

    /// Show `content`, rebuilding the card only when it changed. A header is
    /// not a menu item of its own, so its insides may be rebuilt while the
    /// menu is open.
    pub(crate) fn set(&self, content: HeaderContent) {
        const RIGHT: f64 = 14.0;
        if self.ivars().content.borrow().as_ref() == Some(&content) {
            return;
        }
        let mtm = MainThreadMarker::from(&**self.view());
        for view in self.subviews().to_vec() {
            view.removeFromSuperview();
        }
        let width = self.bounds().size.width.max(SUBMENU_WIDTH);
        let mut y = 8.0;
        let first_height = 18.0;
        // The action rows' columns: the icon centred where theirs are, the
        // text starting where theirs does.
        if let Some((symbol, tone)) = content.symbol
            && let Some(image) =
                crate::controller::symbol_image_sized_pub(symbol, &tone_color(tone), 12.5)
        {
            let view = objc2_app_kit::NSImageView::imageViewWithImage(&image, mtm);
            let size = image.size();
            view.setFrame(NSRect::new(
                NSPoint::new(
                    (ACTION_ICON_X - size.width / 2.0).round(),
                    y + ((first_height - size.height) / 2.0).round(),
                ),
                size,
            ));
            self.addSubview(&view);
        }
        let mut x = ACTION_TEXT_X;
        let semibold = sl_macos::consts::font_weight(sl_macos::consts::Weight::Semibold);
        let title = NSTextField::labelWithString(&NSString::new(), mtm);
        title.setAttributedStringValue(&styled(
            &content.title,
            &NSColor::labelColor(),
            TEXT_SIZE,
            semibold,
        ));
        title.sizeToFit();
        title.setFrameOrigin(NSPoint::new(
            x,
            y + ((first_height - title.frame().size.height) / 2.0).round(),
        ));
        self.addSubview(&title);
        x += title.frame().size.width + 8.0;
        for pill in &content.pills {
            let view = crate::compact::tinted_pill(mtm, pill, true);
            view.setFrameOrigin(NSPoint::new(x, y));
            self.addSubview(&view);
            x += view.frame().size.width + 6.0;
        }
        if let Some(pill) = &content.end_pill {
            let view = crate::compact::tinted_pill(mtm, pill, true);
            view.setFrameOrigin(NSPoint::new(width - RIGHT - view.frame().size.width, y));
            view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            self.addSubview(&view);
        }
        y += first_height + 5.0;
        let regular = sl_macos::consts::font_weight(sl_macos::consts::Weight::Regular);
        for line in &content.lines {
            let label = NSTextField::labelWithString(&NSString::new(), mtm);
            label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            let text = NSMutableAttributedString::new();
            for seg in line {
                text.appendAttributedString(&styled(
                    &seg.text,
                    &tone_color(seg.tone),
                    DETAIL_FONT_SIZE,
                    regular,
                ));
            }
            label.setAttributedStringValue(&text);
            label.sizeToFit();
            let height = label.frame().size.height;
            label.setFrame(NSRect::new(
                NSPoint::new(ACTION_TEXT_X, y),
                NSSize::new(width - ACTION_TEXT_X - RIGHT, height),
            ));
            label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
            self.addSubview(&label);
            y += height + 1.0;
        }
        self.setFrameSize(NSSize::new(width, (y + 7.0).ceil()));
        *self.ivars().content.borrow_mut() = Some(content);
    }
}

/// For previews: the desktop behind a menu.
#[derive(Clone)]
pub(crate) struct Backdrop(Canvas<BackdropLook>);

/// [`Backdrop`]'s behaviour.
pub(crate) struct BackdropLook;

impl CanvasBehavior for BackdropLook {
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        NSColor::windowBackgroundColor().setFill();
        NSBezierPath::fillRect(view.bounds());
    }
}

impl std::ops::Deref for Backdrop {
    type Target = Canvas<BackdropLook>;

    fn deref(&self) -> &Canvas<BackdropLook> {
        &self.0
    }
}

impl Backdrop {
    pub(crate) fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        Self(Canvas::new(mtm, frame, BackdropLook))
    }
}

/// What a [`ToggleButton`] shows.
pub(crate) struct ToggleLook {
    on: Cell<bool>,
    symbol: &'static str,
}

/// The footer's Auto caffeinate switch, drawn by hand.
#[derive(Clone)]
pub(crate) struct ToggleButton(ControlCanvas<ToggleLook>);

impl std::ops::Deref for ToggleButton {
    type Target = ControlCanvas<ToggleLook>;

    fn deref(&self) -> &ControlCanvas<ToggleLook> {
        &self.0
    }
}

impl ToggleButton {
    /// The view `view` is, if it is one of these.
    pub(crate) fn from_view(view: &NSView) -> Option<Self> {
        ControlCanvas::from_view(view).map(Self)
    }

    fn ivars(&self) -> &ToggleLook {
        self.behavior()
    }
}

impl CanvasBehavior for ToggleLook {
    /// Not flipped, as an `NSControl` is not by default.
    fn flipped(&self) -> bool {
        false
    }

    /// Drawn by hand: a push button's own "on" look, and any fill colour set
    /// on it, are ignored inside a menu and in a window that is not key --
    /// both of which the tray's footers are -- so it stays grey.
    fn draw(&self, view: &NSView, _dirty: NSRect) {
        let bounds = view.bounds();
        let on = self.on.get();
        let fill = if on {
            NSColor::controlAccentColor()
        } else {
            NSColor::labelColor().colorWithAlphaComponent(0.1)
        };
        fill.setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, 6.0, 6.0).fill();
        let ink = if on {
            NSColor::whiteColor()
        } else {
            NSColor::labelColor()
        };
        if let Some(image) = crate::controller::symbol_image_sized_pub(self.symbol, &ink, 12.0) {
            let size = image.size();
            let rect = NSRect::new(
                NSPoint::new(
                    ((bounds.size.width - size.width) / 2.0).round(),
                    ((bounds.size.height - size.height) / 2.0).round(),
                ),
                size,
            );
            sl_macos::draw::draw_image(&image, rect);
        }
    }

    /// Flipped here at once, so it answers the click before the controller's
    /// repaint confirms it.
    fn mouse_down(&self, view: &NSControl, _event: &NSEvent) -> bool {
        self.on.set(!self.on.get());
        NSView::setNeedsDisplay(view, true);
        true
    }

    fn accepts_first_mouse(&self) -> bool {
        true
    }
}

impl ToggleButton {
    /// A `size` button showing the SF Symbol `symbol`, `tip` on hover.
    pub(crate) fn new(
        mtm: MainThreadMarker,
        symbol: &'static str,
        tip: &str,
        size: NSSize,
    ) -> Self {
        let this = Self(ControlCanvas::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), size),
            ToggleLook {
                on: Cell::new(false),
                symbol,
            },
        ));
        this.setToolTip(Some(&NSString::from_str(tip)));
        this
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "toggle ({}) {}",
            self.ivars().symbol,
            if self.ivars().on.get() { "on" } else { "off" }
        )
    }

    /// Its tooltip, to name it by.
    pub(crate) fn tip(&self) -> String {
        self.toolTip()
            .map(|tip| tip.to_string())
            .unwrap_or_default()
    }

    pub(crate) fn set_on(&self, on: bool) {
        if self.ivars().on.replace(on) != on {
            NSView::setNeedsDisplay(self, true);
        }
    }
}
