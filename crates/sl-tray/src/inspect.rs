//! The tray's surfaces as text: what each menu row, card, button and label
//! shows, read back from the live views. For `--dump-views` and the
//! self-test, which compare it from run to run, so it must come out the same
//! for the same state -- and stay the same when the code drawing it is
//! reorganised, which is why it is written out field by field here rather
//! than taken from a `Debug` derive.

use std::fmt::Write as _;
use objc2::Message;
use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSControl, NSImageView, NSMenu, NSMenuItem, NSScrollView, NSSegmentedControl, NSTabView, NSTextField, NSView, NSWindow};

use crate::menu_rows::{ActionContent, HeaderContent, RowContent};
use crate::sessions::{Seg, Tone};

/// `text`, with any tone but plain marked: `DN-479 ‹Dim:#753›`.
pub(crate) fn segs(segs: &[Seg]) -> String {
    segs.iter().map(|seg| toned(&seg.text, seg.tone)).collect()
}

/// `text` marked with `tone` unless it is plain.
pub(crate) fn toned(text: &str, tone: Tone) -> String {
    match tone {
        Tone::Plain => text.to_owned(),
        tone => format!("‹{tone:?}:{text}›"),
    }
}

pub(crate) fn row(content: &RowContent) -> String {
    let mut out = String::from("row");
    if let Some(icon) = content.icon {
        write!(out, " [{icon:?}]").ok();
    }
    if let Some((used, tone)) = content.ring {
        write!(out, " ring {used}/{tone:?}").ok();
    }
    write!(out, " {:?}", segs(&content.title)).ok();
    if !content.detail.is_empty() {
        write!(out, " | {:?}", segs(&content.detail)).ok();
    }
    if let Some(mark) = content.mark {
        write!(out, " mark={mark:?}").ok();
    }
    if let Some((text, tone)) = &content.right {
        write!(out, " right={:?}", toned(text, *tone)).ok();
    }
    match content.chevron {
        Some(true) => out.push_str(" ⌄"),
        Some(false) => out.push_str(" ›"),
        None => {}
    }
    if content.tinted {
        out.push_str(" tinted");
    }
    if content.dimmed {
        out.push_str(" dimmed");
    }
    out
}

pub(crate) fn action(content: &ActionContent) -> String {
    let mut out = String::from("action");
    if let Some(symbol) = content.symbol {
        match content.symbol_tone {
            Some(tone) => write!(out, " ({symbol} {tone:?})"),
            None => write!(out, " ({symbol})"),
        }
        .ok();
    }
    write!(out, " {:?}", segs(&content.title)).ok();
    if content.chevron {
        out.push_str(" ›");
    }
    if !content.enabled {
        out.push_str(" disabled");
    }
    out
}

pub(crate) fn header(content: &HeaderContent) -> String {
    let mut out = String::from("header");
    if let Some((symbol, tone)) = content.symbol {
        write!(out, " ({symbol} {tone:?})").ok();
    }
    write!(out, " {:?}", content.title).ok();
    for pill in &content.pills {
        write!(out, " [{}]", toned(&pill.text, pill.tone)).ok();
    }
    if let Some(pill) = &content.end_pill {
        write!(out, " … [{}]", toned(&pill.text, pill.tone)).ok();
    }
    for line in &content.lines {
        write!(out, " / {:?}", segs(line)).ok();
    }
    out
}

/// What a custom view says of itself, for the views this crate draws; `None`
/// for any other view.
fn custom(view: &NSView) -> Option<String> {
    use crate::{compact, menu_rows, meters};
    if let Some(view) = menu_rows::RowView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = menu_rows::ActionView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = menu_rows::HeaderView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = menu_rows::StatusView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = menu_rows::ToggleButton::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = meters::MeterView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = meters::MetersView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = compact::CardView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = compact::TintView::from_view(view) {
        return Some(view.describe());
    }
    if let Some(view) = crate::stats::StatsView::from_view(view) {
        return Some(view.describe());
    }
    None
}

/// Custom views whose subviews only render what [`custom`] already said.
fn opaque(view: &NSView) -> bool {
    crate::menu_rows::RowView::from_view(view).is_some()
        || crate::menu_rows::ActionView::from_view(view).is_some()
        || crate::menu_rows::HeaderView::from_view(view).is_some()
}

/// ` ⟶` for a control that sends an action when clicked.
fn clickable(view: &NSView) -> &'static str {
    match view.downcast_ref::<NSControl>() {
        Some(control) if control.action().is_some() => " ⟶",
        _ => "",
    }
}

/// A one-line description of `view` itself, without its subviews; `None`
/// for a view that shows nothing of its own (a plain container).
pub(crate) fn line(view: &NSView) -> Option<String> {
    if let Some(text) = custom(view) {
        return Some(text);
    }
    if let Some(control) = view.downcast_ref::<NSSegmentedControl>() {
        let labels: Vec<String> = (0..control.segmentCount())
            .map(|i| {
                let label = control.labelForSegment(i).map(|label| label.to_string()).unwrap_or_default();
                if control.isSelectedForSegment(i) { format!("*{label}") } else { label }
            })
            .collect();
        return Some(format!("segmented [{}]", labels.join(" | ")));
    }
    if let Some(button) = view.downcast_ref::<NSButton>() {
        let title = button.title().to_string();
        let tip = button.toolTip().map(|tip| tip.to_string()).unwrap_or_default();
        let state = if button.state() == objc2_app_kit::NSControlStateValueOn { " on" } else { "" };
        let enabled = if button.isEnabled() { "" } else { " disabled" };
        return Some(match (title.is_empty(), tip.is_empty()) {
            (false, _) => format!("button {title:?}{state}{enabled}"),
            (true, false) => format!("button ({tip}){state}{enabled}"),
            (true, true) => format!("button{state}{enabled}"),
        });
    }
    if let Some(field) = view.downcast_ref::<NSTextField>() {
        let text = field.stringValue().to_string();
        return (!text.is_empty()).then(|| format!("text {text:?}"));
    }
    if view.downcast_ref::<NSImageView>().is_some() {
        return Some("image".to_owned());
    }
    None
}

/// The subviews worth walking: a scroll view's document, every tab of a tab
/// view (not only the selected one), else the plain subviews.
fn children(view: &NSView) -> Vec<Retained<NSView>> {
    if let Some(scroll) = view.downcast_ref::<NSScrollView>() {
        return scroll.documentView().into_iter().collect();
    }
    if let Some(tabs) = view.downcast_ref::<NSTabView>() {
        return tabs.tabViewItems().to_vec().iter().filter_map(|item| item.view(objc2::MainThreadMarker::from(view))).collect();
    }
    view.subviews().to_vec()
}

/// `view` and everything under it, one line per view that shows something,
/// indented by depth. Hidden views are left out, as they are off screen.
pub(crate) fn describe_view(view: &NSView, depth: usize, out: &mut Vec<String>) {
    if view.isHidden() {
        return;
    }
    let mut depth = depth;
    if let Some(text) = line(view) {
        out.push(format!("{}{text}{}", "  ".repeat(depth), clickable(view)));
        depth += 1;
    }
    if opaque(view) {
        return;
    }
    for child in children(view) {
        describe_view(&child, depth, out);
    }
}

/// `menu`'s visible items and their submenus, as [`describe_view`] does.
pub(crate) fn describe_menu(menu: &NSMenu, depth: usize, out: &mut Vec<String>) {
    for item in menu.itemArray().to_vec() {
        describe_item(&item, depth, out);
    }
}

fn describe_item(item: &NSMenuItem, depth: usize, out: &mut Vec<String>) {
    if item.isHidden() {
        return;
    }
    let pad = "  ".repeat(depth);
    if item.isSeparatorItem() {
        out.push(format!("{pad}---"));
    } else if let Some(view) = item.view() {
        describe_view(&view, depth, out);
    } else {
        let action = if item.action().is_some() { " ⟶" } else { "" };
        let enabled = if item.isEnabled() { "" } else { " disabled" };
        out.push(format!("{pad}item {:?}{action}{enabled}", item.title().to_string()));
    }
    if let Some(submenu) = item.submenu() {
        describe_menu(&submenu, depth + 1, out);
    }
}

/// A window's toolbar and content.
pub(crate) fn describe_window(window: &NSWindow, out: &mut Vec<String>) {
    out.push(format!("window {:?}", window.title().to_string()));
    if let Some(toolbar) = window.toolbar() {
        for item in toolbar.items().to_vec() {
            match item.view() {
                Some(view) => describe_view(&view, 1, out),
                None => out.push(format!("  toolbar {:?}", item.itemIdentifier().to_string())),
            }
        }
    }
    if let Some(content) = window.contentView() {
        describe_view(&content, 1, out);
    }
}

// --------------------------------------------------------------------------
// Controls, for the self-test to click
// --------------------------------------------------------------------------

/// Something the self-test can click.
#[derive(Clone)]
pub(crate) enum Target {
    /// A control with an action; for a segmented control, one segment.
    Control(Retained<NSControl>, Option<isize>),
    /// A plain menu item with an action, by its index in its menu.
    Item(Retained<NSMenu>, isize),
}

/// What a control is called in a path: [`raw_label`] without the counts
/// that a click can change (`Closed · 3`, `All 12`), so a fold keeps one
/// name as it folds and unfolds.
fn label(view: &NSView) -> String {
    strip_counts(&raw_label(view))
}

/// `raw` without counts: a number after a `·`, or one that ends it.
fn strip_counts(raw: &str) -> String {
    let words: Vec<&str> = raw.split(' ').collect();
    let mut kept: Vec<&str> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let count = !word.is_empty() && word.chars().all(|c| c.is_ascii_digit());
        let after_dot = i > 0 && words[i - 1] == "·";
        if count && (after_dot || (i > 0 && i + 1 == words.len())) {
            if after_dot {
                kept.pop();
            }
            continue;
        }
        kept.push(word);
    }
    kept.join(" ").trim().to_owned()
}

/// What a control is called: its title, else what it shows.
fn raw_label(view: &NSView) -> String {
    if let Some(button) = view.downcast_ref::<NSButton>() {
        let title = button.title().to_string();
        if !title.is_empty() {
            return title;
        }
        if let Some(tip) = button.toolTip() {
            return tip.to_string();
        }
    }
    if let Some(button) = crate::menu_rows::ToggleButton::from_view(view) {
        return button.tip();
    }
    if let Some(row) = crate::menu_rows::RowView::from_view(view) {
        return row.label();
    }
    if let Some(action) = crate::menu_rows::ActionView::from_view(view) {
        return action.label();
    }
    if crate::compact::CardView::from_view(view).is_some() {
        // A card is called by its first line of text.
        let mut lines = Vec::new();
        for child in view.subviews().to_vec() {
            describe_view(&child, 0, &mut lines);
            if let Some(text) = lines.iter().find_map(|line| line.strip_prefix("text ")) {
                return format!("card {}", text.trim_matches('"'));
            }
        }
        return "card".to_owned();
    }
    line(view).unwrap_or_else(|| "view".to_owned())
}

/// `count` choices in the order a run of clicks changes something with
/// each: from the one after `current`, round to `current` itself.
fn rotated(count: usize, current: Option<usize>) -> Vec<usize> {
    let start = current.map_or(0, |current| current + 1);
    (0..count).map(|i| (start + i) % count.max(1)).collect()
}

/// Every clickable control in and under `view`, each with a path: `prefix`,
/// then its label. A segmented control yields one target per segment, and
/// the meters one per page, ordered so each click changes the choice.
pub(crate) fn controls_in_view(view: &NSView, prefix: &str, out: &mut Vec<(String, Target)>) {
    if view.isHidden() {
        return;
    }
    let prefix = prefix.to_owned();
    if let Some(control) = view.downcast_ref::<NSControl>()
        && control.action().is_some()
    {
        let name = format!("{prefix} / {}", label(view));
        if crate::compact::CardView::from_view(view).is_some() {
            // A card's buttons first, named under it: clicking the card
            // folds it, and its label can change as it unfolds.
            for child in children(view) {
                controls_in_view(&child, &name, out);
            }
            out.push((name, Target::Control(control.retain(), None)));
            return;
        }
        if let Some(segmented) = view.downcast_ref::<NSSegmentedControl>() {
            let count = usize::try_from(segmented.segmentCount()).unwrap_or(0);
            let current = usize::try_from(segmented.selectedSegment()).ok();
            for i in rotated(count, current) {
                let segment = segmented.labelForSegment(isize::try_from(i).unwrap_or(isize::MAX)).map(|label| label.to_string()).unwrap_or_default();
                out.push((format!("{prefix} / [{}]", strip_counts(&segment)), Target::Control(control.retain(), Some(isize::try_from(i).unwrap_or(isize::MAX)))));
            }
        } else if let Some(meters) = crate::meters::MetersView::from_view(view) {
            if let Some((pages, current)) = meters.pages() {
                for page in rotated(pages, Some(current)) {
                    out.push((format!("{prefix} / page {}", page + 1), Target::Control(control.retain(), Some(isize::try_from(page).unwrap_or(isize::MAX)))));
                }
            }
        } else if crate::menu_rows::ActionView::from_view(view).is_some_and(|row| !row.is_enabled()) {
            // A disabled row takes no clicks.
        } else {
            out.push((name.clone(), Target::Control(control.retain(), None)));
        }
    }
    if opaque(view) {
        return;
    }
    for child in children(view) {
        controls_in_view(&child, &prefix, out);
    }
}

/// Every clickable control in `menu` and its submenus, the paths running
/// through the rows that open each submenu.
pub(crate) fn controls_in_menu(menu: &NSMenu, prefix: &str, out: &mut Vec<(String, Target)>) {
    for (index, item) in menu.itemArray().to_vec().iter().enumerate() {
        if item.isHidden() || item.isSeparatorItem() {
            continue;
        }
        let name = if let Some(view) = item.view() {
            controls_in_view(&view, prefix, out);
            format!("{prefix} / {}", label(&view))
        } else {
            let name = format!("{prefix} / {}", item.title());
            if item.action().is_some() {
                out.push((name.clone(), Target::Item(menu.retain(), isize::try_from(index).unwrap_or(isize::MAX))));
            }
            name
        };
        if let Some(submenu) = item.submenu() {
            controls_in_menu(&submenu, &name, out);
        }
    }
}

/// Make each path unique, numbering repeats in order: `… / Zed`, `… / Zed #2`.
pub(crate) fn number_repeats(controls: &mut [(String, Target)]) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (path, _) in controls.iter_mut() {
        let count = seen.entry(path.clone()).or_default();
        *count += 1;
        if *count > 1 {
            write!(path, " #{count}").ok();
        }
    }
}

/// Click `target` the way AppKit does: a button is clicked (a checkbox
/// flips first), a segment is selected, any other control sends its action
/// to its target, and a menu item is performed by its menu.
pub(crate) fn fire(target: &Target) {
    match target {
        Target::Control(control, segment) => {
            if let Some(button) = control.downcast_ref::<NSButton>() {
                sl_macos::system::perform_click(button);
                return;
            }
            if let Some(segment) = segment {
                match control.downcast_ref::<NSSegmentedControl>() {
                    Some(segmented) => segmented.setSelectedSegment(*segment),
                    // The meters: the page clicked, as their mouse-down sets it.
                    None => control.setTag(*segment),
                }
            }
            sl_macos::action::fire(control);
        }
        Target::Item(menu, index) => menu.performActionForItemAtIndex(*index),
    }
}
