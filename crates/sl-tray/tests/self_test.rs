//! Runs `statusline-tray --self-test` and `--dump-views`: the real tray,
//! built from a made-up fixture, every control clicked (see the binary's
//! `self_test` module). Spawned as a process, as `dump_snapshot.rs` does, so
//! AppKit gets the main thread it needs.
//!
//! Everything the tray could touch is redirected: a scratch home, cache,
//! Claude config and Codex home, a defaults suite of its own instead of the
//! tray's real settings, the clock pinned, and a `PATH` whose `gh`, `git`,
//! `open`, `zed`... are stubs that fail loudly. Effects are recorded by the
//! binary, never performed, so the stubs are only a second line of defence.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures, which fail the test loudly"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    base: PathBuf,
    suite: String,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!(
            "sl-tray-self-test-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        for dir in [
            "home/.claude",
            "codex",
            "cache",
            "claude-config/projects",
            "stubs",
        ] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        for tool in [
            "gh",
            "claude",
            "git",
            "lsof",
            "open",
            "pbcopy",
            "osascript",
            "zed",
            "linear",
            "caffeinate",
        ] {
            let stub = base.join("stubs").join(tool);
            std::fs::write(
                &stub,
                format!("#!/bin/sh\necho \"STUB {tool} called: $*\" >&2\nexit 97\n"),
            )
            .unwrap();
            make_executable(&stub);
        }
        // One suite per test, the same every run: emptied as the run starts,
        // so reusing it is harmless, and macOS leaves each suite's (empty)
        // file behind whatever is done -- one per test, not one per run.
        Self {
            base,
            suite: format!("com.statusline.selftest.{name}"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_statusline-tray"));
        command.env_clear();
        // Under `cargo llvm-cov`, where the binary writes its coverage.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
            .args(args)
            .env("HOME", self.base.join("home"))
            .env("CODEX_HOME", self.base.join("codex"))
            .env("XDG_CACHE_HOME", self.base.join("cache"))
            .env("CLAUDE_CONFIG_DIR", self.base.join("claude-config"))
            .env("SL_TRAY_DEFAULTS_SUITE", &self.suite)
            .env("SL_TRAY_NOW_MS", "1788200000000")
            .env("SL_TRAY_NO_NOTIFY", "1")
            .env("TZ", "UTC")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.base.join("stubs").display()),
            )
            .output()
            .expect("the binary should run")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// What a click must do, by the control's path and outcome: each row is a
/// line the transcript must contain. Written by outcome, not by selector, so
/// it holds however the controls are wired.
const EXPECTED: &[&str] = &[
    "check menuWillOpen: repaints at once: ok",
    "check the timer fires while the menu is tracked: ok",
    "check after menuDidClose:, the timer slows down: ok",
    "check after menuDidClose:, the closed menu is not repainted: ok",
    "check the next menuWillOpen: repaints: ok",
    "check the menu bar icon opens the popover: ok",
    "check a click as the popover closes does not reopen it: ok",
    "check a later click reopens it: ok",
    "check searching closed sessions narrows the list: ok",
    "check the Stats toolbar has its items: ok",
    "check resizing the Stats window reflows it: ok",
    "check the Stats window survives an appearance switch: ok",
    // Regular menu.
    "regular / page 2 => set meterProvider=codex",
    "regular / DN-479 / M4 / Copy full path => copy \"/x/DN-479 / M4\"",
    "regular / DN-479 / M4 / Open worktree in Zed => zed /x/DN-479 / M4",
    "regular / DN-479 / M4 / Open in Linear => open https://linear.app/x/issue/DN-479",
    "regular / DN-540 / M5  #781 / test (ubuntu-latest) / Re-run this job… => confirm \"Re-run \\\"test (ubuntu-latest)\\\"?\"; rerun \"test (ubuntu-latest)\" in /x/DN-540 / M5 (https://github.com/o/r/actions/runs/1/job/2)",
    "regular / DN-540 / M5  #781 / test (ubuntu-latest) / Show failed-step logs… => logs \"test (ubuntu-latest)\" in /x/DN-540 / M5 (https://github.com/o/r/actions/runs/1/job/2)",
    "regular / DN-540 / M5  #781 / test (ubuntu-latest) / Copy job link => copy \"https://github.com/o/r/actions/runs/1/job/2\"",
    "regular / DN-540 / M5  #781 / Open PR in browser => open https://github.com/o/r/pull/781",
    "regular / DN-540 / M5  #781 / Copy review feedback => copy-review #781 in /x/DN-540 / M5",
    "regular / DN-540 / M5  #781 / Open review feedback in Zed => review-in-zed #781 in /x/DN-540 / M5",
    "regular / remote-on / Copy remote-control session ID => copy \"rc-1234\"",
    "regular / Cold => view changed",
    "regular / Closed => view changed",
    "regular / DN-512 / M2 drafts / Copy resume command => copy \"claude --resume c1\"",
    "regular / codex refactor / Copy resume command => copy \"codex resume x9\"",
    "regular / Stack #746 · 2/5 merged / Open stack on GitHub => open https://github.com/o/r/pull/753",
    "regular / All closed sessions… => opened \"Closed Sessions\"",
    "regular / Settings => opened \"Statusline Tray Settings\"",
    "regular / Stats => opened \"Claude Code Usage\"",
    "regular / Quit => quit",
    // Compact popover.
    "compact / [Needs you] => view changed",
    "compact / card DN-540 / M5 / PR => open https://github.com/o/r/pull/781; popover closed",
    "compact / card DN-540 / M5 / Re-run… => confirm \"Re-run \\\"test (ubuntu-latest)\\\"?\"; rerun \"test (ubuntu-latest)\" in /x/DN-540 / M5 (https://github.com/o/r/actions/runs/1/job/2); popover closed",
    "compact / card Stack #746  2/5 merged / Open stack on GitHub => open https://github.com/o/r/pull/753; popover closed",
    "compact / card DN-512 / M2 drafts => view changed",
    "compact / card DN-512 / M2 drafts / Copy resume command => copy \"claude --resume c1\"; popover closed",
    "compact / card codex refactor / Zed => zed /x/x9; popover closed",
    "compact / Quit => quit",
    // Settings: every tab, every kind of control.
    "settings / [Compact] => set layout=compact",
    "settings / [Rings] => set meterStyle=rings",
    "settings / Show spend under the meters => set showSpend=0",
    "settings / Show the Linear ticket a branch is named for => set showLinear=0",
    "settings / Show what the sessions are doing => set barBadge=0",
    "settings / Keep the Mac awake for sessions with /rc on => set rcKeepsAwake=0",
    "settings / [45 s] => set refreshUsageSecs=45",
    "settings / [5 min] => set refreshPrSecs=300",
    "settings / [Hide] => set coldMode=hide",
    "settings / [Tight] => set density=tight",
    "settings / [80 / 90 %] => set ringNotePct=80",
    "settings / A CI job fails on a session's PR => set notifyCiFailures=0",
    "settings / A PR in a stack needs a rebase => set notifyStackRebase=1",
    // Closed Sessions: each row's buttons are about that row's session.
    "closed / Copy Resume => copy \"claude --resume c1\"",
    "closed / Zed #2 => zed /x/c2",
    "closed / Copy Resume #3 => copy \"codex resume x9\"",
    "closed / Dismiss => view changed",
    // Stats.
    "stats / [7 Days] => set StatsRangeDays=7",
    "stats / [Most Expensive] => set StatsSessionTab=1",
];

#[test]
fn every_control_does_what_it_should() {
    let sandbox = Sandbox::new("controls");
    let output = sandbox.run(&["--self-test"]);
    let transcript = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "self-test failed:\n{}\n{transcript}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing: Vec<&&str> = EXPECTED
        .iter()
        .filter(|line| !transcript.lines().any(|got| got == **line))
        .collect();
    assert!(
        missing.is_empty(),
        "missing from the transcript: {missing:#?}\n--- transcript ---\n{transcript}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("STUB"),
        "a real tool was run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_view_dump_shows_both_layouts_and_comes_out_the_same_twice() {
    let sandbox = Sandbox::new("dump");
    let first = sandbox.run(&["--dump-views", "full"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let text = String::from_utf8_lossy(&first.stdout).into_owned();
    for section in ["== regular", "== regular, folds open", "== compact"] {
        assert!(
            text.lines().any(|line| line == section),
            "no {section:?} section"
        );
    }
    assert!(
        text.contains("action (zed) \"Open worktree in Zed\" ⟶"),
        "a submenu action"
    );
    assert!(
        text.contains("text \"Updated 29s ago · 433,498 records\""),
        "the footer, from the fixed clock"
    );
    // A PR's pills, worded for where they are: the submenu header's count,
    // the compact card's count of what.
    assert!(
        text.contains("[‹Green:Ready for review›] … [‹Red:CI ✗ 1›]"),
        "the header's pills"
    );
    assert!(text.contains("text \"CI ✗ 1 failed\""), "a card's CI pill");
    let second = sandbox.run(&["--dump-views", "full"]);
    assert_eq!(
        text,
        String::from_utf8_lossy(&second.stdout),
        "the dump must be deterministic"
    );
}

#[test]
fn the_harness_refuses_to_run_against_the_real_settings() {
    let sandbox = Sandbox::new("refuse");
    let mut command = Command::new(env!("CARGO_BIN_EXE_statusline-tray"));
    command.env_clear();
    // Under `cargo llvm-cov`: its profile goes where the others' do, not into the crate.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let output = command
        .arg("--self-test")
        .env("HOME", sandbox.base.join("home"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("the binary should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("SL_TRAY_DEFAULTS_SUITE"));
}

/// The previews draw every custom view -- the menu's rows (plain and
/// highlighted), the meters, the popover's cards, the Stats window in both
/// appearances -- into PNGs and TIFFs. Their pixels are compared by hand (or against
/// saved goldens); here they must come out, so the drawing code runs under
/// `cargo test`.
#[test]
fn the_previews_draw_every_view() {
    let sandbox = Sandbox::new("previews");
    let out = sandbox.base.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let icons = sandbox.run(&["--icon-preview", out.join("icon").to_str().unwrap()]);
    assert!(
        icons.status.success(),
        "{}",
        String::from_utf8_lossy(&icons.stderr)
    );
    for mode in ["light", "dark"] {
        let path = out.join(format!("stats-{mode}.png"));
        let stats = sandbox.run(&[
            "--stats-preview",
            "fixture:full",
            path.to_str().unwrap(),
            mode,
        ]);
        assert!(
            stats.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&stats.stderr)
        );
    }
    let pngs: Vec<(String, u64)> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.metadata().unwrap().len(),
            )
        })
        .collect();
    assert!(pngs.len() >= 20, "every preview written: {pngs:?}");
    for (name, size) in &pngs {
        let bytes = std::fs::read(out.join(name)).unwrap();
        let image = bytes.starts_with(b"\x89PNG")
            || bytes.starts_with(b"MM\0*")
            || bytes.starts_with(b"II*\0");
        assert!(
            image && *size > 200,
            "{name}: a PNG or TIFF with something in it ({size} bytes)"
        );
    }
    assert!(
        !String::from_utf8_lossy(&icons.stderr).contains("STUB"),
        "a real tool was run"
    );
}

/// The rate-limit capture is read as it changes, not only at the usage
/// refresh: one newer than the snapshot's shows at once.
#[test]
fn a_capture_newer_than_the_snapshot_shows_at_once() {
    let sandbox = Sandbox::new("capture");
    let dir = sandbox.base.join("cache/statusline");
    std::fs::create_dir_all(&dir).unwrap();
    // A minute before the fixed clock; the fixture's own is two minutes old.
    std::fs::write(
        dir.join("limits.json"),
        r#"{"captured_at_ms": 1788199940000, "rate_limits": {
             "five_hour": {"used_percentage": 64, "resets_at": 1788208400},
             "seven_day": {"used_percentage": 55, "resets_at": 1788459200}}}"#,
    )
    .unwrap();
    let output = sandbox.run(&["--dump-views", "full"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let meters: Vec<&str> = text
        .lines()
        .filter(|line| line.trim_start().starts_with("meter Bars"))
        .take(2)
        .collect();
    assert!(
        meters
            .first()
            .is_some_and(|line| line.contains("meter Bars 64%")),
        "the file's session window: {meters:?}"
    );
    assert!(
        meters
            .get(1)
            .is_some_and(|line| line.contains("meter Bars 55%")),
        "and its weekly one: {meters:?}"
    );
}
