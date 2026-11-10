//! Standard controls, made with no target: wire them with
//! [`crate::action::bind`].

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSImage, NSSegmentSwitchTracking, NSSegmentedControl};
use objc2_foundation::{NSArray, NSString};

/// A push button titled `title`.
pub fn button(mtm: MainThreadMarker, title: &str) -> Retained<NSButton> {
    // SAFETY: no target and no action: nothing is sent until it is bound.
    unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm) }
}

/// A push button with `image` and `title`.
pub fn button_with_image(mtm: MainThreadMarker, title: &str, image: &NSImage) -> Retained<NSButton> {
    // SAFETY: as in `button`.
    unsafe { NSButton::buttonWithTitle_image_target_action(&NSString::from_str(title), image, None, None, mtm) }
}

/// A checkbox titled `title`.
pub fn checkbox(mtm: MainThreadMarker, title: &str) -> Retained<NSButton> {
    // SAFETY: as in `button`.
    unsafe { NSButton::checkboxWithTitle_target_action(&NSString::from_str(title), None, None, mtm) }
}

/// A segmented control of `labels`, one selected at a time.
pub fn segmented(mtm: MainThreadMarker, labels: &[&str]) -> Retained<NSSegmentedControl> {
    let labels: Vec<Retained<NSString>> = labels.iter().map(|label| NSString::from_str(label)).collect();
    // SAFETY: as in `button`.
    unsafe { NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(&NSArray::from_retained_slice(&labels), NSSegmentSwitchTracking::SelectOne, None, None, mtm) }
}
