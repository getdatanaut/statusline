//! Delegates built from closures.
//!
//! `setDelegate:` does not retain the delegate either, so each delegate here
//! is kept alive by the object it is the delegate of (as an associated
//! object), exactly as long as that object can call it.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSMenu, NSMenuDelegate, NSPopover, NSPopoverDelegate, NSToolbar, NSToolbarDelegate,
    NSToolbarItem, NSWindow, NSWindowDelegate,
};
use objc2_foundation::{NSArray, NSNotification, NSObjectProtocol, NSString};

static DELEGATE_KEY: u8 = 0;

/// What a menu tells its delegate.
pub struct MenuEvents {
    pub will_open: Box<dyn Fn()>,
    pub did_close: Box<dyn Fn()>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `MenuDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLMenuDelegate"]
    #[ivars = MenuEvents]
    pub struct MenuDelegate;

    // SAFETY: no requirements beyond being an object.
    unsafe impl NSObjectProtocol for MenuDelegate {}

    // SAFETY: the methods below have the protocol's signatures.
    unsafe impl NSMenuDelegate for MenuDelegate {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            (self.ivars().will_open)();
        }

        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            (self.ivars().did_close)();
        }
    }
);

/// Call `events` as `menu` opens and closes.
pub fn set_menu_delegate(menu: &NSMenu, events: MenuEvents) {
    let delegate = MenuDelegate::alloc(MainThreadMarker::from(menu)).set_ivars(events);
    // SAFETY: `NSObject`'s designated initializer, on a fresh allocation.
    let delegate: Retained<MenuDelegate> = unsafe { msg_send![super(delegate), init] };
    crate::action::retain_on(menu, &DELEGATE_KEY, Some(&delegate));
    menu.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `PopoverDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLPopoverDelegate"]
    #[ivars = Box<dyn Fn()>]
    pub struct PopoverDelegate;

    // SAFETY: no requirements beyond being an object.
    unsafe impl NSObjectProtocol for PopoverDelegate {}

    // SAFETY: the method below has the protocol's signature.
    unsafe impl NSPopoverDelegate for PopoverDelegate {
        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, _notification: &NSNotification) {
            (self.ivars())();
        }
    }
);

/// Call `did_close` whenever `popover` closes, by any route.
pub fn set_popover_delegate(popover: &NSPopover, did_close: impl Fn() + 'static) {
    let delegate = PopoverDelegate::alloc(MainThreadMarker::from(popover))
        .set_ivars(Box::new(did_close) as Box<dyn Fn()>);
    // SAFETY: `NSObject`'s designated initializer, on a fresh allocation.
    let delegate: Retained<PopoverDelegate> = unsafe { msg_send![super(delegate), init] };
    crate::action::retain_on(popover, &DELEGATE_KEY, Some(&delegate));
    popover.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `WindowDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLWindowDelegate"]
    #[ivars = Box<dyn Fn()>]
    pub struct WindowDelegate;

    // SAFETY: no requirements beyond being an object.
    unsafe impl NSObjectProtocol for WindowDelegate {}

    // SAFETY: the method below has the protocol's signature.
    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            (self.ivars())();
        }
    }
);

/// Call `did_resize` whenever `window` resizes, live resizes included.
pub fn set_window_delegate(window: &NSWindow, did_resize: impl Fn() + 'static) {
    let delegate = WindowDelegate::alloc(MainThreadMarker::from(window))
        .set_ivars(Box::new(did_resize) as Box<dyn Fn()>);
    // SAFETY: `NSObject`'s designated initializer, on a fresh allocation.
    let delegate: Retained<WindowDelegate> = unsafe { msg_send![super(delegate), init] };
    crate::action::retain_on(window, &DELEGATE_KEY, Some(&delegate));
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
}

/// What a toolbar asks its delegate.
pub struct ToolbarItems {
    /// The identifiers it shows, in order; also all it allows.
    pub identifiers: Box<dyn Fn() -> Vec<Retained<NSString>>>,
    /// The item for one identifier.
    pub item: Box<ItemFor>,
}

/// The toolbar item for an identifier, if there is one.
pub type ItemFor = dyn Fn(&NSString) -> Option<Retained<NSToolbarItem>>;

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `ToolbarDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLToolbarDelegate"]
    #[ivars = ToolbarItems]
    pub struct ToolbarDelegate;

    // SAFETY: no requirements beyond being an object.
    unsafe impl NSObjectProtocol for ToolbarDelegate {}

    // SAFETY: the methods below have the protocol's signatures.
    unsafe impl NSToolbarDelegate for ToolbarDelegate {
        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            NSArray::from_retained_slice(&(self.ivars().identifiers)())
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            NSArray::from_retained_slice(&(self.ivars().identifiers)())
        }

        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSString,
            _inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            (self.ivars().item)(identifier)
        }
    }
);

/// Answer `toolbar`'s questions with `items`. Set before the toolbar is put
/// on a window: it asks for its items as soon as it is.
pub fn set_toolbar_delegate(toolbar: &NSToolbar, items: ToolbarItems) {
    let delegate = ToolbarDelegate::alloc(MainThreadMarker::from(toolbar)).set_ivars(items);
    // SAFETY: `NSObject`'s designated initializer, on a fresh allocation.
    let delegate: Retained<ToolbarDelegate> = unsafe { msg_send![super(delegate), init] };
    crate::action::retain_on(toolbar, &DELEGATE_KEY, Some(&delegate));
    toolbar.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
}
