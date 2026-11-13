//! Small calls on system objects that the bindings mark `unsafe` for reasons
//! that do not apply to how the tray makes them.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSButton, NSMenu, NSMenuItem, NSPasteboard, NSPasteboardTypeString, NSTabViewItem, NSTextStorage,
    NSTextView, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{NSRect, NSString, NSUserDefaults};

/// Put `text` on the general pasteboard, replacing what was there.
pub fn copy_to_pasteboard(text: &str) {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: `NSPasteboardTypeString` is an AppKit constant, and a string is
    // what that type holds.
    unsafe { pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) };
}

/// Store `value` under `key` in `defaults`.
pub fn defaults_set_string(defaults: &NSUserDefaults, key: &str, value: &str) {
    // SAFETY: an `NSString` is a valid property-list value.
    unsafe {
        defaults.setObject_forKey(Some(&NSString::from_str(value)), &NSString::from_str(key));
    }
}

/// The text view's storage.
pub fn text_storage(view: &NSTextView) -> Option<Retained<NSTextStorage>> {
    // SAFETY: an `NSTextView` is main-thread-only, so this runs on the main
    // thread, where its storage may be read.
    unsafe { view.textStorage() }
}

/// The view `view` is in, if any.
pub fn superview(view: &NSView) -> Option<Retained<NSView>> {
    // SAFETY: an `NSView` is main-thread-only, so its view hierarchy is read
    // on the main thread.
    unsafe { view.superview() }
}

/// Click `button` as a person would: a checkbox flips, then the action is sent.
pub fn perform_click(button: &NSButton) {
    // SAFETY: no sender; every action in the tray takes any sender.
    unsafe { button.performClick(None) };
}

/// A tab with no identifier.
pub fn tab_view_item() -> Retained<NSTabViewItem> {
    // SAFETY: a nil identifier is allowed.
    unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) }
}

/// Track the pointer over `view` with `options`, the view itself the owner,
/// as it is the one the events go to. The area is added to `view`, which
/// keeps it; the owner is unretained, and an owner that is the view holding
/// the area cannot outlive it.
pub fn track_view(view: &NSView, options: NSTrackingAreaOptions) {
    // SAFETY: the owner is `view`, which holds the area for as long as it can
    // send to it; no user info.
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            options,
            Some(view),
            None,
        )
    };
    view.addTrackingArea(&area);
}

/// Give the app a main menu holding only Edit (Undo, Cut, Copy, Paste,
/// Select All), so ⌘Z/⌘X/⌘C/⌘V/⌘A work in its text fields. An accessory app
/// never shows its main menu, but key equivalents still go through it, and
/// without one a text field gets none of them.
pub fn install_edit_menu(mtm: MainThreadMarker) {
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
    for (title, action, key) in [
        ("Undo", objc2::sel!(undo:), "z"),
        ("Redo", objc2::sel!(redo:), "Z"),
        ("Cut", objc2::sel!(cut:), "x"),
        ("Copy", objc2::sel!(copy:), "c"),
        ("Paste", objc2::sel!(paste:), "v"),
        ("Select All", objc2::sel!(selectAll:), "a"),
    ] {
        // SAFETY: each is a standard responder action, sent along the
        // responder chain (no target), which ignores it where unhandled.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        edit.addItem(&item);
    }
    let edit_item = menu_item(mtm, "Edit");
    edit_item.setSubmenu(Some(&edit));
    let main = NSMenu::new(mtm);
    main.addItem(&edit_item);
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&main));
}

/// A menu item titled `title`, with no action and no key equivalent.
pub fn menu_item(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    // SAFETY: no action selector, so nothing is ever sent.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            &NSString::new(),
        )
    }
}
