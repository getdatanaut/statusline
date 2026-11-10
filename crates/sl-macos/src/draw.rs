//! Drawing: images into the current context, bitmaps out as PNG, and colours
//! that follow the appearance they are drawn in.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearance, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSCompositingOperation, NSImage,
};
use objc2_foundation::{NSData, NSDictionary, NSRect};

/// Draw all of `image` into `rect` of the current context, the right way up
/// in a flipped view as in an unflipped one.
pub fn draw_image(image: &NSImage, rect: NSRect) {
    // SAFETY: no hints dictionary is passed; the image and rect are plain
    // values and the call draws into whatever context is current.
    unsafe {
        image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            rect,
            NSRect::ZERO,
            NSCompositingOperation::SourceOver,
            1.0,
            true,
            None,
        );
    }
}

/// `bitmap` encoded as PNG.
pub fn png(bitmap: &NSBitmapImageRep) -> Option<Retained<NSData>> {
    // SAFETY: an empty properties dictionary is a valid one for PNG.
    unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
}

/// A colour that is `pick(dark)` in whatever appearance it is drawn in, so it
/// follows a switch between light and dark instead of freezing to the one
/// current when it was made. `pick` is `Send + Sync`: an `NSColor` may be
/// sent to, and resolved on, any thread, and it calls `pick` there.
pub fn dynamic_color(
    pick: impl Fn(bool) -> Retained<NSColor> + Send + Sync + 'static,
) -> Retained<NSColor> {
    let provider = RcBlock::new(
        move |appearance: NonNull<NSAppearance>| -> NonNull<NSColor> {
            // SAFETY: AppKit passes a live appearance for the call's duration.
            let appearance = unsafe { appearance.as_ref() };
            let color = pick(crate::consts::is_dark(appearance));
            // The provider hands back an autoreleased colour, as its caller
            // expects; `autorelease_return` never returns null for a live object.
            NonNull::new(Retained::autorelease_return(color))
                .unwrap_or_else(|| NonNull::from(&*NSColor::clearColor()))
        },
    );
    // SAFETY: the provider returns a valid colour for any appearance.
    unsafe { NSColor::colorWithName_dynamicProvider(None, &provider) }
}
