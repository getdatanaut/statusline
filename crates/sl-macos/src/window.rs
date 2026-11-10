//! Windows the tray owns.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::NSRect;

/// A buffered window with `style`, its content `frame`, that is kept rather
/// than released when it closes -- the tray holds every window it makes in a
/// `Retained`, and a window that also released itself on close would be freed
/// twice.
pub fn new_window(mtm: MainThreadMarker, frame: NSRect, style: NSWindowStyleMask) -> Retained<NSWindow> {
    // SAFETY: a fresh allocation, initialised once.
    let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, style, NSBackingStoreType::Buffered, false) };
    // SAFETY: the caller's `Retained` is the window's owner, so it must not
    // release itself as well.
    unsafe { window.setReleasedWhenClosed(false) };
    window
}
