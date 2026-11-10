//! Attributed text. An attribute dictionary is a bag of `key -> AnyObject`,
//! and AppKit trusts each value to be the type its key expects; [`Attrs`]
//! makes that a type the compiler checks, so building, drawing and measuring
//! text is safe.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::{
    NSBackgroundColorAttributeName, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSKernAttributeName, NSLinkAttributeName,
    NSParagraphStyle, NSParagraphStyleAttributeName, NSStringDrawing, NSStringDrawingOptions, NSStringNSExtendedStringDrawing,
};
use objc2_foundation::{NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString, NSNumber, NSRange, NSRect, NSString, NSURL};

/// The attributes the tray styles text with, each its own type.
#[derive(Default, Clone, Copy)]
pub struct Attrs<'a> {
    pub font: Option<&'a NSFont>,
    pub color: Option<&'a NSColor>,
    pub background: Option<&'a NSColor>,
    pub link: Option<&'a NSURL>,
    pub paragraph: Option<&'a NSParagraphStyle>,
}

impl Attrs<'_> {
    /// As the dictionary AppKit takes.
    pub fn dictionary(&self) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
        let mut keys: Vec<&NSAttributedStringKey> = Vec::new();
        let mut values: Vec<&AnyObject> = Vec::new();
        // SAFETY: AppKit constants, valid for the life of the process. Each
        // value pushed beside its key is the type that key documents.
        let names = unsafe { (NSFontAttributeName, NSForegroundColorAttributeName, NSBackgroundColorAttributeName, NSLinkAttributeName) };
        // SAFETY: as above.
        let paragraph_name = unsafe { NSParagraphStyleAttributeName };
        let (font_name, color_name, background_name, link_name) = names;
        if let Some(font) = self.font {
            keys.push(font_name);
            values.push(font);
        }
        if let Some(color) = self.color {
            keys.push(color_name);
            values.push(color);
        }
        if let Some(color) = self.background {
            keys.push(background_name);
            values.push(color);
        }
        if let Some(url) = self.link {
            keys.push(link_name);
            values.push(url);
        }
        if let Some(style) = self.paragraph {
            keys.push(paragraph_name);
            values.push(style);
        }
        NSDictionary::from_slices(&keys, &values)
    }
}

/// `text` with `attrs`.
pub fn attributed(text: &str, attrs: &Attrs) -> Retained<NSAttributedString> {
    let attributes = attrs.dictionary();
    // SAFETY: every value in `attributes` has the type its key expects (see
    // `Attrs::dictionary`).
    unsafe { NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &NSString::from_str(text), Some(&attributes)) }
}

/// Space the whole of `text`'s letters `kern` points further apart.
pub fn add_kern(text: &NSMutableAttributedString, kern: f64) {
    let value = NSNumber::new_f64(kern);
    // SAFETY: the kern attribute takes an `NSNumber`, and the range is the
    // whole string.
    unsafe { text.addAttribute_value_range(NSKernAttributeName, &value, NSRange::new(0, text.length())) };
}

/// Draw `text` in `rect` with `attrs`, one line, cut short with an ellipsis
/// if it does not fit (the paragraph style decides how).
pub fn draw_text(text: &str, rect: NSRect, attrs: &Attrs, options: NSStringDrawingOptions) {
    let attributes = attrs.dictionary();
    // SAFETY: every value in `attributes` has the type its key expects; no
    // drawing context object is passed.
    unsafe { NSString::from_str(text).drawWithRect_options_attributes_context(rect, options, Some(&attributes), None) };
}

/// How wide `text` is drawn with `attrs`, in points, rounded up.
pub fn text_width(text: &str, attrs: &Attrs) -> f64 {
    let attributes = attrs.dictionary();
    // SAFETY: every value in `attributes` has the type its key expects.
    unsafe { NSString::from_str(text).sizeWithAttributes(Some(&attributes)) }.width.ceil()
}
