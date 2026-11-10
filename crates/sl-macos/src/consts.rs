//! Framework constants, read safely. Reading an `extern` static is `unsafe`
//! in Rust only because the linker could hand back anything; these are
//! AppKit's own exported constants, valid for the life of the process.

use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSEventTrackingRunLoopMode,
    NSFontWeight, NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSToolbarFlexibleSpaceItemIdentifier, NSToolbarItemIdentifier,
};
use objc2_foundation::{NSArray, NSRunLoopMode};

/// The font weights the tray uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

/// `weight` as AppKit's `NSFontWeight`.
pub fn font_weight(weight: Weight) -> NSFontWeight {
    match weight {
        // SAFETY: an AppKit constant, valid for the life of the process.
        Weight::Regular => unsafe { NSFontWeightRegular },
        // SAFETY: as above.
        Weight::Medium => unsafe { NSFontWeightMedium },
        // SAFETY: as above.
        Weight::Semibold => unsafe { NSFontWeightSemibold },
        // SAFETY: as above.
        Weight::Bold => unsafe { NSFontWeightBold },
    }
}

/// The two system appearances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

/// The system appearance named by `which`; `None` only if AppKit has none by
/// that name, which it always has.
pub fn appearance(which: Appearance) -> Option<Retained<NSAppearance>> {
    let name = match which {
        // SAFETY: an AppKit constant, valid for the life of the process.
        Appearance::Light => unsafe { NSAppearanceNameAqua },
        // SAFETY: as above.
        Appearance::Dark => unsafe { NSAppearanceNameDarkAqua },
    };
    NSAppearance::appearanceNamed(name)
}

/// Whether `appearance` is a dark one: of light and dark, the one it is
/// closest to (a high-contrast or vibrant variant included).
pub fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: AppKit constants, valid for the life of the process.
    let (light, dark) = (unsafe { NSAppearanceNameAqua }, unsafe {
        NSAppearanceNameDarkAqua
    });
    let names = NSArray::from_slice(&[light, dark]);
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|name| &*name == dark)
}

/// The run loop mode a menu is tracked in.
pub fn tracking_run_loop_mode() -> &'static NSRunLoopMode {
    // SAFETY: an AppKit constant, valid for the life of the process.
    unsafe { NSEventTrackingRunLoopMode }
}

/// The toolbar's flexible space item.
pub fn toolbar_flexible_space() -> &'static NSToolbarItemIdentifier {
    // SAFETY: an AppKit constant, valid for the life of the process.
    unsafe { NSToolbarFlexibleSpaceItemIdentifier }
}
