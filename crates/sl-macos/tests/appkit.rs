//! sl-macos against the real AppKit, on the main thread (`harness = false`):
//! each wrapper does what its docs say. Run by `cargo test`; needs a logged-in
//! GUI session, as the tray's own self-test does.

#![expect(clippy::print_stdout, clippy::print_stderr, reason = "a test harness of its own: it reports what it checked")]

use std::cell::Cell;
use std::rc::Rc;

use objc2::{MainThreadMarker, MainThreadOnly};
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSBitmapImageRep, NSColor, NSEventMask, NSFont, NSView, NSWindowStyleMask};
use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};
use sl_macos::consts::{self, Appearance, Weight};
use sl_macos::canvas::{Canvas, CanvasBehavior, ControlCanvas, Hit};
use sl_macos::{draw, event, process, system, text, timer, window};

type Check = (&'static str, fn(MainThreadMarker) -> Result<(), String>);

fn main() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("appkit tests must run on the main thread");
        std::process::exit(1);
    };
    let _app = NSApplication::sharedApplication(mtm);
    let checks: [Check; 11] = [
        ("constants", constants),
        ("appearances", appearances),
        ("attributed text", attributed_text),
        ("dynamic colour follows the appearance", dynamic_colour),
        ("png of a bitmap", png),
        ("window outlives its close", window_kept),
        ("timer fires in the tracking mode, stops when dropped", repeating_timer),
        ("global monitor installs and drops", global_monitor),
        ("disk counters", disk_counters),
        ("canvas handles and defaults", canvas_handles),
        ("text storage", text_storage),
    ];
    let mut failed = 0;
    for (name, check) in checks {
        match check(mtm) {
            Ok(()) => println!("ok   {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed += 1;
            }
        }
    }
    println!("{} passed, {failed} failed", checks.len() - failed);
    std::process::exit(i32::from(failed > 0));
}

fn ensure(ok: bool, why: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why.to_owned()) }
}

fn constants(_: MainThreadMarker) -> Result<(), String> {
    let weights = [Weight::Regular, Weight::Medium, Weight::Semibold, Weight::Bold].map(consts::font_weight);
    ensure(weights.windows(2).all(|pair| pair[0] < pair[1]), "weights grow heavier in order")?;
    ensure(!consts::tracking_run_loop_mode().to_string().is_empty(), "a tracking mode name")?;
    ensure(!consts::toolbar_flexible_space().to_string().is_empty(), "a flexible space identifier")
}

fn appearances(_: MainThreadMarker) -> Result<(), String> {
    let light = consts::appearance(Appearance::Light).ok_or("no light appearance")?;
    let dark = consts::appearance(Appearance::Dark).ok_or("no dark appearance")?;
    ensure(!consts::is_dark(&light), "light is not dark")?;
    ensure(consts::is_dark(&dark), "dark is dark")
}

fn attributed_text(_: MainThreadMarker) -> Result<(), String> {
    let font = NSFont::systemFontOfSize(13.0);
    let color = NSColor::redColor();
    let attrs = text::Attrs { font: Some(&font), color: Some(&color), ..Default::default() };
    let string = text::attributed("hello", &attrs);
    ensure(string.string().to_string() == "hello", "the text is kept")?;
    ensure(attrs.dictionary().count() == 2, "two attributes, two entries")?;
    let wide = text::text_width("hello world", &attrs);
    let narrow = text::text_width("hi", &attrs);
    ensure(wide > narrow && narrow > 0.0, "longer text is wider")?;
    let mutable = objc2_foundation::NSMutableAttributedString::from_attributed_nsstring(&string);
    text::add_kern(&mutable, 2.0);
    ensure(mutable.length() == 5, "kerning keeps the text")
}

fn dynamic_colour(_: MainThreadMarker) -> Result<(), String> {
    let color = draw::dynamic_color(|dark| NSColor::colorWithSRGBRed_green_blue_alpha(if dark { 1.0 } else { 0.0 }, 0.0, 0.0, 1.0));
    let white_in = |which| -> Result<f64, String> {
        let appearance = consts::appearance(which).ok_or("no appearance")?;
        let white = Cell::new(-1.0);
        let block = block2::RcBlock::new(|| {
            if let Some(resolved) = color.colorUsingType(objc2_app_kit::NSColorType::ComponentBased) {
                white.set(resolved.redComponent());
            }
        });
        appearance.performAsCurrentDrawingAppearance(&block);
        Ok(white.get())
    };
    let (light, dark) = (white_in(Appearance::Light)?, white_in(Appearance::Dark)?);
    ensure(light < 0.1 && dark > 0.9, &format!("no red in light, full red in dark: {light} / {dark}"))
}

fn png(mtm: MainThreadMarker) -> Result<(), String> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(8.0, 8.0)));
    let bitmap: Retained<NSBitmapImageRep> = view.bitmapImageRepForCachingDisplayInRect(view.bounds()).ok_or("no bitmap")?;
    let data = draw::png(&bitmap).ok_or("no PNG")?;
    let bytes = data.to_vec();
    ensure(bytes.starts_with(b"\x89PNG"), "PNG magic")
}

fn window_kept(mtm: MainThreadMarker) -> Result<(), String> {
    let window = window::new_window(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 80.0)), NSWindowStyleMask::Titled | NSWindowStyleMask::Closable);
    ensure(!window.isReleasedWhenClosed(), "not released when closed")?;
    window.close();
    // Still usable: a window released on close would be freed here.
    window.setTitle(&objc2_foundation::NSString::from_str("still here"));
    ensure(window.title().to_string() == "still here", "usable after close")?;
    let content = window.contentView().ok_or("no content view")?;
    ensure(system::superview(&content).is_some(), "the content view sits in the frame view")
}

fn repeating_timer(_: MainThreadMarker) -> Result<(), String> {
    let ticks = Rc::new(Cell::new(0));
    let counter = Rc::clone(&ticks);
    let timer = timer::RepeatingTimer::common_modes(0.05, 0.0, move || counter.set(counter.get() + 1));
    let until = NSDate::dateWithTimeIntervalSinceNow(0.4);
    while NSDate::now().timeIntervalSinceDate(&until) < 0.0 {
        NSRunLoop::currentRunLoop().runMode_beforeDate(consts::tracking_run_loop_mode(), &until);
    }
    let fired = ticks.get();
    ensure(fired >= 2, &format!("fires while tracking: {fired} ticks"))?;
    drop(timer);
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.2));
    ensure(ticks.get() == fired, "no ticks once dropped")
}

fn global_monitor(_: MainThreadMarker) -> Result<(), String> {
    // Global monitors need no permission for mouse events. Nothing can be
    // clicked in another app from here, so this checks install and removal
    // (a double removal would raise).
    let monitor = event::GlobalMonitor::new(NSEventMask::LeftMouseDown, |_| {}).ok_or("not installed")?;
    let second = event::GlobalMonitor::new(NSEventMask::RightMouseDown, |_| {}).ok_or("not installed")?;
    drop(monitor);
    drop(second);
    Ok(())
}

fn disk_counters(_: MainThreadMarker) -> Result<(), String> {
    let before = process::disk_io();
    let path = std::env::temp_dir().join(format!("sl-macos-disk-{}", std::process::id()));
    std::fs::write(&path, vec![7u8; 1 << 20]).map_err(|e| e.to_string())?;
    std::fs::remove_file(&path).ok();
    let after = process::disk_io();
    ensure(after.read >= before.read && after.written >= before.written, "counters only grow")
}

/// A behaviour that keeps every default, and counts its draws.
#[derive(Default)]
struct Plain {
    draws: Cell<usize>,
}

impl CanvasBehavior for Plain {
    fn draw(&self, _view: &NSView, _dirty: NSRect) {
        self.draws.set(self.draws.get() + 1);
    }
}

/// Another behaviour, to tell apart from [`Plain`].
struct Other;

impl CanvasBehavior for Other {}

fn canvas_handles(mtm: MainThreadMarker) -> Result<(), String> {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(20.0, 10.0));
    let canvas = Canvas::new(mtm, frame, Plain::default());
    ensure(canvas.isFlipped(), "flipped by default")?;
    // Drawn through AppKit, into a bitmap.
    let bitmap = canvas.bitmapImageRepForCachingDisplayInRect(canvas.bounds()).ok_or("no bitmap")?;
    canvas.cacheDisplayInRect_toBitmapImageRep(canvas.bounds(), &bitmap);
    ensure(canvas.behavior().draws.get() == 1, "drawRect: reaches the behaviour")?;
    // Back from the view hierarchy, as its own type only.
    let plain: Retained<NSView> = canvas.retained_view();
    let found = Canvas::<Plain>::from_view(&plain).ok_or("found again")?;
    ensure(found == canvas && found.clone() == canvas, "the same view, and its clone")?;
    ensure(Canvas::<Other>::from_view(&plain).is_none(), "not as another behaviour")?;
    ensure(ControlCanvas::<Plain>::from_view(&plain).is_none(), "not as a control")?;
    let weak = canvas.weak();
    ensure(Canvas::<Plain>::upgrade(&weak).is_some_and(|again| again == canvas), "a weak handle comes back while it lives")?;
    ensure(Canvas::new(mtm, frame, Plain::default()) != canvas, "two views are two")?;
    // The defaults a behaviour leaves alone.
    let other = Other;
    let control = ControlCanvas::new(mtm, frame, Other);
    let event = objc2_app_kit::NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        objc2_app_kit::NSEventType::LeftMouseDown,
        NSPoint::new(1.0, 1.0),
        objc2_app_kit::NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        1,
        1.0,
    )
    .ok_or("no event")?;
    other.mouse_moved(&control, &event);
    other.mouse_exited(&control, &event);
    other.appearance_changed(&control);
    ensure(!other.mouse_down(&control, &event) && !other.mouse_up(&control, &event), "a click is not a press by default")?;
    ensure(other.hit() == Hit::Default && !other.accepts_first_mouse() && other.cursor().is_none(), "plain hit testing, no first mouse, the arrow")?;
    ensure(ControlCanvas::<Other>::from_view(&control.retained_view()).is_some_and(|again| again == control.clone()), "a control comes back as itself")
}

fn text_storage(mtm: MainThreadMarker) -> Result<(), String> {
    let view = objc2_app_kit::NSTextView::initWithFrame(objc2_app_kit::NSTextView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(50.0, 20.0)));
    view.setString(&objc2_foundation::NSString::from_str("held"));
    let storage = system::text_storage(&view).ok_or("no storage")?;
    ensure(storage.string().to_string() == "held", "the view's text")
}
