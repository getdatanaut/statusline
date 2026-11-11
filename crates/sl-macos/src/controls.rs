//! Standard controls, made with no target: wire them with
//! [`crate::action::bind`].

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSButton, NSImage, NSPopUpButton, NSSegmentSwitchTracking, NSSegmentedControl, NSSlider,
    NSSwitch,
};
use objc2_foundation::{NSArray, NSRect, NSString};

/// A push button titled `title`.
pub fn button(mtm: MainThreadMarker, title: &str) -> Retained<NSButton> {
    // SAFETY: no target and no action: nothing is sent until it is bound.
    unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm) }
}

/// A push button with `image` and `title`.
pub fn button_with_image(
    mtm: MainThreadMarker,
    title: &str,
    image: &NSImage,
) -> Retained<NSButton> {
    // SAFETY: as in `button`.
    unsafe {
        NSButton::buttonWithTitle_image_target_action(
            &NSString::from_str(title),
            image,
            None,
            None,
            mtm,
        )
    }
}

/// A checkbox titled `title`.
pub fn checkbox(mtm: MainThreadMarker, title: &str) -> Retained<NSButton> {
    // SAFETY: as in `button`.
    unsafe {
        NSButton::checkboxWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    }
}

/// A segmented control of `labels`, one selected at a time.
pub fn segmented(mtm: MainThreadMarker, labels: &[&str]) -> Retained<NSSegmentedControl> {
    let labels: Vec<Retained<NSString>> = labels
        .iter()
        .map(|label| NSString::from_str(label))
        .collect();
    // SAFETY: as in `button`.
    unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &NSArray::from_retained_slice(&labels),
            NSSegmentSwitchTracking::SelectOne,
            None,
            None,
            mtm,
        )
    }
}

/// An on/off switch, as System Settings has them.
pub fn switch(mtm: MainThreadMarker) -> Retained<NSSwitch> {
    NSSwitch::new(mtm)
}

/// A pop-up menu of `labels`, one chosen at a time.
pub fn popup(mtm: MainThreadMarker, labels: &[&str]) -> Retained<NSPopUpButton> {
    let popup = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), NSRect::ZERO, false);
    for label in labels {
        popup.addItemWithTitle(&NSString::from_str(label));
    }
    popup
}

/// A slider over `0..positions`, one tick for each position.
pub fn slider(mtm: MainThreadMarker, positions: usize) -> Retained<NSSlider> {
    let last = positions.saturating_sub(1);
    // SAFETY: as in `button`.
    let slider = unsafe {
        NSSlider::sliderWithValue_minValue_maxValue_target_action(
            0.0,
            0.0,
            f64::from(u32::try_from(last).unwrap_or(0)),
            None,
            None,
            mtm,
        )
    };
    slider.setNumberOfTickMarks(isize::try_from(positions).unwrap_or(0));
    slider.setAllowsTickMarkValuesOnly(true);
    slider
}
