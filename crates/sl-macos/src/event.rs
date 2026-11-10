//! Watching events that go to other apps.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask};

/// A global event monitor, removed when this is dropped. Each monitor is
/// removed exactly once, which AppKit requires.
pub struct GlobalMonitor {
    token: Retained<AnyObject>,
}

impl GlobalMonitor {
    /// Call `handler` with each event matching `mask` that goes to another
    /// app; `None` if AppKit would not install it. Installed from the main
    /// thread (`mtm`), where AppKit handles events: a `handler` that is not
    /// `Send` is then called on the thread that made it, whether AppKit
    /// calls it from the main thread or from the installing thread's run
    /// loop.
    pub fn new(
        _mtm: MainThreadMarker,
        mask: NSEventMask,
        handler: impl Fn(&NSEvent) + 'static,
    ) -> Option<Self> {
        let block = RcBlock::new(move |event: NonNull<NSEvent>| {
            // SAFETY: AppKit passes a live event for the call's duration.
            handler(unsafe { event.as_ref() });
        });
        NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block)
            .map(|token| Self { token })
    }
}

impl Drop for GlobalMonitor {
    fn drop(&mut self) {
        // SAFETY: `token` came from `addGlobalMonitorForEventsMatchingMask:handler:`
        // and this is the only place it is removed.
        unsafe { NSEvent::removeMonitor(&self.token) };
    }
}
