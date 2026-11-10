//! `--self-test` and `--dump-views`: the real tray, built from a
//! [`fixture`](crate::fixture), driven with nobody at the keyboard.
//!
//! `--dump-views <fixture>` prints both layouts as text: every row, card,
//! pill, button and label they show, folded and unfolded.
//!
//! `--self-test` clicks every control the tray has -- the Regular menu and
//! its submenus, the Compact popover, the Settings, Closed Sessions and Stats
//! windows -- the way AppKit would, and prints what each click did: the
//! effects it asked for (recorded, never performed: see [`crate::effects`]),
//! the settings it saved, the windows it opened or closed, whether what is on
//! screen changed. A control that does nothing at all is a failure; so is a
//! delegate callback or the repaint timer not doing its job. The output is
//! compared from run to run, so it reads by outcome, not by how a control is
//! wired: clicks are made through the control's own target and action,
//! whatever those are.
//!
//! Both refuse to run without `SL_TRAY_DEFAULTS_SUITE`: the settings they
//! save must never be the ones the tray in actual use reads.

#![expect(clippy::print_stdout, clippy::print_stderr, clippy::exit, reason = "debug commands: they print their findings and exit with a status")]

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSControl, NSMenuDelegate, NSView, NSWindow};
use objc2_foundation::{MainThreadMarker, NSDate, NSObject, NSRunLoop, NSString};

use crate::controller::BarController;
use crate::effects::{self, Effect};
use crate::fixture::{self, Fixture};
use crate::inspect::{self, Target};
use crate::view_prefs::{self, Layout};

/// Run the main run loop for `secs`, as the app would between events.
fn pump(secs: f64) {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(secs));
}

/// The tray built from a fixture, and the handles a test pokes it through.
struct Harness {
    mtm: MainThreadMarker,
    controller: std::rc::Rc<BarController>,
    /// What the tray shows, as if the sessions thread had published it.
    board: crate::sessions::Board,
    /// The closed sessions as the fixture had them: the walks dismiss them,
    /// and the Closed Sessions window needs them back.
    closed_fixture: Vec<crate::sticky::Remembered>,
    suite: String,
}

/// The suite named by `SL_TRAY_DEFAULTS_SUITE`, emptied; else the reason
/// not to run.
fn sandbox() -> Result<String, String> {
    let suite = view_prefs::suite().ok_or_else(|| format!("set {}: this never touches the real settings", view_prefs::SUITE_ENV))?;
    view_prefs::defaults().removePersistentDomainForName(&NSString::from_str(&suite));
    Ok(suite)
}

impl Harness {
    fn new(mtm: MainThreadMarker, fixture: Fixture, suite: String) -> Self {
        let shared = crate::latest::new(fixture.snapshots);
        let sticky = crate::sticky::Sticky::from_json(&serde_json::to_string(&fixture.closed).unwrap_or_default());
        let sticky = Arc::new(Mutex::new(sticky));
        let closed_fixture = fixture.closed.clone();
        let board = crate::latest::new(crate::sessions::Published { open: fixture.open, closed: fixture.closed, stacks: fixture.stacks });
        let auto_awake = crate::awake::AutoCaffeinate::new(AtomicBool::new(crate::awake::load()));
        let controller = BarController::new(mtm, shared, Arc::clone(&board), auto_awake, sticky);
        controller.install();
        let harness = Self { mtm, controller, board, closed_fixture, suite };
        pump(0.1);
        harness
    }

    /// The fixture's closed sessions back, after a walk dismissed them, so
    /// the next surface has closed rows to click; a tick lists them.
    fn restore_closed(&self) {
        crate::latest::edit(&self.board, |published| published.closed.clone_from(&self.closed_fixture));
        pump(1.2);
    }

    /// The visible windows the tray opened, by title.
    fn windows(&self) -> Vec<Retained<NSWindow>> {
        NSApplication::sharedApplication(self.mtm).windows().to_vec().into_iter().filter(|w| w.isVisible() && !w.title().to_string().is_empty()).collect()
    }

    /// A window whose title starts with `title`, visible or not.
    fn window(&self, title: &str) -> Option<Retained<NSWindow>> {
        NSApplication::sharedApplication(self.mtm).windows().to_vec().into_iter().find(|w| w.title().to_string().starts_with(title))
    }

    fn regular(&self) -> Vec<String> {
        let mut out = Vec::new();
        inspect::describe_menu(&self.controller.menu(), 0, &mut out);
        out
    }

    fn compact(&self) -> Vec<String> {
        let mut out = Vec::new();
        inspect::describe_view(&self.controller.popover().0, 0, &mut out);
        out
    }

    /// Everything on screen, or that would be: the menu, the popover if it
    /// is up, every window.
    fn everything(&self) -> String {
        let mut out = self.regular();
        if self.controller.popover().1 {
            out.push("== popover".to_owned());
            out.extend(self.compact());
        }
        for window in self.windows() {
            inspect::describe_window(&window, &mut out);
        }
        out.join("\n")
    }

    /// The settings saved so far, each value cut short.
    fn prefs(&self) -> BTreeMap<String, String> {
        let mut prefs = BTreeMap::new();
        let Some(domain) = view_prefs::defaults().persistentDomainForName(&NSString::from_str(&self.suite)) else { return prefs };
        for key in domain.allKeys().to_vec() {
            let value = domain.objectForKey(&key).and_then(|value| value.downcast::<NSObject>().ok()).map(|value| format!("{value:?}")).unwrap_or_default();
            let value = if value.chars().count() > 60 { format!("{}… ({} chars)", value.chars().take(40).collect::<String>(), value.chars().count()) } else { value };
            prefs.insert(key.to_string(), value);
        }
        prefs
    }

    fn probe(&self) -> Probe {
        Probe {
            screen: self.everything(),
            prefs: self.prefs(),
            windows: self.windows().iter().map(|w| w.title().to_string()).collect(),
            popover: self.controller.popover().1,
        }
    }

    /// Show the popover if it is not up, by clicking the menu bar icon as a
    /// person would -- once the click that closed it is long enough ago.
    fn ensure_popover(&self) {
        if self.controller.popover().1 {
            return;
        }
        pump(0.35);
        if let Some(button) = self.controller.status_button() {
            inspect::fire(&Target::Control(Retained::into_super(Retained::into_super(button)), None));
            pump(0.05);
        }
    }

    /// Click every control `collect` finds, once each, re-collecting after
    /// every click since a click may rebuild what is on screen. `prepare`
    /// runs before each collection.
    fn walk(&self, collect: &dyn Fn(&Self) -> Vec<(String, Target)>, prepare: &dyn Fn(&Self), fired: &mut HashSet<String>, log: &mut Log) {
        for _ in 0..2000 {
            prepare(self);
            let mut controls = collect(self);
            inspect::number_repeats(&mut controls);
            // Dismissals last: they take away what the rest are about.
            controls.sort_by_key(|(path, _)| path.ends_with("/ Dismiss") || path.contains("/ Dismiss #"));
            let Some((path, target)) = controls.into_iter().find(|(path, _)| !fired.contains(path)) else { return };
            fired.insert(path.clone());
            let before = self.probe();
            effects::take();
            inspect::fire(&target);
            pump(0.02);
            let after = self.probe();
            let outcome = outcome(&before, &after, &effects::take());
            if outcome == "nothing" {
                log.fail(format!("{path}: the click did nothing"));
            }
            log.line(format!("{path} => {outcome}"));
        }
        log.fail("a surface kept growing new controls: stopped after 2000 clicks".to_owned());
    }
}

/// What the self-test looks at before and after a click.
struct Probe {
    screen: String,
    prefs: BTreeMap<String, String>,
    windows: Vec<String>,
    popover: bool,
}

/// What a click did, from what changed: `nothing` when nothing did.
fn outcome(before: &Probe, after: &Probe, effects: &[Effect]) -> String {
    let mut parts: Vec<String> = effects.iter().map(Effect::to_string).collect();
    for (key, value) in &after.prefs {
        if before.prefs.get(key) != Some(value) {
            parts.push(format!("set {key}={value}"));
        }
    }
    for key in before.prefs.keys().filter(|key| !after.prefs.contains_key(*key)) {
        parts.push(format!("unset {key}"));
    }
    for title in after.windows.iter().filter(|title| !before.windows.contains(title)) {
        parts.push(format!("opened {title:?}"));
    }
    for title in before.windows.iter().filter(|title| !after.windows.contains(title)) {
        parts.push(format!("closed {title:?}"));
    }
    match (before.popover, after.popover) {
        (false, true) => parts.push("popover opened".to_owned()),
        (true, false) => parts.push("popover closed".to_owned()),
        _ => {}
    }
    if parts.is_empty() && before.screen != after.screen {
        parts.push("view changed".to_owned());
    }
    if parts.is_empty() { "nothing".to_owned() } else { parts.join("; ") }
}

/// The transcript and the failures.
#[derive(Default)]
struct Log {
    lines: Vec<String>,
    failures: Vec<String>,
}

impl Log {
    fn line(&mut self, line: String) {
        self.lines.push(line);
    }

    fn fail(&mut self, failure: String) {
        self.lines.push(format!("FAIL {failure}"));
        self.failures.push(failure);
    }

    fn check(&mut self, name: &str, ok: bool) {
        if ok {
            self.line(format!("check {name}: ok"));
        } else {
            self.fail(format!("check {name}"));
        }
    }
}

/// The fixture `spec`, or exit saying why not.
fn fixture_or_exit(spec: &str) -> Fixture {
    fixture::load(spec).unwrap_or_else(|error| {
        eprintln!("sl-tray: {error}");
        std::process::exit(2);
    })
}

fn suite_or_exit() -> String {
    sandbox().unwrap_or_else(|error| {
        eprintln!("sl-tray: {error}");
        std::process::exit(2);
    })
}

/// The top-level rows of the menu that fold a group open or shut.
fn folds(harness: &Harness) -> Vec<(String, Target)> {
    let menu = harness.controller.menu();
    let mut out = Vec::new();
    for item in menu.itemArray().to_vec() {
        if item.isHidden() || item.submenu().is_some() {
            continue;
        }
        if let Some(view) = item.view()
            && let Some(row) = crate::menu_rows::RowView::from_view(&view)
            && row.action().is_some()
        {
            out.push((row.label(), Target::Control(Retained::into_super(row.retain()), None)));
        }
    }
    out
}

/// The popover's cards, by path.
fn cards(harness: &Harness) -> Vec<(String, Target)> {
    let mut controls = Vec::new();
    inspect::controls_in_view(&harness.controller.popover().0, "compact", &mut controls);
    controls.retain(|(_, target)| matches!(target, Target::Control(control, _) if crate::compact::CardView::from_view(control).is_some()));
    inspect::number_repeats(&mut controls);
    controls
}

/// `--dump-views <fixture>`: both layouts as text, then exit.
pub(crate) fn dump_views(mtm: MainThreadMarker, spec: &str) -> ! {
    let suite = suite_or_exit();
    effects::record(false);
    let harness = Harness::new(mtm, fixture_or_exit(spec), suite);
    let mut out = vec![format!("# fixture {spec}"), "== regular".to_owned()];
    harness.controller.set_layout(Layout::Regular);
    out.extend(harness.regular());
    for (_, fold) in folds(&harness) {
        inspect::fire(&fold);
    }
    out.push("== regular, folds open".to_owned());
    out.extend(harness.regular());

    harness.controller.set_layout(Layout::Compact);
    harness.ensure_popover();
    out.push("== compact".to_owned());
    out.extend(harness.compact());
    let paths: Vec<String> = cards(&harness).into_iter().map(|(path, _)| path).collect();
    for path in paths {
        harness.ensure_popover();
        let Some((_, card)) = cards(&harness).into_iter().find(|(p, _)| *p == path) else { continue };
        inspect::fire(&card);
        pump(0.02);
        out.push(format!("== {path}, clicked"));
        out.extend(harness.compact());
        // Fold it again, unless it was the Closed group, which stays open
        // so its own cards come up next.
        if !path.contains("card Closed") {
            harness.ensure_popover();
            if let Some((_, card)) = cards(&harness).into_iter().find(|(p, _)| *p == path) {
                inspect::fire(&card);
            }
        }
    }
    view_prefs::discard_suite(&harness.suite);
    println!("{}", out.join("\n"));
    std::process::exit(0);
}

/// `--self-test`: click everything on the `full` fixture, print what each
/// click did, and exit non-zero listing whatever failed.
pub(crate) fn self_test(mtm: MainThreadMarker) -> ! {
    let suite = suite_or_exit();
    effects::record(true);
    let harness = Harness::new(mtm, fixture_or_exit("full"), suite);
    let mut log = Log::default();
    let mut fired = HashSet::new();

    harness.controller.set_layout(Layout::Regular);
    check_menu_delegate(&harness, &mut log);
    log.line("== regular".to_owned());
    harness.walk(
        &|h| {
            let mut controls = Vec::new();
            inspect::controls_in_menu(&h.controller.menu(), "regular", &mut controls);
            controls
        },
        &|_| {},
        &mut fired,
        &mut log,
    );

    log.line("== compact".to_owned());
    harness.restore_closed();
    harness.controller.set_layout(Layout::Compact);
    check_popover_guard(&harness, &mut log);
    harness.walk(
        &|h| {
            let mut controls = Vec::new();
            inspect::controls_in_view(&h.controller.popover().0, "compact", &mut controls);
            controls
        },
        &|h| {
            if view_prefs::load_layout() != Layout::Compact {
                h.controller.set_layout(Layout::Compact);
            }
            h.ensure_popover();
        },
        &mut fired,
        &mut log,
    );

    for (title, name) in [("Statusline Tray Settings", "settings"), ("Closed Sessions", "closed"), ("Claude Code Usage", "stats")] {
        log.line(format!("== {name}"));
        let Some(window) = harness.window(title) else {
            log.fail(format!("the {name} window never opened"));
            continue;
        };
        window.orderFront(None);
        if name == "closed" {
            harness.restore_closed();
            check_closed_search(&window, &mut log);
            check_closed_dismiss(&harness, &window, &mut log);
            harness.restore_closed();
        }
        if name == "stats" {
            check_stats_window(&harness, &window, &mut log);
        }
        harness.walk(
            &|_| {
                let mut controls = Vec::new();
                if let Some(toolbar) = window.toolbar() {
                    for item in toolbar.items().to_vec() {
                        if let Some(view) = item.view() {
                            inspect::controls_in_view(&view, name, &mut controls);
                        }
                    }
                }
                if let Some(content) = window.contentView() {
                    inspect::controls_in_view(&content, name, &mut controls);
                }
                // A search field with nothing typed searches for nothing;
                // `check_closed_search` types into it instead.
                controls.retain(|(_, target)| !matches!(target, Target::Control(control, _) if control.downcast_ref::<objc2_app_kit::NSSearchField>().is_some()));
                controls
            },
            &|_| window.orderFront(None),
            &mut fired,
            &mut log,
        );
    }

    view_prefs::discard_suite(&harness.suite);
    println!("{}", log.lines.join("\n"));
    if log.failures.is_empty() {
        std::process::exit(0);
    }
    eprintln!("sl-tray: self-test failed:\n  {}", log.failures.join("\n  "));
    std::process::exit(1);
}

/// `menuWillOpen:` repaints at once; the timer keeps firing while the menu
/// is tracked (gotcha #1); after `menuDidClose:` a tick repaints again.
fn check_menu_delegate(harness: &Harness, log: &mut Log) {
    let menu = harness.controller.menu();
    let Some(delegate) = menu.delegate() else {
        log.fail("the menu has no delegate".to_owned());
        return;
    };
    let mut late = crate::latest::get(&harness.board).open[0].clone();
    late.session_id = "late-id".into();
    late.name = "late arrival".into();
    crate::latest::edit(&harness.board, |published| published.open.push(late.clone()));
    delegate.menuWillOpen(&menu);
    log.check("menuWillOpen: repaints at once", harness.regular().iter().any(|line| line.contains(&late.name)));

    let ticks = crate::controller::TICKS.load(Ordering::Relaxed);
    let until = NSDate::dateWithTimeIntervalSinceNow(2.5);
    let tracking = sl_macos::consts::tracking_run_loop_mode();
    while NSDate::now().timeIntervalSinceDate(&until) < 0.0 {
        NSRunLoop::currentRunLoop().runMode_beforeDate(tracking, &until);
    }
    log.check("the timer fires while the menu is tracked", crate::controller::TICKS.load(Ordering::Relaxed) > ticks);

    delegate.menuDidClose(&menu);
    crate::latest::edit(&harness.board, |published| published.open.retain(|session| session.session_id != late.session_id));
    pump(1.3);
    log.check("after menuDidClose:, a tick repaints", !harness.regular().iter().any(|line| line.contains(&late.name)));
}

/// The click on the menu bar icon that closed the popover does not open it
/// again; a later one does.
fn check_popover_guard(harness: &Harness, log: &mut Log) {
    harness.ensure_popover();
    log.check("the menu bar icon opens the popover", harness.controller.popover().1);
    harness.controller.close_popover();
    if let Some(button) = harness.controller.status_button() {
        let target = Target::Control(Retained::into_super(Retained::into_super(button)), None);
        inspect::fire(&target);
        pump(0.02);
        log.check("a click as the popover closes does not reopen it", !harness.controller.popover().1);
        pump(0.35);
        inspect::fire(&target);
        pump(0.05);
        log.check("a later click reopens it", harness.controller.popover().1);
    }
}

/// Typing in the search field narrows the list.
fn check_closed_search(window: &NSWindow, log: &mut Log) {
    let Some(content) = window.contentView() else { return };
    let Some(field) = find::<objc2_app_kit::NSSearchField>(&content) else {
        log.fail("the Closed Sessions window has no search field".to_owned());
        return;
    };
    let shown = || {
        let mut out = Vec::new();
        inspect::describe_window(window, &mut out);
        out.join("\n")
    };
    let before = shown();
    field.setStringValue(&NSString::from_str("drafts"));
    let control: &NSControl = &field;
    inspect::fire(&Target::Control(control.retain(), None));
    pump(0.02);
    let after = shown();
    log.check("searching closed sessions narrows the list", before != after && !after.contains("fix flaky ingest test"));
    field.setStringValue(&NSString::new());
    inspect::fire(&Target::Control(control.retain(), None));
    // Painted now, so the next check starts from the whole list.
    pump(0.02);
}

/// Dismiss in the Closed Sessions window forgets the session on its row --
/// gone from the window and from what is published -- and only that one.
fn check_closed_dismiss(harness: &Harness, window: &NSWindow, log: &mut Log) {
    let Some(content) = window.contentView() else { return };
    let shown = || {
        let mut out = Vec::new();
        inspect::describe_window(window, &mut out);
        out.join("\n")
    };
    let before = shown();
    // The session on the first row: the fixture's closed session named first.
    let Some(first) = harness.closed_fixture.iter().filter_map(|entry| Some((before.find(entry.name.as_str())?, entry))).min_by_key(|(at, _)| *at).map(|(_, entry)| entry.clone()) else {
        log.fail("the Closed Sessions window lists no closed session".to_owned());
        return;
    };
    let others: Vec<_> = harness.closed_fixture.iter().filter(|entry| entry.session_id != first.session_id).cloned().collect();
    let mut controls = Vec::new();
    inspect::controls_in_view(&content, "closed", &mut controls);
    let Some((_, dismiss)) = controls.into_iter().find(|(path, _)| path == "closed / Dismiss") else {
        log.fail("the Closed Sessions window has no Dismiss button".to_owned());
        return;
    };
    inspect::fire(&dismiss);
    pump(0.05);
    let after = shown();
    let published: Vec<String> = crate::latest::get(&harness.board).closed.iter().map(|entry| entry.session_id.clone()).collect();
    log.check("Dismiss in Closed Sessions forgets that row's session", !after.contains(first.name.as_str()) && !published.contains(&first.session_id));
    log.check(
        "and keeps every other one",
        !others.is_empty() && others.iter().all(|entry| after.contains(entry.name.as_str()) && published.contains(&entry.session_id)),
    );
}

/// Resizing reflows the Stats window; switching its appearance redraws it.
fn check_stats_window(harness: &Harness, window: &NSWindow, log: &mut Log) {
    let toolbar_items = window.toolbar().map_or(0, |toolbar| toolbar.items().count());
    log.check("the Stats toolbar has its items", toolbar_items >= 1);
    let width = |h: &Harness| h.everything().lines().find(|line| line.contains("stats ")).map(str::to_owned);
    let before = width(harness);
    let mut frame = window.frame();
    frame.size.width += 120.0;
    window.setFrame_display(frame, true);
    pump(0.05);
    log.check("resizing the Stats window reflows it", width(harness) != before);
    let dark = sl_macos::consts::appearance(sl_macos::consts::Appearance::Dark);
    objc2_app_kit::NSAppearanceCustomization::setAppearance(window, dark.as_deref());
    pump(0.05);
    objc2_app_kit::NSAppearanceCustomization::setAppearance(window, None);
    log.check("the Stats window survives an appearance switch", window.isVisible());
}

/// The first view of type `T` under `view`.
fn find<T: objc2::DowncastTarget + objc2::Message>(view: &NSView) -> Option<Retained<T>> {
    if let Some(found) = view.downcast_ref::<T>() {
        return Some(found.retain());
    }
    view.subviews().to_vec().iter().find_map(|child| find::<T>(child))
}

use objc2::Message;
