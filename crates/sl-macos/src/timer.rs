//! Timers that fire while a menu is tracked: a repeating one, and a call
//! made once, on the run loop's next turn.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};

/// A repeating timer in the run loop's *common* modes, invalidated when this
/// is dropped.
///
/// Gotcha #1: `+[NSTimer scheduledTimerWithTimeInterval:...]` installs a
/// timer into `NSDefaultRunLoopMode` only, and while a menu is open the run
/// loop runs in `NSEventTrackingRunLoopMode`, so a scheduled timer stops
/// firing the instant the user opens the menu. This builds the timer
/// unscheduled and adds it to the common modes by hand. Do not "simplify" it
/// back to the scheduled constructor.
pub struct RepeatingTimer {
    timer: Retained<NSTimer>,
}

impl RepeatingTimer {
    /// Call `tick` every `interval` seconds, give or take `tolerance`, on the
    /// current (main) thread's run loop, in every common mode.
    pub fn common_modes(interval: f64, tolerance: f64, tick: impl FnMut() + 'static) -> Self {
        let tick = RefCell::new(tick);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            // A tick never re-enters itself: the run loop calls it once at a time.
            if let Ok(mut tick) = tick.try_borrow_mut() {
                tick();
            }
        });
        // SAFETY: the block owns everything it touches ('static), and the
        // timer copies and keeps it for as long as it can fire.
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(interval, true, &block) };
        timer.setTolerance(tolerance);
        // SAFETY: the timer was made just above and is not scheduled anywhere
        // else; `NSRunLoopCommonModes` is a Foundation constant.
        unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        Self { timer }
    }
}

/// Call `run` once, as soon as the current (main) thread's run loop comes
/// round to it in any common mode: after whatever callback asked for it has
/// returned, so a `run` that blocks (a modal alert) holds neither that
/// callback up nor a [`RepeatingTimer`], which skips its ticks meanwhile.
pub fn once(run: impl FnOnce() + 'static) {
    let run = Cell::new(Some(run));
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        if let Some(run) = run.take() {
            run();
        }
    });
    // SAFETY: the block owns everything it touches ('static), and the timer
    // copies and keeps it until it has fired.
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.0, false, &block) };
    // SAFETY: the timer was made just above and is not scheduled anywhere
    // else; `NSRunLoopCommonModes` is a Foundation constant. The run loop
    // keeps the timer until it fires, which invalidates it.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

impl Drop for RepeatingTimer {
    fn drop(&mut self) {
        self.timer.invalidate();
    }
}
