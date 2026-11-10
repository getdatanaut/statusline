//! Runs the actual `statusline-tray --dump-snapshot` binary end to end.
//!
//! Everything else in this crate's test suite exercises `build_snapshot` and
//! `parse_args` directly, in-process -- this is the one test that spawns the
//! real compiled binary and checks what a human running it would see: exit
//! code, valid JSON on stdout, no crash. It is also, per the task brief, the
//! one command in the whole plan a headless CI environment can meaningfully
//! verify: `--dump-snapshot` never touches AppKit, so this needs no display.
//!
//! `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_CACHE_HOME` are all redirected
//! to a scratch directory unique to this test, so the run never reads the real
//! `~/.claude` or `~/.codex` or writes to the real `~/.cache/statusline` -- see
//! `sl_usage`'s own `paths`/`codex`/`cache` modules for what each controls.

#![expect(
    clippy::unwrap_used,
    reason = "test fixtures, which fail the test loudly"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch `CLAUDE_CONFIG_DIR` (containing an empty `projects/`, so
/// discovery finds a valid but empty corpus rather than erroring) plus a
/// scratch `XDG_CACHE_HOME`, both unique per test and per process.
struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!(
            "sl-tray-dump-snapshot-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(self_config_dir(&base).join("projects")).unwrap();
        std::fs::create_dir_all(self_cache_dir(&base)).unwrap();
        std::fs::create_dir_all(self_codex_home(&base).join("sessions")).unwrap();
        Self { base }
    }

    fn codex_home(&self) -> PathBuf {
        self_codex_home(&self.base)
    }

    fn config_dir(&self) -> PathBuf {
        self_config_dir(&self.base)
    }

    fn cache_dir(&self) -> PathBuf {
        self_cache_dir(&self.base)
    }
}

fn self_config_dir(base: &Path) -> PathBuf {
    base.join("claude-config")
}

fn self_cache_dir(base: &Path) -> PathBuf {
    base.join("xdg-cache")
}

fn self_codex_home(base: &Path) -> PathBuf {
    base.join("codex-home")
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

fn tray_command(scratch: &Scratch) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_statusline-tray"));
    command.env("CLAUDE_CONFIG_DIR", scratch.config_dir());
    command.env("XDG_CACHE_HOME", scratch.cache_dir());
    // An empty scratch Codex home: never the real `~/.codex`.
    command.env("CODEX_HOME", scratch.codex_home());
    // Discovery falls back to `$XDG_CONFIG_HOME/claude` and `~/.claude` only
    // when `CLAUDE_CONFIG_DIR` is unset; setting it above is already
    // exclusive, but clearing this too removes any doubt this run could ever
    // resolve to a real config directory.
    command.env_remove("XDG_CONFIG_HOME");
    command
}

#[test]
fn dump_snapshot_prints_valid_json_and_exits_zero() {
    let scratch = Scratch::new("basic");
    let output = tray_command(&scratch)
        .arg("--dump-snapshot")
        .output()
        .expect("the binary should run");

    assert!(
        output.status.success(),
        "exit code: {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be one JSON document");
    assert!(
        parsed.get("todayDate").is_some(),
        "the snapshot's camelCase fields should be present: {stdout}"
    );
    assert_eq!(
        parsed
            .get("recordCount")
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "empty scratch corpus, no records"
    );
}

#[test]
fn dump_snapshot_reads_the_rate_limit_capture_from_the_cache_dir() {
    let scratch = Scratch::new("limits");
    // Exactly where `statusline::limits` writes it, relative to the scratch
    // `XDG_CACHE_HOME` -- if this path ever stops matching, the tray silently
    // shows "no capture yet" forever, which is the failure this test exists
    // to catch.
    let limits = scratch.cache_dir().join("statusline").join("limits.json");
    std::fs::create_dir_all(limits.parent().unwrap()).unwrap();
    std::fs::write(
        &limits,
        r#"{"captured_at_ms": 1788200392112,
            "rate_limits": {"five_hour": {"used_percentage": 12.5, "resets_at": 1787580599},
                            "seven_day": {"used_percentage": 15.0, "resets_at": 1787889599}}}"#,
    )
    .unwrap();

    let output = tray_command(&scratch)
        .arg("--dump-snapshot")
        .output()
        .expect("the binary should run");
    assert!(
        output.status.success(),
        "exit code: {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be one JSON document");
    let rate_limits = parsed
        .get("rateLimits")
        .expect("the capture should reach the snapshot");
    assert_eq!(
        rate_limits["capturedAtMs"].as_i64(),
        Some(1_788_200_392_112)
    );
    assert_eq!(rate_limits["fiveHour"]["usedPercent"].as_f64(), Some(12.5));
    // Epoch seconds in the capture, epoch ms in the snapshot.
    assert_eq!(
        rate_limits["fiveHour"]["resetsAtMs"].as_i64(),
        Some(1_787_580_599_000)
    );
    assert_eq!(rate_limits["sevenDay"]["usedPercent"].as_f64(), Some(15.0));
}

#[test]
fn dump_snapshot_without_a_capture_reports_no_rate_limits() {
    let scratch = Scratch::new("no-limits");
    let output = tray_command(&scratch)
        .arg("--dump-snapshot")
        .output()
        .expect("the binary should run");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be one JSON document");
    // Present as an explicit null, not absent: "no capture" is a state the
    // menu renders, not a missing field.
    assert_eq!(parsed.get("rateLimits"), Some(&serde_json::Value::Null));
}

#[test]
fn dump_snapshot_with_rebuild_also_prints_valid_json_and_exits_zero() {
    let scratch = Scratch::new("rebuild");
    let output = tray_command(&scratch)
        .arg("--dump-snapshot")
        .arg("--rebuild")
        .output()
        .expect("the binary should run");

    assert!(
        output.status.success(),
        "exit code: {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let _parsed: serde_json::Value = serde_json::from_str(&stdout)
        .expect("stdout should be one JSON document even with --rebuild");
}

#[test]
fn dump_snapshot_codex_and_all_read_the_scratch_codex_home() {
    let scratch = Scratch::new("codex");
    let thread = "01a1c25a-a06b-7471-8a01-1a5dd574e6e4";
    let day = scratch.codex_home().join("sessions/2026/11/09");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(
        day.join(format!("rollout-2026-11-09T23-08-01-{thread}.jsonl")),
        format!(
            r#"{{"timestamp":"2026-11-09T22:10:32.945Z","type":"session_meta","payload":{{"id":"{thread}","cwd":"/repo"}}}}
{{"timestamp":"2026-11-09T22:10:33.260Z","type":"turn_context","payload":{{"turn_id":"t1","model":"gpt-6.1-sol"}}}}
{{"timestamp":"2026-11-09T22:10:36.025Z","type":"token_usage_record","payload":{{"thread_id":"{thread}","turn_id":"t1","response_id":"r1","usage":{{"input_tokens":17119,"cached_input_tokens":6912,"output_tokens":11}}}}}}
{{"timestamp":"2026-11-09T22:10:36.025Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":9.0,"window_minutes":10080,"resets_at":1794418728}},"secondary":null}}}}}}
"#
        ),
    )
    .unwrap();

    let dump = |provider: &str| {
        let output = tray_command(&scratch)
            .args(["--dump-snapshot", provider])
            .output()
            .expect("the binary should run");
        assert!(
            output.status.success(),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("one JSON document")
    };
    let codex = dump("codex");
    assert_eq!(codex["recordCount"].as_u64(), Some(1));
    assert_eq!(codex["models"][0]["model"].as_str(), Some("gpt-6.1-sol"));
    assert_eq!(
        codex["codexLimits"]["sevenDay"]["usedPercent"].as_f64(),
        Some(9.0)
    );
    assert_eq!(
        codex["codexLimits"]["sevenDay"]["windowMinutes"].as_u64(),
        Some(10_080)
    );
    assert!(scratch.cache_dir().join("statusline/usage.sqlite").exists());
    assert!(
        !scratch
            .cache_dir()
            .join("statusline/usage-codex.sqlite")
            .exists(),
        "one database for both"
    );

    let all = dump("all");
    assert_eq!(
        all["recordCount"].as_u64(),
        Some(1),
        "the scratch Claude corpus is empty"
    );
    assert_eq!(dump("claude")["recordCount"].as_u64(), Some(0));
}

#[test]
fn dump_snapshot_codex_without_a_codex_home_says_so_and_fails() {
    let scratch = Scratch::new("no-codex");
    let missing = scratch.base.join("no-such-codex-home");
    let output = tray_command(&scratch)
        .env("CODEX_HOME", &missing)
        .args(["--dump-snapshot", "codex"])
        .output()
        .expect("the binary should run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no Codex home"));
}
