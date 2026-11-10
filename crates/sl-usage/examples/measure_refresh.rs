//! Measure what refreshes cost, the way the tray holds a store: one
//! `UsageTotals` across several refreshes, the process's disk counters read
//! around each. Not part of any build; run it by hand against a copy of the
//! corpus:
//!
//! ```sh
//! CLAUDE_CONFIG_DIR=<copy> cargo run --release -p sl-usage --example measure_refresh -- <store dir> <transcript to append to>
//! ```
//!
//! It appends to the transcript and adds a subagent file next to it, so it
//! refuses to run unless `CLAUDE_CONFIG_DIR` names the copy and the transcript
//! is inside it. Never point it at the real `~/.claude`.
//!
//! "wal" is how much the store's write-ahead log grew during the step: what
//! SQLite actually wrote, which the disk counters only see later, when the OS
//! writes the pages back.

#![expect(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout, reason = "a measurement tool run by hand: it fails loudly and prints its findings")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sl_usage::cache::{RefreshMode, UsageTotals};
use sl_usage::PricingMap;

fn disk() -> (u64, u64) {
    let io = sl_macos::process::disk_io();
    (io.read, io.written)
}

fn wal_size(store_dir: &Path) -> u64 {
    std::fs::metadata(store_dir.join(format!("{}-wal", sl_usage::cache::DB_FILE))).map_or(0, |meta| meta.len())
}

fn step(name: &str, store_dir: &Path, totals: &mut UsageTotals, pricing: &PricingMap) {
    let (r0, w0) = disk();
    let wal0 = wal_size(store_dir);
    let started = Instant::now();
    // As the tray refreshes: discovering the files itself, with the walk it
    // keeps between refreshes.
    totals.refresh(pricing, RefreshMode::Incremental);
    let elapsed = started.elapsed();
    let (r1, w1) = disk();
    let wal1 = wal_size(store_dir);
    println!(
        "{name:<28} {:>9.1} ms   disk read {:>10}   disk written {:>10}   wal {:>8}   records {}   {:?}",
        elapsed.as_secs_f64() * 1000.0,
        r1 - r0,
        w1 - w0,
        wal1.saturating_sub(wal0),
        totals.record_count(),
        totals.last_refresh(),
    );
}

/// The last usage line of `path`, under new message and request ids.
fn new_usage_line(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    let last = text.lines().rev().find(|line| line.contains(r#""usage":{"#) && line.contains(r#""id":"msg_"#)).expect("a usage line");
    let line = last.replacen(r#""id":"msg_"#, &format!(r#""id":"msg_measure{n}_"#), 1);
    line.replacen(r#""requestId":"req_"#, &format!(r#""requestId":"req_measure{n}_"#), 1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (store_dir, transcript) = (PathBuf::from(&args[1]), PathBuf::from(&args[2]));
    let corpus = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).expect("set CLAUDE_CONFIG_DIR to a copy of the corpus");
    let (corpus, inside) = (corpus.canonicalize().expect("CLAUDE_CONFIG_DIR exists"), transcript.canonicalize().expect("the transcript exists"));
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    assert!(inside.starts_with(&corpus), "{} is not inside CLAUDE_CONFIG_DIR {}", inside.display(), corpus.display());
    assert!(!corpus.starts_with(home.join(".claude")) && !corpus.starts_with(home.join(".config/claude")), "CLAUDE_CONFIG_DIR must be a copy, not the real corpus");
    let pricing = PricingMap::new();
    let mut totals = UsageTotals::empty(&store_dir);

    step("open + first refresh", &store_dir, &mut totals, &pricing);
    step("quiet", &store_dir, &mut totals, &pricing);

    let lines: String = (0..5).map(|n| new_usage_line(&transcript, n) + "\n").collect();
    std::fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(lines.as_bytes()).unwrap();
    step("append 5 records", &store_dir, &mut totals, &pricing);

    let session = transcript.file_stem().unwrap();
    let dir = transcript.parent().unwrap().join(session).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let replay: String = (5..8).map(|n| new_usage_line(&transcript, n) + "\n").collect();
    std::fs::write(dir.join(format!("agent-measure-{}.jsonl", std::process::id())), replay).unwrap();
    step("new subagent file", &store_dir, &mut totals, &pricing);

    step("quiet", &store_dir, &mut totals, &pricing);
}
