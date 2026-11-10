//! `statusline-tray`: a native macOS menu bar item showing Claude Code usage.
//!
//! The menu's sections -- Usage Limits, Today, This Week, Current Block,
//! Sessions, Costs and Models -- are each folded once on the
//! background thread (`snapshot`), projected against a live clock at repaint
//! time (`predictions`) and rendered as `NSMenu` submenus (`controller`).
//!
//! Usage Limits, and the menu bar title itself, come from a different place
//! than everything else: the status-line hook writes Claude Code's own
//! `rate_limits` to disk on every turn and `limits` reads it back. There is no
//! Keychain access and no network call anywhere in this workspace, so a
//! capture only refreshes while Claude Code is actually running -- which is
//! why the title marks a stale one rather than presenting a frozen percentage
//! as live.
//!
//! Threading: one background thread owns every call into `sl_usage` and
//! publishes a plain owned [`Snapshot`] into an `Arc<Mutex<_>>`; the main
//! thread owns every call into AppKit and only ever reads that mutex. The
//! `MainThreadMarker` the AppKit bindings demand makes half of that split a
//! compile-time guarantee.
//!
//! ## Debug CLI
//!
//! The plan's entire reporting/debug surface, per [`Flags`]: `--dump-snapshot`
//! (print the `Snapshot` as JSON and exit, no AppKit touched at all -- this is
//! the one path in the crate that a headless environment can actually run) and
//! `--rebuild` (force the cache refresh this run uses to be a full rebuild
//! instead of the normal incremental one).

#![forbid(unsafe_code)]

mod act;
mod actions;
mod awake;
mod cache_ttl;
mod compact;
mod closed_window;
mod codex_sessions;
mod controller;
mod diskio;
mod effects;
mod inspect;
mod latest;
mod feedback;
mod fixture;
mod limits;
mod linear;
mod markdown;
mod menu_rows;
mod meters;
mod notify;
mod predictions;
mod refresh;
mod self_test;
mod sessions;
mod settings_window;
mod snapshot;
mod stacks;
mod stats;
mod sticky;
mod titles;
mod tray_model;
mod view_prefs;
mod provider_marks;
mod zed_logo;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;
use sl_usage::PricingMap;

use crate::controller::BarController;
use crate::snapshot::{Snapshot, Snapshots, now_ms};

/// Print a diagnostic line, but only under `SL_TRAY_DEBUG=1`.
///
/// This exists because the two AppKit lifetime rules this crate is built
/// around (see `controller`'s module docs) are both silent when violated: a
/// timer in the wrong run loop mode just stops firing while the menu is open,
/// and an unretained delegate just never gets called. Neither produces a crash
/// or a warning, so a trace of every tick is the cheapest way for a human with
/// a real menu bar to confirm the mitigations actually work.
pub fn trace(message: &str) {
    // Read per call rather than caching: this is a once-a-second path, and a
    // `OnceLock` here would buy nothing measurable.
    if std::env::var_os("SL_TRAY_DEBUG").is_some_and(|value| value != "0") {
        log(message);
    }
}

/// A line in the tray's log: its stderr, which the login item sends to
/// `~/Library/Logs/statusline-tray.log`.
#[expect(clippy::print_stderr, reason = "stderr is the tray's log")]
pub fn log(message: &str) {
    eprintln!("sl-tray: {message}");
}

/// The debug CLI's boolean flags: `--dump-snapshot`, `--rebuild` and
/// `--test-notification`. The two preview commands, which take arguments, are
/// parsed on their own: `--stats-preview` and `--icon-preview`, for looking at
/// the Stats window and the menu bar icon while working on them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "one per independent command-line flag")]
struct Flags {
    /// Print the `Snapshot` as JSON and exit, before any AppKit setup.
    dump_snapshot: bool,
    /// Which view `--dump-snapshot` prints: `claude` (the default), `codex`
    /// or `all`, given right after the flag.
    dump_provider: view_prefs::Provider,
    /// Discard the usage store and re-read every transcript
    /// (`RefreshMode::Rebuild`) instead of the normal incremental refresh, for
    /// the one refresh this run's flag applies to -- see
    /// `refresh::load_and_build` and `refresh::spawn`.
    rebuild: bool,
    /// Send one notification and exit, to check native delivery end to end.
    test_notification: bool,
    /// Print the open Codex threads as the sessions poll would see them, and
    /// exit. Read-only; for checking the detection against a live `codex`.
    dump_codex_sessions: bool,
}

/// Parse `--dump-snapshot`/`--rebuild` out of an argument list (already
/// stripped of `argv[0]`). Unrecognized arguments are ignored rather than
/// rejected -- this is an internal debug surface, not a public CLI contract --
/// and repeating a flag is harmless (`bool` has no "twice" state).
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Flags {
    let mut flags = Flags::default();
    let mut after_dump = false;
    for arg in args {
        if std::mem::take(&mut after_dump)
            && let Some(provider) = view_prefs::Provider::parse(&arg)
        {
            flags.dump_provider = provider;
            continue;
        }
        match arg.as_str() {
            "--dump-snapshot" => {
                flags.dump_snapshot = true;
                after_dump = true;
            }
            "--rebuild" => flags.rebuild = true,
            "--test-notification" => flags.test_notification = true,
            "--dump-codex-sessions" => flags.dump_codex_sessions = true,
            _ => {}
        }
    }
    flags
}

/// `--dump-snapshot`: build one snapshot synchronously and print it as JSON.
///
/// Deliberately called from `main` *before* `MainThreadMarker::new()` or any
/// other AppKit setup -- this is the one command in the whole plan a headless
/// environment (no display, no menu bar) can actually run and verify output
/// from, and it must stay that way. Reuses `refresh::load_and_build` so the
/// cache-loading logic lives in exactly one place.
#[expect(clippy::print_stdout, clippy::print_stderr, clippy::exit, reason = "a debug command: it prints its answer and exits")]
fn dump_snapshot(rebuild: bool, provider: view_prefs::Provider) -> ! {
    let pricing = PricingMap::new();
    let snapshots = refresh::load_and_build(&mut refresh::Stores::open_default(), &pricing, rebuild);
    let snapshot = match provider {
        view_prefs::Provider::Claude => Some(&snapshots.claude),
        view_prefs::Provider::Codex => snapshots.codex.as_ref(),
        view_prefs::Provider::All => snapshots.all.as_ref(),
    };
    let Some(snapshot) = snapshot else {
        eprintln!("sl-tray: no Codex home (set CODEX_HOME, or install Codex), so there is no {} snapshot", provider.key());
        std::process::exit(1);
    };
    match serde_json::to_string_pretty(snapshot) {
        Ok(json) => {
            println!("{json}");
            std::process::exit(0);
        }
        Err(error) => {
            // `Snapshot` is plain owned data (numbers, strings, `Vec`s) with
            // no map keyed on anything but a `String`, so this should be
            // unreachable in practice; still handled rather than unwrapped so
            // a future field type mistake fails loudly instead of panicking.
            eprintln!("sl-tray: failed to serialize snapshot: {error}");
            std::process::exit(1);
        }
    }
}

/// `--stats-preview <snapshot.json> [<out.png> [light|dark] [<hover column> [<width>]]]`:
/// open only the Stats window, showing a snapshot `--dump-snapshot` printed
/// earlier. With an output path, render the window to it (optionally in one
/// appearance, with a chart column hovered) and exit. For looking at the
/// window while working on it: it reads nothing but that file, starts no
/// refresh, and does not save the window's frame.
fn stats_preview_args<I: IntoIterator<Item = String>>(args: I) -> Option<Vec<String>> {
    let mut args = args.into_iter();
    args.by_ref().find(|arg| arg == "--stats-preview")?;
    let rest: Vec<String> = args.collect();
    (!rest.is_empty()).then_some(rest)
}

#[expect(clippy::print_stdout, clippy::print_stderr, clippy::exit, reason = "a debug command: it prints its answer and exits")]
fn stats_preview(mtm: MainThreadMarker, app: &NSApplication, args: &[String]) -> ! {
    let path = &args[0];
    // `fixture:<name>`: the fixture's Claude Code snapshot, for the goldens.
    let read = |path: &str| -> Result<Snapshot, String> {
        if let Some(name) = path.strip_prefix("fixture:") {
            return fixture::load(name).map(|fixture| fixture.snapshots.claude);
        }
        std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|json| serde_json::from_str(&json).map_err(|e| e.to_string()))
    };
    let snapshot: Snapshot = match read(path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("sl-tray: cannot read a snapshot from {path}: {error}");
            std::process::exit(1);
        }
    };
    if let Some(mode) = args.get(2) {
        let which = if mode == "dark" { sl_macos::consts::Appearance::Dark } else { sl_macos::consts::Appearance::Light };
        objc2_app_kit::NSAppearanceCustomization::setAppearance(app, sl_macos::consts::appearance(which).as_deref());
    }
    let width = args.get(4).and_then(|arg| arg.parse().ok());
    // Whose usage a dumped snapshot is, read off which limits it carries.
    let provider = match (snapshot.codex_limits.is_some(), snapshot.rate_limits.is_some()) {
        (true, true) => view_prefs::Provider::All,
        (true, false) => view_prefs::Provider::Codex,
        _ => view_prefs::Provider::Claude,
    };
    let window = stats::StatsWindow::preview(mtm, &snapshot, provider, width);
    window.show(&snapshot, provider);
    let Some(out) = args.get(1) else {
        app.run();
        std::process::exit(0);
    };
    let settle = || objc2_foundation::NSRunLoop::currentRunLoop().runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.8));
    settle();
    if let Some(column) = args.get(3).and_then(|arg| arg.parse().ok()) {
        window.preview_hover(column);
        settle();
    }
    let written = window.preview_png(out);
    println!("{}", if written { out.as_str() } else { "render failed" });
    std::process::exit(i32::from(!written));
}

#[expect(clippy::print_stdout, clippy::print_stderr, reason = "the debug commands print their answers")]
fn main() {
    let flags = parse_args(std::env::args().skip(1));

    if flags.dump_codex_sessions {
        let Some(home) = sl_usage::codex::codex_home() else {
            eprintln!("sl-tray: no Codex home");
            std::process::exit(1);
        };
        for session in codex_sessions::CodexSessions::default().load(&home) {
            println!(
                "{}  {:?}  pid {:?}  {}  {:?}  idle since {:?}  context {:?}",
                session.session_id,
                session.activity,
                session.pid,
                session.cwd,
                session.name,
                session.last_activity_ms,
                session.context.map(|c| c.percent.round())
            );
        }
        std::process::exit(0);
    }
    if flags.dump_snapshot {
        dump_snapshot(flags.rebuild, flags.dump_provider);
    }
    if flags.test_notification {
        println!("{}", notify::send_blocking("Statusline tray", "Test notification from the tray."));
        std::process::exit(0);
    }

    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("sl-tray: must be started on the main thread");
        std::process::exit(1);
    };

    let app = NSApplication::sharedApplication(mtm);
    // Accessory: menu bar only. No Dock icon, no menu bar app menu, no window.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // `--icon-preview <prefix>`: the menu bar icon's states, and the menu's
    // limits row, as images for looking at while working on them. Draws and
    // exits.
    if let Some(prefix) = std::env::args().skip_while(|arg| arg != "--icon-preview").nth(1) {
        let written = controller::preview_icons(&prefix);
        std::process::exit(i32::from(!written));
    }
    if let Some(args) = stats_preview_args(std::env::args().skip(1)) {
        stats_preview(mtm, &app, &args);
    }
    if std::env::args().any(|arg| arg == "--self-test") {
        self_test::self_test(mtm);
    }
    if let Some(spec) = std::env::args().skip_while(|arg| arg != "--dump-views").nth(1) {
        self_test::dump_views(mtm, &spec);
    }

    let auto_awake = awake::AutoCaffeinate::new(AtomicBool::new(awake::load()));
    // `SL_TRAY_FIXTURE`: made-up state and no background threads, for
    // looking at the tray without anything real behind it.
    let (shared, board, sticky) = match fixture::from_env() {
        Some(Ok(fixture)) => (
            latest::new(fixture.snapshots),
            crate::latest::new(sessions::Published { open: fixture.open, closed: fixture.closed.clone(), stacks: fixture.stacks }),
            Arc::new(Mutex::new(sticky::Sticky::from_json(&serde_json::to_string(&fixture.closed).unwrap_or_default()))),
        ),
        Some(Err(error)) => {
            eprintln!("sl-tray: {error}");
            std::process::exit(2);
        }
        None => {
            // Seed with an empty snapshot so the first paint has something to
            // draw before the background thread's first refresh lands.
            let shared = latest::new(Snapshots::empty(now_ms()));
            refresh::spawn(Arc::clone(&shared), flags.rebuild);
            let board = crate::latest::new(sessions::Published::default());
            // Closed sessions: remembered across launches, published apart from the open ones.
            let sticky = Arc::new(Mutex::new(sticky::Sticky::load()));
            sessions::spawn(Arc::clone(&board), Arc::clone(&auto_awake), Arc::clone(&sticky));
            (shared, board, sticky)
        }
    };

    // Gotcha #2: every target and delegate holds the controller weakly, so
    // this `Rc` is what keeps it alive. It is a plain `let` in `main`, and
    // `run()` below blocks until the app terminates, so it lives for the whole
    // process. Do not shorten this to `BarController::new(..).install()`: the
    // controller would be dropped at the end of the statement and every click
    // and `menuWillOpen:` would silently find nothing to call.
    let controller = BarController::new(mtm, shared, board, auto_awake, sticky);
    controller.install();

    // Blocks until `terminate:` (the Quit menu item). Anything after it only
    // runs on paths that unwind out of AppKit, but keeping the explicit drop
    // documents that `controller` is alive for the entire call above.
    app.run();
    drop(controller);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn no_flags_is_all_false() {
        assert_eq!(parse_args(args(&[])), Flags::default());
    }

    #[test]
    fn dump_snapshot_alone() {
        assert_eq!(parse_args(args(&["--dump-snapshot"])), Flags { dump_snapshot: true, ..Flags::default() });
    }

    #[test]
    fn dump_snapshot_takes_a_provider_right_after_it() {
        let codex = Flags { dump_snapshot: true, dump_provider: view_prefs::Provider::Codex, ..Flags::default() };
        assert_eq!(parse_args(args(&["--dump-snapshot", "codex"])), codex);
        assert_eq!(parse_args(args(&["--dump-snapshot", "all", "--rebuild"])).dump_provider, view_prefs::Provider::All);
        assert_eq!(parse_args(args(&["codex", "--dump-snapshot"])).dump_provider, view_prefs::Provider::Claude, "only right after the flag");
    }

    #[test]
    fn rebuild_alone() {
        assert_eq!(parse_args(args(&["--rebuild"])), Flags { rebuild: true, ..Flags::default() });
    }

    #[test]
    fn both_flags_together_in_either_order() {
        let want = Flags { dump_snapshot: true, rebuild: true, ..Flags::default() };
        assert_eq!(parse_args(args(&["--dump-snapshot", "--rebuild"])), want);
        assert_eq!(parse_args(args(&["--rebuild", "--dump-snapshot"])), want);
    }

    #[test]
    fn unrecognized_flags_are_ignored_not_errors() {
        assert_eq!(parse_args(args(&["--bogus", "--dump-snapshot", "extra-positional"])), Flags { dump_snapshot: true, ..Flags::default() });
    }

    #[test]
    fn the_notification_test_flag_is_recognised() {
        assert_eq!(parse_args(args(&["--test-notification"])), Flags { test_notification: true, ..Flags::default() });
    }

    #[test]
    fn the_log_takes_a_line() {
        log("a line in the test's stderr");
        trace("not shown unless SL_TRAY_DEBUG is set");
    }

    #[test]
    fn a_repeated_flag_is_harmless() {
        assert_eq!(parse_args(args(&["--rebuild", "--rebuild"])), Flags { rebuild: true, ..Flags::default() });
    }
}
