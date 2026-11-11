//! Target-action, made safe.
//!
//! AppKit's `setTarget:` does not retain the target, and `setAction:` takes
//! any selector, so a control wired by hand can message a freed object or one
//! that does not answer. Here every control is wired the same way: its
//! target is an [`ActionTarget`] holding a Rust closure behind the one
//! selector it implements, `fire:`, and that target is retained by the
//! control itself (as an associated object), so it lives exactly as long as
//! the control. Nothing else in the workspace can set a target or an action.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSButton, NSControl, NSControlStateValueOn, NSMenuItem, NSPopUpButton, NSSegmentedControl,
    NSSlider, NSSwitch,
};
use objc2_foundation::NSObjectProtocol;

/// What a bound control runs when it fires: given the sender.
type Handler = Box<dyn Fn(&AnyObject)>;

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `ActionTarget` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLActionTarget"]
    #[ivars = Handler]
    pub struct ActionTarget;

    // SAFETY: `NSObjectProtocol` has no requirements beyond being an object.
    unsafe impl NSObjectProtocol for ActionTarget {}

    impl ActionTarget {
        /// The one action every bound control sends.
        #[unsafe(method(fire:))]
        fn fire(&self, sender: Option<&AnyObject>) {
            let sender = sender.unwrap_or_else(|| self.as_ref());
            (self.ivars())(sender);
        }
    }
);

impl ActionTarget {
    fn new(mtm: MainThreadMarker, handler: impl Fn(&AnyObject) + 'static) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Box::new(handler) as Handler);
        // SAFETY: `NSObject`'s designated initializer, on a fresh allocation.
        unsafe { msg_send![super(this), init] }
    }
}

/// The key the target is kept under on its control. Its address is the key,
/// so it only has to be a static of its own.
static TARGET_KEY: u8 = 0;

/// Keep `value` alive for as long as `owner` is, under `key`; replacing what
/// was kept there, or releasing it when `value` is `None`.
pub(crate) fn retain_on(owner: &AnyObject, key: &'static u8, value: Option<&AnyObject>) {
    let owner: *const AnyObject = owner;
    let value: *const AnyObject = value.map_or(std::ptr::null(), |value| value);
    // SAFETY: both pointers are live objects (or null for `value`), the key is
    // a static's address, and RETAIN_NONATOMIC makes the owner retain the
    // value until it is replaced or the owner is freed. Main-thread-only
    // objects are only ever associated on the main thread.
    unsafe {
        objc2::ffi::objc_setAssociatedObject(
            owner.cast_mut(),
            std::ptr::from_ref(key).cast::<c_void>(),
            value.cast_mut(),
            objc2::ffi::OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
    }
}

/// From now on, a click on `control` calls `handler` with the sender.
pub fn bind(control: &NSControl, handler: impl Fn(&AnyObject) + 'static) {
    let target = ActionTarget::new(MainThreadMarker::from(control), handler);
    retain_on(control, &TARGET_KEY, Some(&target));
    // SAFETY: `target` answers `fire:`, which takes the sender, and the
    // control retains it (just above) for as long as it can send to it.
    unsafe { control.setTarget(Some(&target)) };
    // SAFETY: as above.
    unsafe { control.setAction(Some(sel!(fire:))) };
}

/// As [`bind`], for a menu item.
pub fn bind_item(item: &NSMenuItem, handler: impl Fn(&AnyObject) + 'static) {
    let target = ActionTarget::new(MainThreadMarker::from(item), handler);
    retain_on(item, &TARGET_KEY, Some(&target));
    // SAFETY: as in `bind`.
    unsafe { item.setTarget(Some(&target)) };
    // SAFETY: as in `bind`.
    unsafe { item.setAction(Some(sel!(fire:))) };
}

/// A click on `control` does nothing from now on.
pub fn unbind(control: &NSControl) {
    // SAFETY: clearing the action; nothing is sent.
    unsafe { control.setAction(None) };
    // SAFETY: clearing the target.
    unsafe { control.setTarget(None) };
    retain_on(control, &TARGET_KEY, None);
}

/// Send `control`'s action to its target, as a click does. Every target and
/// action in the workspace is set by [`bind`], so this always reaches a
/// handler, or does nothing for an unbound control.
pub fn fire(control: &NSControl) {
    let Some(action) = control.action() else {
        return;
    };
    let target = control.target();
    // SAFETY: the only action any control carries is `fire:`, set by `bind`
    // together with a target that answers it and that the control retains.
    unsafe { control.sendAction_to(Some(action), target.as_deref()) };
}

/// A sender's tag: a control's, or a menu item's; 0 for anything else.
pub fn tag(sender: &AnyObject) -> isize {
    if let Some(control) = sender.downcast_ref::<NSControl>() {
        return control.tag();
    }
    sender
        .downcast_ref::<NSMenuItem>()
        .map_or(0, NSMenuItem::tag)
}

/// A segmented control's selected segment, or a pop-up menu's chosen item;
/// -1 for any other sender.
pub fn selected_segment(sender: &AnyObject) -> isize {
    if let Some(popup) = sender.downcast_ref::<NSPopUpButton>() {
        return popup.indexOfSelectedItem();
    }
    sender
        .downcast_ref::<NSSegmentedControl>()
        .map_or(-1, NSSegmentedControl::selectedSegment)
}

/// A slider sender's position, rounded; -1 for anything else.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a slider's range is small whole numbers"
)]
pub fn slider_value(sender: &AnyObject) -> i64 {
    sender
        .downcast_ref::<NSSlider>()
        .map_or(-1, |slider| slider.doubleValue().round() as i64)
}

/// Whether a switch or button sender is on (a checked checkbox); false for
/// anything else.
pub fn is_on(sender: &AnyObject) -> bool {
    if let Some(switch) = sender.downcast_ref::<NSSwitch>() {
        return switch.state() == NSControlStateValueOn;
    }
    sender
        .downcast_ref::<NSButton>()
        .is_some_and(|button| button.state() == NSControlStateValueOn)
}

/// Binds controls to values of `A` and hands every click, with its value, to
/// one handler that is set later -- so controls can be built before the
/// object that answers them exists.
pub struct ActionRouter<A> {
    handler: Rc<RefCell<Option<RouteHandler<A>>>>,
}

/// What a router hands each click to.
type RouteHandler<A> = Rc<dyn Fn(A, &AnyObject)>;

impl<A> Clone for ActionRouter<A> {
    fn clone(&self) -> Self {
        Self {
            handler: Rc::clone(&self.handler),
        }
    }
}

impl<A> Default for ActionRouter<A> {
    fn default() -> Self {
        Self {
            handler: Rc::new(RefCell::new(None)),
        }
    }
}

impl<A: Clone + 'static> ActionRouter<A> {
    /// A router with no handler yet: clicks before [`set_handler`](Self::set_handler) do nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// A click on `control` hands `action` to the handler.
    pub fn bind(&self, control: &NSControl, action: A) {
        let router = self.clone();
        bind(control, move |sender| {
            router.dispatch(action.clone(), sender);
        });
    }

    /// As [`bind`](Self::bind), for a menu item.
    pub fn bind_item(&self, item: &NSMenuItem, action: A) {
        let router = self.clone();
        bind_item(item, move |sender| router.dispatch(action.clone(), sender));
    }

    /// Answer every click with `handler`.
    pub fn set_handler(&self, handler: impl Fn(A, &AnyObject) + 'static) {
        *self.handler.borrow_mut() = Some(Rc::new(handler));
    }

    fn dispatch(&self, action: A, sender: &AnyObject) {
        // Cloned out first, so a handler may rebind controls (and so borrow
        // the router) while it runs.
        let handler = self.handler.borrow().clone();
        if let Some(handler) = handler {
            handler(action, sender);
        }
    }
}
