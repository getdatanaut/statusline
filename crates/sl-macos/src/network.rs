//! Whether the Mac has a usable network path, from Network.framework's path
//! monitor: the system's own answer, given at once and again on every change
//! (Wi-Fi dropping or joining, a cable, a VPN), without asking any host.

use std::ffi::c_void;

use block2::{Block, RcBlock};

/// `nw_path_monitor_t`, `nw_path_t` and `dispatch_queue_t`: retained
/// Objective-C objects, only ever passed straight back to the system.
type Object = *mut c_void;

/// `nw_path_status_satisfied`: the path can be used.
const SATISFIED: i32 = 1;
/// `QOS_CLASS_UTILITY`: work the user does not wait on.
const QOS_CLASS_UTILITY: isize = 0x11;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    fn nw_path_monitor_create() -> Object;
    fn nw_path_monitor_set_update_handler(monitor: Object, handler: &Block<dyn Fn(Object)>);
    fn nw_path_monitor_set_queue(monitor: Object, queue: Object);
    fn nw_path_monitor_start(monitor: Object);
    fn nw_path_get_status(path: Object) -> i32;
}

unsafe extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> Object;
}

/// Call `changed` with whether the network can be used: once soon after this
/// returns, then on every change, on a background queue, for the life of the
/// process. `false` if the monitor cannot be made.
pub fn watch(changed: impl Fn(bool) + Send + Sync + 'static) -> bool {
    let handler = RcBlock::new(move |path: Object| {
        // SAFETY: the monitor hands its handler a valid `nw_path_t`, kept
        // alive for the length of the call.
        let status = unsafe { nw_path_get_status(path) };
        changed(status == SATISFIED);
    });
    // SAFETY: no arguments; returns a new retained monitor, or null.
    let monitor = unsafe { nw_path_monitor_create() };
    if monitor.is_null() {
        return false;
    }
    // SAFETY: `monitor` is the live monitor made above, not yet started; the
    // handler owns everything it touches ('static) and is copied by the call.
    unsafe { nw_path_monitor_set_update_handler(monitor, &handler) };
    // SAFETY: a global queue always exists and needs no release.
    let queue = unsafe { dispatch_get_global_queue(QOS_CLASS_UTILITY, 0) };
    // SAFETY: `monitor` is live and not yet started; `queue` is a valid queue.
    unsafe { nw_path_monitor_set_queue(monitor, queue) };
    // SAFETY: `monitor` has its handler and queue set, as starting requires.
    // It is never cancelled or released, so it watches for the whole process.
    unsafe { nw_path_monitor_start(monitor) };
    true
}
