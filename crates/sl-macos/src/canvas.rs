//! Views drawn and driven from Rust.
//!
//! A custom AppKit view needs a class of its own, and defining one is
//! `unsafe`. Instead, two classes live here, and the tray's views are
//! *behaviours* plugged into them:
//!
//! - [`Canvas`]: an `NSView` that draws, follows the pointer and the
//!   appearance, and otherwise lets events through as any view does.
//! - [`ControlCanvas`]: an `NSControl` that does all that, and also takes
//!   clicks, sending its bound action (see [`crate::action`]) when the
//!   behaviour says the click was a press.
//!
//! The behaviour is shared (`Rc`) between the view's ivars and the Rust
//! handle, so the handle reads it without a downcast, and a view found in the
//! view hierarchy can be turned back into its handle with `from_view`.

use std::any::Any;
use std::ops::Deref;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSButton, NSControl, NSCursor, NSEvent, NSResponder, NSView};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect};

/// How a control canvas answers a hit test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hit {
    /// As any view: the deepest subview under the point.
    #[default]
    Default,
    /// Its buttons keep their clicks; anywhere else on it is itself, so its
    /// labels and decorations do not take clicks meant for it.
    ButtonsElseSelf,
}

/// What a canvas does. Every method has a default that does nothing.
pub trait CanvasBehavior: Any {
    /// Top-down coordinates, as the tray lays everything out.
    fn flipped(&self) -> bool {
        true
    }

    /// Draw the part of `view` within `dirty`.
    fn draw(&self, _view: &NSView, _dirty: NSRect) {}

    /// The pointer moved over `view` (with a tracking area asking for it).
    fn mouse_moved(&self, _view: &NSView, _event: &NSEvent) {}

    /// The pointer left `view`.
    fn mouse_exited(&self, _view: &NSView, _event: &NSEvent) {}

    /// `view`'s appearance changed (light or dark); it has already been
    /// told to redraw.
    fn appearance_changed(&self, _view: &NSView) {}

    /// For a control canvas: a mouse-down on it; `true` sends its action.
    fn mouse_down(&self, _view: &NSControl, _event: &NSEvent) -> bool {
        false
    }

    /// For a control canvas: a mouse-up on it; `true` sends its action.
    fn mouse_up(&self, _view: &NSControl, _event: &NSEvent) -> bool {
        false
    }

    /// For a control canvas: how it answers a hit test.
    fn hit(&self) -> Hit {
        Hit::Default
    }

    /// For a control canvas: whether a click that activates the window also
    /// clicks it.
    fn accepts_first_mouse(&self) -> bool {
        false
    }

    /// For a control canvas: the cursor over it, if not the arrow.
    fn cursor(&self) -> Option<Retained<NSCursor>> {
        None
    }
}

/// The behaviour as the view holds it.
type Behavior = Rc<dyn CanvasBehavior>;

define_class!(
    // SAFETY:
    // - `NSView` may be subclassed; this overrides only methods that exist to
    //   be overridden, each calling the superclass where AppKit requires it.
    // - `CanvasView` does not implement `Drop`.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLCanvasView"]
    #[ivars = Behavior]
    pub struct CanvasView;

    // SAFETY: no requirements beyond being an object.
    unsafe impl NSObjectProtocol for CanvasView {}

    impl CanvasView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            self.ivars().flipped()
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.ivars().draw(self, dirty);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.ivars().mouse_moved(self, event);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            self.ivars().mouse_exited(self, event);
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            // SAFETY: the superclass's own implementation, no arguments.
            let (): () = unsafe { msg_send![super(self), viewDidChangeEffectiveAppearance] };
            self.setNeedsDisplay(true);
            self.ivars().appearance_changed(self);
        }
    }
);

define_class!(
    // SAFETY:
    // - `NSControl` may be subclassed; this overrides only methods that exist
    //   to be overridden, each calling the superclass where AppKit requires it.
    // - `CanvasControl` does not implement `Drop`.
    #[unsafe(super(NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SLCanvasControl"]
    #[ivars = Behavior]
    pub struct CanvasControl;

    // SAFETY: no requirements beyond being an object.
    unsafe impl NSObjectProtocol for CanvasControl {}

    impl CanvasControl {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            self.ivars().flipped()
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.ivars().draw(self, dirty);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.ivars().mouse_down(self, event) {
                crate::action::fire(self);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if self.ivars().mouse_up(self, event) {
                crate::action::fire(self);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.ivars().mouse_moved(self, event);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            self.ivars().mouse_exited(self, event);
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            self.ivars().accepts_first_mouse()
        }

        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            // SAFETY: `hitTest:` is `NSView`'s, and returns a view or nil.
            let hit: Option<Retained<NSView>> = unsafe { msg_send![super(self), hitTest: point] };
            let hit = match (self.ivars().hit(), hit) {
                (_, None) => None,
                (Hit::Default, Some(view)) => Some(view),
                (Hit::ButtonsElseSelf, Some(view)) if view.downcast_ref::<NSButton>().is_some() => Some(view),
                (Hit::ButtonsElseSelf, Some(_)) => Some(Retained::into_super(Retained::into_super(objc2::Message::retain(self)))),
            };
            hit.map_or(std::ptr::null_mut(), Retained::autorelease_return)
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            if let Some(cursor) = self.ivars().cursor() {
                self.addCursorRect_cursor(self.bounds(), &cursor);
            }
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            // SAFETY: the superclass's own implementation, no arguments.
            let (): () = unsafe { msg_send![super(self), viewDidChangeEffectiveAppearance] };
            NSView::setNeedsDisplay(self, true);
            self.ivars().appearance_changed(self);
        }
    }
);

/// A [`CanvasView`] running `B`.
pub struct Canvas<B> {
    view: Retained<CanvasView>,
    behavior: Rc<B>,
}

/// A [`CanvasControl`] running `B`.
pub struct ControlCanvas<B> {
    view: Retained<CanvasControl>,
    behavior: Rc<B>,
}

macro_rules! canvas_handle {
    ($handle:ident, $class:ident) => {
        impl<B: CanvasBehavior> $handle<B> {
            /// A new view at `frame`, running `behavior`.
            pub fn new(mtm: MainThreadMarker, frame: NSRect, behavior: B) -> Self {
                let behavior = Rc::new(behavior);
                let shared: Behavior = behavior.clone();
                let this = $class::alloc(mtm).set_ivars(shared);
                // SAFETY: `NSView`'s designated initializer, on a fresh allocation.
                let view: Retained<$class> = unsafe { msg_send![super(this), initWithFrame: frame] };
                Self { view, behavior }
            }

            /// Its behaviour.
            pub fn behavior(&self) -> &B {
                &self.behavior
            }

            /// The view as AppKit sees it.
            pub fn view(&self) -> &$class {
                &self.view
            }

            /// The view, retained, as a plain `NSView`.
            pub fn retained_view(&self) -> Retained<NSView> {
                let view: &NSView = &self.view;
                objc2::Message::retain(view)
            }

            /// A weak reference to the view, for closures it holds itself.
            pub fn weak(&self) -> objc2::rc::Weak<$class> {
                objc2::rc::Weak::from_retained(&self.view)
            }

            /// The handle again, from [`weak`](Self::weak), while the view lives.
            pub fn upgrade(weak: &objc2::rc::Weak<$class>) -> Option<Self> {
                let view = weak.load()?;
                Self::from_view(&view)
            }

            /// The handle of `view`, if it is one of these running `B`.
            pub fn from_view(view: &NSView) -> Option<Self> {
                let view = view.downcast_ref::<$class>()?;
                let shared: &Behavior = view.ivars();
                let behavior = Rc::clone(shared);
                let any: Rc<dyn Any> = behavior;
                let behavior = any.downcast::<B>().ok()?;
                Some(Self { view: objc2::Message::retain(view), behavior })
            }
        }

        impl<B> Clone for $handle<B> {
            fn clone(&self) -> Self {
                Self { view: self.view.clone(), behavior: Rc::clone(&self.behavior) }
            }
        }

        impl<B> Deref for $handle<B> {
            type Target = $class;

            fn deref(&self) -> &$class {
                &self.view
            }
        }

        impl<B> PartialEq for $handle<B> {
            fn eq(&self, other: &Self) -> bool {
                std::ptr::eq(Retained::as_ptr(&self.view), Retained::as_ptr(&other.view))
            }
        }
    };
}

canvas_handle!(Canvas, CanvasView);
canvas_handle!(ControlCanvas, CanvasControl);
