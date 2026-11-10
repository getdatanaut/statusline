//! End-to-end tests for the usage store.
//!
//! Every test drives a [`UsageTotals`] against a throwaway corpus and a
//! throwaway store directory under the temp dir, so nothing here touches the
//! real `~/.claude` tree, the real store, or any process-wide environment
//! variable.
//!
//! The oracle is [`UsageCache::from_files`]: every file parsed from scratch and
//! deduped in one pass. After every kind of change, the store must report what
//! the oracle reports for the same files. The one place they are meant to
//! differ is a transcript deleted from a live session: the store keeps the
//! records it already read, the oracle has no file to read them from.
//!
//! What a refresh *read* is asserted directly from
//! [`UsageTotals::last_refresh`]. Some tests also prove a file was not re-read
//! with [`poison_preserving_metadata`]: rewrite its bytes with different content
//! of the same length and restore its mtime, so `stat` cannot tell -- a store
//! that re-read it would surface the poisoned model.

#![expect(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "test helpers, which fail the test loudly")]

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use sl_usage::cache::{RefreshMode, UsageCache, UsageTotals};
use sl_usage::{PricingMap, UsageRecord, identify_blocks, session_summaries, store_path, usage_files};

// ---------------------------------------------------------------- fixtures

fn pricing() -> &'static PricingMap {
    static MAP: OnceLock<PricingMap> = OnceLock::new();
    MAP.get_or_init(PricingMap::new)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 2024-01-15T10:40:00Z, ten minutes after the fixtures' first record.
const NOW: i64 = 1_705_315_200_000;
const HOUR: i64 = 3_600_000;

/// A fresh `<tmp>/projects/` corpus root plus a fresh store directory.
struct Fixture {
    root: PathBuf,
    store_dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("sl-usage-store-{}-{name}-{n}", std::process::id()));
        fs::remove_dir_all(&base).ok();
        let root = base.join("corpus");
        let store_dir = base.join("store");
        fs::create_dir_all(root.join("projects")).unwrap();
        fs::create_dir_all(&store_dir).unwrap();
        Self { root, store_dir }
    }

    /// Path of a session's main transcript, `projects/<project>/<session>.jsonl`.
    fn transcript(&self, project: &str, session: &str) -> PathBuf {
        let dir = self.root.join("projects").join(project);
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{session}.jsonl"))
    }

    /// Path of a subagent replay, `projects/<project>/<session>/subagents/<name>.jsonl`.
    fn replay(&self, project: &str, session: &str, name: &str) -> PathBuf {
        let dir = self.root.join("projects").join(project).join(session).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.jsonl"))
    }

    /// Discovery over the fixture corpus, exactly as production feeds it.
    fn files(&self) -> Vec<PathBuf> {
        usage_files(std::slice::from_ref(&self.root))
    }

    fn oracle(&self) -> UsageCache {
        UsageCache::from_files(&self.files(), pricing())
    }

    /// A store over this fixture, as the tray holds one: kept across refreshes.
    fn store(&self) -> Store<'_> {
        Store { fixture: self, totals: UsageTotals::empty(&self.store_dir), now_ms: NOW }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            fs::remove_dir_all(base).ok();
        }
    }
}

struct Store<'a> {
    fixture: &'a Fixture,
    totals: UsageTotals,
    now_ms: i64,
}

impl Store<'_> {
    fn refresh(&mut self) -> sl_usage::RefreshStats {
        self.totals.refresh_files(&self.fixture.files(), pricing(), RefreshMode::Incremental);
        let stats = self.totals.last_refresh().clone();
        assert_eq!(stats.error, None, "a refresh must not fail");
        stats
    }

    fn rebuild(&mut self) -> sl_usage::RefreshStats {
        self.totals.refresh_files(&self.fixture.files(), pricing(), RefreshMode::Rebuild);
        self.totals.last_refresh().clone()
    }

    fn offset(&self, path: &Path) -> u64 {
        self.totals.parsed_offset(path).unwrap_or_else(|| panic!("{} is not stored", path.display()))
    }

    /// The models billed on 2024-01-15, which the poisoning flips between.
    fn models_on_the_15th(&self) -> Vec<String> {
        let mut models: Vec<String> = self.totals.daily_model_aggregates().get("2024-01-15").map(|m| m.keys().cloned().collect()).unwrap_or_default();
        models.sort();
        models
    }

    /// Everything the store reports, against the oracle's account of the same
    /// files. Sums are compared to a tolerance: they add up in another order.
    fn assert_matches_oracle(&self, step: &str) {
        let oracle = self.fixture.oracle();
        let t = &self.totals;
        assert_eq!(t.record_count(), oracle.record_count(), "{step}: record count");
        assert_eq!(t.group_count(), oracle.groups().len(), "{step}: groups");
        assert!((t.total_cost() - oracle.total_cost()).abs() < 1e-9, "{step}: total cost {} vs {}", t.total_cost(), oracle.total_cost());

        // Day x model.
        let (got, want) = (t.daily_model_aggregates(), oracle.daily_model_aggregates());
        assert_eq!(got.len(), want.len(), "{step}: days");
        for (date, by_model) in &want {
            assert_eq!(got.get(date).map(std::collections::HashMap::len), Some(by_model.len()), "{step}: {date} models");
            for (model, aggregate) in by_model {
                let held = got.get(date).and_then(|m| m.get(model)).unwrap_or_else(|| panic!("{step}: {date} {model} missing"));
                assert_eq!(
                    (held.input_tokens, held.output_tokens, held.cache_creation_5m_tokens, held.cache_creation_1h_tokens, held.cache_read_tokens, held.messages),
                    (aggregate.input_tokens, aggregate.output_tokens, aggregate.cache_creation_5m_tokens, aggregate.cache_creation_1h_tokens, aggregate.cache_read_tokens, aggregate.messages),
                    "{step}: {date} {model} tokens"
                );
                assert!((held.cost - aggregate.cost).abs() < 1e-9, "{step}: {date} {model} cost");
            }
        }

        // Per model and per token kind, folded straight from the records.
        let mut models: std::collections::BTreeMap<String, (f64, u64, u64, bool)> = std::collections::BTreeMap::default();
        let (mut input, mut output, mut write, mut read) = (0u64, 0u64, 0u64, 0u64);
        for r in oracle.all_records() {
            let e = models.entry(r.model.clone().unwrap_or_else(|| "unknown".into())).or_default();
            e.0 += r.cost;
            e.1 += r.total_tokens();
            e.2 += r.output_tokens;
            e.3 |= r.missing_pricing;
            input += r.input_tokens;
            output += r.output_tokens;
            write += r.cache_creation_tokens();
            read += r.cache_read_tokens;
        }
        let got = t.model_totals();
        assert_eq!(got.len(), models.len(), "{step}: models");
        for (model, (cost, tokens, out, unpriced)) in &models {
            let m = got.get(model).unwrap_or_else(|| panic!("{step}: model {model} missing"));
            assert!((m.cost - cost).abs() < 1e-9 && m.tokens == *tokens && m.output_tokens == *out && m.unpriced == *unpriced, "{step}: model {model}");
        }
        let tokens = t.token_totals();
        assert_eq!((tokens.input, tokens.output, tokens.cache_write, tokens.cache_read), (input, output, write, read), "{step}: token totals");

        // Sessions.
        let mut want = session_summaries(&oracle);
        let mut got = t.session_summaries();
        want.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        got.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        assert_eq!(got.len(), want.len(), "{step}: sessions");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(
                (&g.session_id, &g.project_path, g.total_tokens, g.input_tokens, g.output_tokens, g.message_count, g.first_seen, g.last_activity, &g.models),
                (&w.session_id, &w.project_path, w.total_tokens, w.input_tokens, w.output_tokens, w.message_count, w.first_seen, w.last_activity, &w.models),
                "{step}: session {}",
                w.session_id
            );
            assert!((g.cost - w.cost).abs() < 1e-9, "{step}: session {} cost", w.session_id);
        }

        // The active block, at a few instants: inside it, and long after.
        let all: Vec<&UsageRecord> = oracle.all_records().collect();
        for now in [self.now_ms, self.now_ms + HOUR, self.now_ms + 4 * HOUR, self.now_ms + 9 * HOUR] {
            let want = identify_blocks(&all, now).into_iter().rev().find(|b| b.is_active && !b.is_gap);
            let got = t.active_block(now);
            assert_eq!(got.is_some(), want.is_some(), "{step}: block active at +{}h", (now - self.now_ms) / HOUR);
            if let (Some(g), Some(w)) = (got, want) {
                assert_eq!(
                    (g.start_time_ms, g.end_time_ms, Some(g.first_entry_ms), w.actual_end_time_ms.map(|_| g.last_entry_ms)),
                    (w.start_time_ms, w.end_time_ms, w.first_entry_time_ms, w.actual_end_time_ms),
                    "{step}: block bounds"
                );
                assert!((g.cost - w.cost).abs() < 1e-9, "{step}: block cost");
                assert_eq!(g.tokens, w.input_tokens + w.output_tokens + w.cache_creation_5m_tokens + w.cache_creation_1h_tokens + w.cache_read_tokens, "{step}: block tokens");
                let want_rate = sl_usage::block_burn_rate(&w).map(|r| r.cost_per_hour);
                assert_eq!(g.cost_per_hour().is_some(), want_rate.is_some(), "{step}: burn rate defined");
                if let (Some(a), Some(b)) = (g.cost_per_hour(), want_rate) {
                    assert!((a - b).abs() < 1e-6, "{step}: burn rate");
                }
            }
        }
    }
}

/// One assistant usage line. `input`/`output` are written verbatim, so callers
/// that want length-preserving edits should keep their digit counts equal.
fn line(session: &str, message_id: &str, request_id: &str, timestamp: &str, input: u64, output: u64) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","sessionId":"{session}","requestId":"{request_id}","message":{{"id":"{message_id}","model":"claude-sonnet-4","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
    )
}

/// The same line as a sidechain replay of `message_id` (new request id, same
/// timestamp -- which is what makes `dedupe`'s replay fallback fire).
fn sidechain_line(session: &str, message_id: &str, request_id: &str, timestamp: &str, input: u64, output: u64) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","sessionId":"{session}","requestId":"{request_id}","isSidechain":true,"message":{{"id":"{message_id}","model":"claude-sonnet-4","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
    )
}

/// A usage line with an explicit `speed`, the way Claude Code writes an
/// assistant message's *final* record (its provisional one has no `speed`).
fn final_line(session: &str, message_id: &str, request_id: &str, timestamp: &str, input: u64, output: u64) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","sessionId":"{session}","requestId":"{request_id}","message":{{"id":"{message_id}","model":"claude-sonnet-4","usage":{{"input_tokens":{input},"output_tokens":{output},"speed":"standard"}}}}}}"#
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = String::new();
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
}

fn len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

/// Rewrite a transcript with same-length, different-meaning content and put
/// its original mtime back, so `stat` cannot tell it changed. Flips the model
/// between `claude-sonnet-4` and `claude-sonnet-3` and returns the one now on
/// disk.
fn poison_preserving_metadata(path: &Path) -> &'static str {
    const REAL: &str = r#""model":"claude-sonnet-4""#;
    const POISON: &str = r#""model":"claude-sonnet-3""#;
    let original = fs::read_to_string(path).unwrap();
    let mtime = fs::metadata(path).unwrap().modified().unwrap();
    let (poisoned, now) = if original.contains(REAL) {
        (original.replace(REAL, POISON), "claude-sonnet-3")
    } else {
        (original.replace(POISON, REAL), "claude-sonnet-4")
    };
    assert_eq!(poisoned.len(), original.len(), "poisoned content must be the same byte length");
    assert_ne!(poisoned, original, "poisoning must actually change the content");
    fs::write(path, &poisoned).unwrap();
    File::options().write(true).open(path).unwrap().set_times(FileTimes::new().set_modified(mtime)).unwrap();
    now
}

/// Cost of a single JSONL line, computed by the same parser the store uses.
fn cost_of(line: &str) -> f64 {
    let records = sl_usage::parse_bytes(format!("{line}\n").as_bytes(), "sess", "proj", pricing());
    assert_eq!(records.len(), 1, "fixture line should parse to exactly one record");
    records[0].cost
}

fn sonnet_4() -> Vec<String> {
    vec!["claude-sonnet-4".to_owned()]
}

// ------------------------------------------------------------- reading less

#[test]
fn a_quiet_refresh_reads_nothing_and_writes_nothing() {
    let fixture = Fixture::new("quiet");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(
        &path,
        &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)],
    );

    let mut store = fixture.store();
    let first = store.refresh();
    assert!(first.rebuilt, "the first refresh starts from nothing");
    assert_eq!((first.files_read, first.bytes_read, first.rows_written), (1, len(&path), 2));
    store.assert_matches_oracle("first load");

    poison_preserving_metadata(&path);
    let db_files: Vec<PathBuf> = ["", "-wal", "-shm"].iter().map(|s| PathBuf::from(format!("{}{s}", store_path(&fixture.store_dir).display()))).collect();
    let before: Vec<Option<(u64, SystemTime)>> = db_files.iter().map(|p| fs::metadata(p).ok().map(|m| (m.len(), m.modified().unwrap()))).collect();

    let second = store.refresh();
    assert_eq!((second.files_read, second.bytes_read, second.transactions), (0, 0, 0), "nothing changed, so nothing is read or written");
    let after: Vec<Option<(u64, SystemTime)>> = db_files.iter().map(|p| fs::metadata(p).ok().map(|m| (m.len(), m.modified().unwrap()))).collect();
    assert_eq!(before, after, "the database files are untouched");
    assert_eq!(store.models_on_the_15th(), sonnet_4(), "seeing the poisoned model would mean the file was re-read");
    assert_eq!(store.offset(&path), len(&path));
}

#[test]
fn appending_a_record_reads_exactly_the_appended_bytes() {
    let fixture = Fixture::new("append");
    let path_a = fixture.transcript("proj", "sess-a");
    let path_b = fixture.transcript("proj", "sess-b");
    write_lines(&path_a, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&path_b, &[line("sess-b", "msg-9", "req-9", "2024-01-15T10:30:00.000Z", 12, 22)]);

    let mut store = fixture.store();
    store.refresh();
    let cost_before = store.totals.total_cost();
    poison_preserving_metadata(&path_b);

    let appended = line("sess-a", "msg-2", "req-2", "2024-01-15T10:32:00.000Z", 30, 40);
    let size_before = len(&path_a);
    append(&path_a, format!("{appended}\n").as_bytes());
    let stats = store.refresh();

    assert_eq!((stats.files_read, stats.bytes_read, stats.rows_written, stats.rows_removed), (1, len(&path_a) - size_before, 1, 0));
    assert_eq!(stats.transactions, 1);
    assert_eq!(store.totals.record_count(), 3);
    assert!((store.totals.total_cost() - (cost_before + cost_of(&appended))).abs() < 1e-12);
    assert_eq!(store.offset(&path_a), len(&path_a), "the offset advances to the new end");
    assert_eq!(store.models_on_the_15th(), sonnet_4(), "the untouched group was not re-read");
}

#[test]
fn a_new_file_in_an_existing_group_is_read_by_itself() {
    // The old cache re-parsed the whole session group here; a busy session
    // gains subagent files all the time.
    let fixture = Fixture::new("new-file");
    let main = fixture.transcript("proj", "sess-a");
    write_lines(&main, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut store = fixture.store();
    store.refresh();

    poison_preserving_metadata(&main);
    let replay = fixture.replay("proj", "sess-a", "replay");
    write_lines(&replay, &[sidechain_line("sess-a", "msg-1", "req-2", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let stats = store.refresh();

    assert_eq!((stats.files_read, stats.bytes_read), (1, len(&replay)), "only the new file is read");
    assert_eq!(store.totals.record_count(), 1, "the replay collapses into its parent");
    assert_eq!(store.models_on_the_15th(), sonnet_4(), "the main transcript was not re-read");
    assert_eq!(store.totals.group_count(), 1);
}

#[test]
fn touching_a_file_without_changing_its_bytes_reads_nothing() {
    let fixture = Fixture::new("touch");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut store = fixture.store();
    store.refresh();

    let later = SystemTime::now() + std::time::Duration::from_secs(60);
    File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(later)).unwrap();
    let stats = store.refresh();
    assert_eq!((stats.files_read, stats.bytes_read), (0, 0), "no new bytes, nothing to read");
    assert_eq!(stats.transactions, 1, "the new mtime is recorded");
    assert_eq!(store.refresh().transactions, 0, "and from then on the file is clean");
    store.assert_matches_oracle("touched");
}

#[test]
fn a_second_process_sharing_the_store_sees_what_the_first_committed() {
    let fixture = Fixture::new("two-processes");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut first = fixture.store();
    let mut second = fixture.store();
    first.refresh();
    let stats = second.refresh();
    assert_eq!(stats.bytes_read, 0, "the second process finds it already read");
    second.assert_matches_oracle("second process, first load");

    append(&path, format!("{}\n", line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)).as_bytes());
    first.refresh();
    let stats = second.refresh();
    assert_eq!(stats.bytes_read, 0, "nor does it re-read what the first one appended");
    second.assert_matches_oracle("second process, after the first appended");
}

// --------------------------------------------------------- file lifecycle

#[test]
fn a_complete_line_counts_before_its_newline_and_is_re_read_once_it_has_one() {
    let fixture = Fixture::new("partial");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut store = fixture.store();
    store.refresh();
    let complete = len(&path);

    // A whole record, but no newline yet: it counts, as it would in a fresh
    // parse, and its bytes are not consumed.
    append(&path, line("sess-a", "msg-2", "req-2", "2024-01-15T10:32:00.000Z", 30, 40).as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);
    assert_eq!(store.offset(&path), complete, "the offset stays on the last line boundary");
    store.assert_matches_oracle("unterminated");

    append(&path, b"\n");
    let stats = store.refresh();
    assert_eq!((stats.rows_removed, stats.rows_written), (1, 1), "the line is read again and its row replaced");
    assert_eq!(store.totals.record_count(), 2, "not counted twice");
    assert_eq!(store.offset(&path), len(&path));
    store.assert_matches_oracle("terminated");

    // Half a JSON object is not a record until the rest arrives.
    let pending = line("sess-a", "msg-3", "req-3", "2024-01-15T10:33:00.000Z", 12, 22);
    let (head, rest) = pending.split_at(pending.len() / 2);
    append(&path, head.as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);
    append(&path, format!("{rest}\n").as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 3);
    store.assert_matches_oracle("completed");
}

#[test]
fn a_file_that_never_ends_in_a_newline_leaves_its_last_line_unconsumed() {
    let fixture = Fixture::new("no-trailing-newline");
    let path = fixture.transcript("proj", "sess-a");
    let first_line = line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20);
    let second_line = line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21);
    fs::write(&path, format!("{first_line}\n{second_line}")).unwrap();

    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);
    assert_eq!(store.offset(&path), (first_line.len() + 1) as u64);

    append(&path, b"\n");
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);
    assert_eq!(store.offset(&path), len(&path));
    store.assert_matches_oracle("terminated");
}

#[test]
fn truncating_a_file_drops_its_rows_and_re_reads_it() {
    let fixture = Fixture::new("truncate");
    let path = fixture.transcript("proj", "sess-a");
    let kept = line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20);
    write_lines(
        &path,
        &[kept.clone(), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21), line("sess-a", "msg-3", "req-3", "2024-01-15T10:32:00.000Z", 12, 22)],
    );
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 3);

    write_lines(&path, std::slice::from_ref(&kept));
    let stats = store.refresh();
    assert_eq!((stats.rows_removed, stats.rows_written), (3, 1));
    assert_eq!(store.totals.record_count(), 1, "no stale records survive a truncation");
    assert!((store.totals.total_cost() - cost_of(&kept)).abs() < 1e-12);
    assert_eq!(store.offset(&path), len(&path));
    store.assert_matches_oracle("truncated");
}

#[test]
fn shrinking_a_file_without_falling_below_parsed_offset_still_counts_as_a_replacement() {
    let fixture = Fixture::new("shrink");
    let path = fixture.transcript("proj", "sess-a");
    let one = line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20);
    let two = line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21);
    let three = format!(
        r#"{{"timestamp":"2024-01-15T10:32:00.000Z","sessionId":"sess-a","requestId":"req-3","padding":"{}","message":{{"id":"msg-3","model":"claude-sonnet-4","usage":{{"input_tokens":12,"output_tokens":22}}}}}}"#,
        "p".repeat(4096)
    );
    fs::write(&path, format!("{one}\n{two}\n{three}")).unwrap();
    let original_size = len(&path);
    let mut store = fixture.store();
    store.refresh();
    let offset = store.offset(&path);
    assert_eq!(offset, (one.len() + two.len() + 2) as u64);

    let replacement = line("sess-a", "msg-4", "req-4", "2024-01-15T10:33:00.000Z", 13, 23);
    fs::write(&path, format!("{one}\n{two}\n{replacement}\n")).unwrap();
    assert!(len(&path) > offset && len(&path) < original_size);
    store.refresh();
    store.assert_matches_oracle("shrunk");
    assert_eq!(store.totals.record_count(), 3, "msg-1, msg-2 and msg-4; msg-3 is gone");
}

#[test]
fn replacing_a_file_with_a_different_inode_re_reads_it() {
    let fixture = Fixture::new("inode");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut store = fixture.store();
    store.refresh();

    let staged = path.with_extension("staged");
    write_lines(&staged, &[line("sess-a", "msg-7", "req-7", "2024-01-15T11:30:00.000Z", 10, 20), line("sess-a", "msg-8", "req-8", "2024-01-15T11:31:00.000Z", 10, 20)]);
    fs::rename(&staged, &path).unwrap();
    let stats = store.refresh();
    assert_eq!(stats.rows_removed, 1);
    assert_eq!(store.totals.record_count(), 2);
    store.assert_matches_oracle("replaced");
}

#[test]
fn deleting_one_file_of_a_live_group_keeps_the_records_it_contributed() {
    let fixture = Fixture::new("delete");
    let main = fixture.transcript("proj", "sess-a");
    let replay = fixture.replay("proj", "sess-a", "replay");
    write_lines(&main, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&replay, &[line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);

    fs::remove_file(&replay).unwrap();
    store.refresh();
    assert_eq!(store.totals.record_count(), 2, "the records were real; the file going away does not unbill them");
    assert_eq!(store.totals.group_count(), 1);
    assert_eq!(store.refresh().transactions, 0, "and the deletion is only recorded once");
}

#[test]
fn a_group_whose_files_all_disappear_is_dropped() {
    let fixture = Fixture::new("prune");
    let gone = fixture.transcript("proj", "sess-gone");
    let kept = fixture.transcript("proj", "sess-kept");
    write_lines(&gone, &[line("sess-gone", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&kept, &[line("sess-kept", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.group_count(), 2);

    fs::remove_file(&gone).unwrap();
    let stats = store.refresh();
    assert_eq!(stats.rows_removed, 1);
    assert_eq!(store.totals.group_count(), 1);
    store.assert_matches_oracle("pruned");
    assert!(store.totals.parsed_offset(&gone).is_none());
}

#[test]
fn an_empty_discovery_never_wipes_the_store() {
    // A transiently missing Claude directory must not read as "the user
    // deleted every transcript".
    let fixture = Fixture::new("empty-discovery");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut store = fixture.store();
    store.refresh();

    store.totals.refresh_files(&[], pricing(), RefreshMode::Incremental);
    assert_eq!(store.totals.last_refresh().transactions, 0);
    assert_eq!(store.totals.record_count(), 1, "what is stored survives");
    store.refresh();
    store.assert_matches_oracle("recovered");
}

#[test]
fn an_empty_or_unreadable_corpus_is_an_empty_store_without_erroring() {
    let fixture = Fixture::new("empty-corpus");
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 0);
    assert_eq!(store.totals.group_count(), 0);

    // A file with no usage lines still forms an (empty) group.
    let path = fixture.transcript("proj", "sess-a");
    fs::write(&path, "{\"type\":\"summary\"}\nnot even json\n").unwrap();
    store.refresh();
    assert_eq!(store.totals.group_count(), 1);
    assert_eq!(store.offset(&path), len(&path));

    let empty = fixture.transcript("proj", "sess-empty");
    fs::write(&empty, "").unwrap();
    store.refresh();
    assert_eq!(store.offset(&empty), 0);
    store.assert_matches_oracle("empty files");
}

#[test]
fn lines_far_longer_than_a_read_block_round_trip() {
    // The reader works a block at a time; a line spanning several blocks has
    // to come out whole, through a first read and through a tail read.
    let fixture = Fixture::new("huge-line");
    let path = fixture.transcript("proj", "sess-a");
    let huge = |id: &str| {
        format!(
            r#"{{"timestamp":"2024-01-15T10:30:00.000Z","sessionId":"sess-a","requestId":"req-{id}","padding":"{}","message":{{"id":"msg-{id}","model":"claude-sonnet-4","usage":{{"input_tokens":10,"output_tokens":20}}}}}}"#,
            "p".repeat(19 * 1024 * 1024)
        )
    };
    write_lines(&path, &[huge("1")]);
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 1);
    assert_eq!(store.offset(&path), len(&path));

    append(&path, format!("{}\n", huge("2")).as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);
    assert_eq!(store.offset(&path), len(&path));
    store.assert_matches_oracle("two huge lines");
}

// ------------------------------------------------------------------ dedupe

#[test]
fn a_transcript_and_its_subagent_replays_form_one_deduped_group() {
    let fixture = Fixture::new("grouping");
    let main = fixture.transcript("proj", "sess-a");
    write_lines(
        &main,
        &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)],
    );
    write_lines(&fixture.replay("proj", "sess-a", "replay-1"), &[sidechain_line("sess-a", "msg-1", "req-8", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&fixture.replay("proj", "sess-a", "replay-2"), &[sidechain_line("sess-a", "msg-3", "req-9", "2024-01-15T10:32:00.000Z", 12, 22)]);
    write_lines(&fixture.transcript("proj", "sess-b"), &[line("sess-b", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);

    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.group_count(), 2);
    assert_eq!(store.totals.record_count(), 4, "msg-1's replay counts once; the same id in another session is another record");
    store.assert_matches_oracle("grouped");
}

#[test]
fn a_duplicate_split_across_two_session_groups_counts_once() {
    // A resumed session's transcript carries lines under the previous
    // session's id, so one billed message can sit in two groups.
    let fixture = Fixture::new("cross-group-dupe");
    let first = fixture.transcript("proj", "sess-a");
    let second = fixture.transcript("proj", "sess-b");
    let shared = line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20);
    write_lines(&first, &[shared.clone(), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    write_lines(&second, &[shared.clone(), line("sess-b", "msg-3", "req-3", "2024-01-15T10:32:00.000Z", 12, 22)]);

    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.group_count(), 2);
    assert_eq!(store.totals.record_count(), 3);
    store.assert_matches_oracle("cross-group");
    store.refresh();
    store.assert_matches_oracle("warm");
    store.rebuild();
    store.assert_matches_oracle("rebuilt");
}

#[test]
fn a_streamed_message_finalised_in_a_later_refresh_replaces_its_provisional_record() {
    // Claude Code writes an assistant message's usage twice under one key: a
    // provisional record when the stream opens, the real one when it closes.
    let fixture = Fixture::new("streamed-finalise");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 2, 2)]);
    let mut store = fixture.store();
    store.refresh();

    let finalised = final_line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:29.000Z", 2, 5064);
    append(&path, format!("{finalised}\n").as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 1, "the two writes are one message");
    assert_eq!(store.totals.token_totals().output, 5064, "the final record replaces the provisional one");
    assert!((store.totals.total_cost() - cost_of(&finalised)).abs() < 1e-12);
    store.assert_matches_oracle("finalised");
}

#[test]
fn a_worse_duplicate_arriving_later_never_displaces_a_better_one() {
    let fixture = Fixture::new("worse-duplicate");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[final_line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:29.000Z", 2, 5064)]);
    let mut store = fixture.store();
    store.refresh();

    append(&path, format!("{}\n", line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 2, 2)).as_bytes());
    store.refresh();
    assert_eq!(store.totals.token_totals().output, 5064, "the better record stands");
    store.assert_matches_oracle("worse duplicate");
}

#[test]
fn a_sidechain_replay_arriving_in_a_later_refresh_does_not_double_count() {
    let fixture = Fixture::new("late-replay");
    let main = fixture.transcript("proj", "sess-a");
    let replay = fixture.replay("proj", "sess-a", "replay");
    write_lines(&main, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&replay, &[line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    let mut store = fixture.store();
    store.refresh();

    append(&replay, format!("{}\n", sidechain_line("sess-a", "msg-1", "req-8", "2024-01-15T10:30:00.000Z", 10, 20)).as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);
    store.assert_matches_oracle("late replay");
}

#[test]
fn a_replay_that_predates_its_parents_finalising_rewrite_still_collapses() {
    // A subagent replay carries its parent's *final* timestamp. While the
    // parent is provisional the two do not match; once the parent's final line
    // lands, the partition settles to what one pass over everything gives.
    let fixture = Fixture::new("late-finalising-parent");
    let main = fixture.transcript("proj", "sess-a");
    let replay = fixture.replay("proj", "sess-a", "replay");
    write_lines(&main, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 2, 2)]);
    write_lines(&replay, &[sidechain_line("sess-a", "msg-1", "req-9", "2024-01-15T10:30:29.000Z", 2, 5064)]);
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 2, "timestamps still differ");
    store.assert_matches_oracle("provisional parent");

    let finalised = final_line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:29.000Z", 2, 5064);
    append(&main, format!("{finalised}\n").as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 1);
    assert!((store.totals.total_cost() - cost_of(&finalised)).abs() < 1e-12);
    store.assert_matches_oracle("finalised parent");

    append(&main, format!("{}\n", line("sess-a", "msg-2", "req-2", "2024-01-15T10:35:00.000Z", 10, 20)).as_bytes());
    store.refresh();
    assert_eq!(store.totals.record_count(), 2, "and it stays collapsed");
    store.assert_matches_oracle("after");
}

#[test]
fn totals_match_the_oracle_through_every_kind_of_change() {
    let fixture = Fixture::new("totals-match");
    let first = fixture.transcript("proj", "sess-a");
    let second = fixture.transcript("proj", "sess-b");
    let shared = line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20);
    write_lines(&first, &[shared.clone(), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    write_lines(&second, &[shared.clone(), line("sess-b", "msg-3", "req-3", "2024-01-15T10:32:00.000Z", 12, 22)]);
    let streamed = fixture.transcript("proj", "sess-d");
    write_lines(&streamed, &[line("sess-d", "msg-9", "req-9", "2024-01-15T10:33:00.000Z", 2, 2)]);

    let mut store = fixture.store();
    store.refresh();
    store.assert_matches_oracle("first load");
    assert_eq!(store.totals.record_count(), 4);

    for round in 0..3 {
        store.refresh();
        store.assert_matches_oracle(&format!("quiet refresh {round}"));
    }

    append(&first, format!("{}\n", line("sess-a", "msg-4", "req-4", "2024-01-15T10:34:00.000Z", 13, 23)).as_bytes());
    store.refresh();
    store.assert_matches_oracle("a group grew");

    append(&streamed, format!("{}\n", final_line("sess-d", "msg-9", "req-9", "2024-01-15T10:33:29.000Z", 2, 5064)).as_bytes());
    store.refresh();
    store.assert_matches_oracle("a streamed message was finalised");

    // The file holding the surviving copy loses it: the other group's copy,
    // a loser so far, has to count again.
    write_lines(&first, &[line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    store.refresh();
    store.assert_matches_oracle("the surviving duplicate disappeared");

    // A duplicate appearing in a new group that sorts first takes over.
    write_lines(&fixture.transcript("proj", "sess-0"), &[shared.clone(), line("sess-0", "msg-5", "req-5", "2024-01-15T10:35:00.000Z", 14, 24)]);
    store.refresh();
    store.assert_matches_oracle("a group appeared holding a duplicate");

    fs::remove_file(&second).unwrap();
    store.refresh();
    store.assert_matches_oracle("a group vanished");

    store.rebuild();
    store.assert_matches_oracle("rebuilt");
}

#[test]
fn a_rebuild_keeps_nothing_of_a_transcript_gone_since() {
    let fixture = Fixture::new("rebuild-forgets");
    write_lines(&fixture.transcript("proj-1", "sess-a"), &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let gone = fixture.transcript("proj-2", "sess-b");
    write_lines(&gone, &[line("sess-b", "msg-2", "req-2", "2024-01-15T09:00:00.000Z", 5, 5)]);
    let mut store = fixture.store();
    store.refresh();
    assert_eq!(store.totals.record_count(), 2);

    fs::remove_file(&gone).unwrap();
    store.rebuild();
    assert_eq!(store.totals.record_count(), 1, "the rebuild read only what is on disk");
    // And the database holds no more than that.
    let conn = rusqlite::Connection::open(store_path(&fixture.store_dir)).unwrap();
    let count = |table: &str| -> i64 { conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).unwrap() };
    assert_eq!((count("files"), count("groups"), count("rows"), count("buckets")), (1, 1, 1, 1), "nothing of the gone transcript was kept");
}

#[test]
fn rebuild_matches_the_incrementally_refreshed_store() {
    let fixture = Fixture::new("rebuild-parity");
    let main_a = fixture.transcript("proj-1", "sess-a");
    let replay_a = fixture.replay("proj-1", "sess-a", "replay");
    let main_b = fixture.transcript("proj-2", "sess-b");
    write_lines(&main_a, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    write_lines(&replay_a, &[sidechain_line("sess-a", "msg-1", "req-9", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&main_b, &[line("sess-b", "msg-5", "req-5", "2024-01-15T09:00:00.000Z", 5, 5)]);
    let mut store = fixture.store();
    store.refresh();

    append(&main_a, format!("{}\n", line("sess-a", "msg-3", "req-3", "2024-01-15T10:33:00.000Z", 12, 22)).as_bytes());
    append(&main_b, format!("{}\n", line("sess-b", "msg-6", "req-6", "2024-01-15T09:05:00.000Z", 6, 6)).as_bytes());
    store.refresh();
    append(&main_a, format!("{}\n", line("sess-a", "msg-4", "req-4", "2024-01-15T10:34:00.000Z", 13, 23)).as_bytes());
    append(&replay_a, format!("{}\n", sidechain_line("sess-a", "msg-4", "req-8", "2024-01-15T10:34:00.000Z", 13, 23)).as_bytes());
    store.refresh();
    append(&replay_a, format!("{}\n", sidechain_line("sess-a", "msg-3", "req-7", "2024-01-15T10:33:00.000Z", 12, 22)).as_bytes());
    store.refresh();
    store.assert_matches_oracle("incremental");
    let (records, cost) = (store.totals.record_count(), store.totals.total_cost());

    poison_preserving_metadata(&main_b);
    let stats = store.rebuild();
    assert!(stats.rebuilt);
    assert_eq!(stats.files_read, 3, "a rebuild reads everything again");
    assert!(store.models_on_the_15th().contains(&"claude-sonnet-3".to_owned()), "including what was poisoned");
    poison_preserving_metadata(&main_b);
    store.rebuild();
    assert_eq!(store.totals.record_count(), records);
    assert!((store.totals.total_cost() - cost).abs() < 1e-12);
    assert_eq!(store.refresh().transactions, 0, "a refresh straight after a rebuild is a no-op");
}

// ------------------------------------------------------- the database file

#[test]
fn a_corrupt_database_is_replaced_and_rebuilt() {
    let fixture = Fixture::new("corrupt-db");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    fixture.store().refresh();

    fs::write(store_path(&fixture.store_dir), b"this is not a database, not even a little").unwrap();
    fs::remove_file(format!("{}-wal", store_path(&fixture.store_dir).display())).ok();
    let mut store = fixture.store();
    let stats = store.refresh();
    assert!(stats.rebuilt, "an unusable database is started over");
    store.assert_matches_oracle("rebuilt from a corrupt file");
    assert_eq!(fixture.store().refresh().bytes_read, 0, "and the new one is persisted");
}

#[test]
fn a_database_from_a_newer_build_is_left_alone() {
    let fixture = Fixture::new("newer-db");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    fixture.store().refresh();
    {
        let conn = rusqlite::Connection::open(store_path(&fixture.store_dir)).unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    }

    let mut store = fixture.store();
    store.refresh();
    store.assert_matches_oracle("served from memory");
    let conn = rusqlite::Connection::open(store_path(&fixture.store_dir)).unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, 99, "a newer build's database is not migrated backwards or deleted");
}

#[test]
fn a_new_price_list_recomputes_costs_without_reading_a_transcript() {
    let fixture = Fixture::new("reprice");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(&path, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20), line("sess-a", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    fixture.store().refresh();

    // What a build with another price list left behind: other costs, and
    // another fingerprint.
    {
        let conn = rusqlite::Connection::open(store_path(&fixture.store_dir)).unwrap();
        conn.execute_batch("UPDATE rows SET cost = 999.0; UPDATE buckets SET cost = 999.0; UPDATE meta SET value = 'an older price list' WHERE key = 'pricing';").unwrap();
    }
    poison_preserving_metadata(&path);

    let mut store = fixture.store();
    let stats = store.refresh();
    assert!(stats.repriced);
    assert_eq!(stats.bytes_read, 0, "every input of a cost is stored");
    assert_eq!(store.models_on_the_15th(), sonnet_4(), "the transcript was not re-read");
    poison_preserving_metadata(&path);
    store.assert_matches_oracle("repriced");
    assert!(!fixture.store().refresh().repriced, "and only once");
}

// --------------------------------------------------------- what is reported

#[test]
fn the_active_block_follows_the_whole_chain_of_blocks_not_a_recent_window() {
    // Activity every 20 minutes for 14 hours, spread over several groups: no
    // idle gap, so where the current block starts depends on every earlier
    // record.
    let fixture = Fixture::new("block-chain");
    let start = 1_705_276_800_000i64 + 2 * HOUR; // 02:00Z
    let mut per_group: Vec<Vec<String>> = vec![Vec::new(); 4];
    for n in 0..(14 * 3) {
        let at = start + i64::try_from(n).unwrap() * 20 * 60_000;
        per_group[n % 4].push(line(&format!("sess-{}", n % 4), &format!("msg-{n}"), &format!("req-{n}"), &chrono_free_iso(at), 10 + n as u64, 20));
    }
    for (group, lines) in per_group.iter().enumerate() {
        write_lines(&fixture.transcript("proj", &format!("sess-{group}")), lines);
    }
    let mut store = fixture.store();
    let last = start + 14 * 3 * 20 * 60_000;
    store.now_ms = last;
    store.refresh();
    store.assert_matches_oracle("a block chain with no gap");
    let block = store.totals.active_block(last).expect("activity a moment ago");
    assert!(block.first_entry_ms > start);
}

#[test]
fn an_idle_gap_restarts_the_block_chain() {
    let fixture = Fixture::new("block-gap");
    let path = fixture.transcript("proj", "sess-a");
    write_lines(
        &path,
        &[
            line("sess-a", "msg-1", "req-1", "2024-01-15T01:00:00.000Z", 10, 20),
            line("sess-a", "msg-2", "req-2", "2024-01-15T02:00:00.000Z", 10, 20),
            // Six idle hours, then the current block.
            line("sess-a", "msg-3", "req-3", "2024-01-15T08:30:00.000Z", 10, 20),
            line("sess-a", "msg-4", "req-4", "2024-01-15T10:30:00.000Z", 10, 20),
        ],
    );
    let mut store = fixture.store();
    store.refresh();
    store.assert_matches_oracle("after a gap");
    assert_eq!(store.totals.active_block(NOW).map(|b| b.start_time_ms), Some(1_705_305_600_000), "08:00Z");
}

#[test]
fn a_few_thousand_records_load_once_and_then_cost_nothing() {
    let fixture = Fixture::new("perf");
    let mut expected = 0usize;
    for session in 0..20 {
        let path = fixture.transcript("proj", &format!("sess-{session:03}"));
        let lines: Vec<String> = (0..500)
            .map(|i| {
                line(
                    &format!("sess-{session:03}"),
                    &format!("msg-{session:03}-{i:04}"),
                    &format!("req-{session:03}-{i:04}"),
                    &format!("2024-01-15T{:02}:{:02}:{:02}.000Z", i / 3600, (i / 60) % 60, i % 60),
                    10,
                    20,
                )
            })
            .collect();
        expected += lines.len();
        write_lines(&path, &lines);
    }

    let mut store = fixture.store();
    let cold = Instant::now();
    store.refresh();
    let cold = cold.elapsed();
    assert_eq!(store.totals.record_count(), expected);

    let warm = Instant::now();
    let stats = store.refresh();
    let warm = warm.elapsed();
    assert_eq!((stats.bytes_read, stats.transactions), (0, 0));

    let reopened = Instant::now();
    let mut fresh = fixture.store();
    let stats = fresh.refresh();
    let reopened = reopened.elapsed();
    assert_eq!(stats.bytes_read, 0, "a new process reads the rollups, not the transcripts");
    assert_eq!(fresh.totals.record_count(), expected);

    println!("perf: {expected} records over 20 groups -- cold {cold:?}, warm {warm:?}, reopened {reopened:?}");
    assert!(warm.as_secs_f64() < 2.0 && reopened.as_secs_f64() < 2.0, "warm {warm:?}, reopened {reopened:?}");
}

/// `YYYY-MM-DDTHH:MM:SS.000Z` for an epoch-millisecond instant on 2024-01-15 or
/// just after.
fn chrono_free_iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let day = 15 + (days - 19_737);
    format!("2024-01-{day:02}T{:02}:{:02}:{:02}.000Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------------- Codex

/// A scratch `CODEX_HOME` with a store beside it. Its tests hand the store
/// explicit file lists, so nothing reads the real `~/.codex`.
struct CodexFixture {
    home: PathBuf,
    store_dir: PathBuf,
}

impl CodexFixture {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("sl-usage-codex-{}-{name}-{n}", std::process::id()));
        fs::remove_dir_all(&base).ok();
        let home = base.join("codex-home");
        let store_dir = base.join("store");
        fs::create_dir_all(home.join("sessions/2026/11/09")).unwrap();
        fs::create_dir_all(&store_dir).unwrap();
        Self { home, store_dir }
    }

    fn rollout(&self, thread: &str) -> PathBuf {
        self.home.join(format!("sessions/2026/11/09/rollout-2026-11-09T23-08-01-{thread}.jsonl"))
    }

    fn files(&self) -> Vec<PathBuf> {
        sl_usage::codex::codex_files(&self.home)
    }

    fn totals(&self) -> UsageTotals {
        UsageTotals::empty_for(&self.store_dir, sl_usage::Source::Codex)
    }

    fn refresh(&self, totals: &mut UsageTotals) -> sl_usage::RefreshStats {
        totals.refresh_files(&self.files(), pricing(), RefreshMode::Incremental);
        let stats = totals.last_refresh().clone();
        assert_eq!(stats.error, None, "a refresh of the scratch store should not fail");
        stats
    }

    /// Every response record across the fixture, priced from scratch: what the
    /// store must agree with however it was refreshed.
    fn full_scan(&self) -> (usize, f64) {
        let mut records = 0;
        let mut cost = 0.0;
        for file in self.files() {
            let rollout = sl_usage::codex::read_rollout(&file, pricing()).unwrap();
            records += rollout.records.len();
            cost += rollout.records.iter().map(|(_, r)| r.cost).sum::<f64>();
        }
        (records, cost)
    }
}

impl Drop for CodexFixture {
    fn drop(&mut self) {
        if let Some(base) = self.home.parent() {
            fs::remove_dir_all(base).ok();
        }
    }
}

const THREAD_A: &str = "01a1c25a-a06b-7471-8a01-1a5dd574e6e4";
const THREAD_B: &str = "01a1c25a-a0b2-7083-bf71-eb249595094c";

fn codex_meta(thread: &str, cwd: &str) -> String {
    format!(r#"{{"timestamp":"2026-11-09T22:10:32.945Z","type":"session_meta","payload":{{"id":"{thread}","cwd":"{cwd}","originator":"codex-tui","source":"cli"}}}}"#)
}

fn codex_turn(turn: &str, model: &str) -> String {
    format!(r#"{{"timestamp":"2026-11-09T22:10:33.260Z","type":"turn_context","payload":{{"turn_id":"{turn}","cwd":"/x","model":"{model}"}}}}"#)
}

fn codex_usage(thread: &str, turn: &str, response: &str, ts: &str, input: u64, cached: u64, output: u64) -> String {
    format!(
        r#"{{"timestamp":"{ts}","type":"token_usage_record","payload":{{"thread_id":"{thread}","turn_id":"{turn}","response_id":"{response}","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output}}}}}}}"#
    )
}

#[test]
fn a_codex_store_keeps_its_own_file_and_counts_response_records() {
    let fx = CodexFixture::new("own-file");
    write_lines(
        &fx.rollout(THREAD_A),
        &[codex_meta(THREAD_A, "/repo/a"), codex_turn("t1", "gpt-6-astra"), codex_usage(THREAD_A, "t1", "r1", "2026-11-09T22:10:36.025Z", 1000, 400, 50)],
    );
    let mut totals = fx.totals();
    fx.refresh(&mut totals);
    assert!(fx.store_dir.join("usage.sqlite").exists(), "one database for both");
    assert!(!fx.store_dir.join("usage-codex.sqlite").exists());
    assert_eq!(totals.record_count(), 1);
    let tokens = totals.token_totals();
    assert_eq!((tokens.input, tokens.cache_read, tokens.output, tokens.cache_write), (600, 400, 50, 0));
    let sessions = totals.session_summaries();
    assert_eq!((sessions[0].session_id.as_str(), sessions[0].project_path.as_str()), (THREAD_A, "/repo/a"));
    assert_eq!(sessions[0].models, vec!["gpt-6-astra".to_owned()]);
}

#[test]
fn a_codex_rollout_is_re_read_whole_so_a_late_turn_context_reprices_its_records() {
    let fx = CodexFixture::new("late-context");
    let path = fx.rollout(THREAD_A);
    write_lines(&path, &[codex_meta(THREAD_A, "/repo"), codex_usage(THREAD_A, "t1", "r1", "2026-11-09T22:10:36.025Z", 1000, 0, 10)]);
    let mut totals = fx.totals();
    fx.refresh(&mut totals);
    assert!(totals.model_totals().contains_key("unknown"), "no model yet");

    append(&path, format!("{}\n{}\n", codex_turn("t1", "gpt-6.1-sol"), codex_usage(THREAD_A, "t2", "r2", "2026-11-09T22:11:00.000Z", 10, 0, 1)).as_bytes());
    let stats = fx.refresh(&mut totals);
    assert_eq!(stats.bytes_read, len(&path), "the whole file is read again");
    let models = totals.model_totals();
    assert_eq!(models.get("gpt-6.1-sol").map(|m| m.tokens), Some(1010), "t1's record moved to the model named after it: {models:?}");
    assert_eq!(models.get("unknown").map(|m| m.tokens), Some(11), "t2 has neither a context nor thread settings");
    assert_eq!(totals.record_count(), 2, "re-reading replaces rows, never adds a second copy");
    let (records, cost) = fx.full_scan();
    assert_eq!(totals.record_count(), records);
    assert!((totals.total_cost() - cost).abs() < 1e-12);

    assert_eq!(fx.refresh(&mut totals).files_read, 0, "an untouched rollout is not read");
}

#[test]
fn an_incremental_codex_store_agrees_with_a_full_scan_through_appends_and_deletes() {
    let fx = CodexFixture::new("agree");
    let (a, b) = (fx.rollout(THREAD_A), fx.rollout(THREAD_B));
    write_lines(&a, &[codex_meta(THREAD_A, "/a"), codex_turn("t1", "gpt-6-astra"), codex_usage(THREAD_A, "t1", "r1", "2026-11-09T10:00:00.000Z", 300_000, 1_000, 10)]);
    write_lines(&b, &[codex_meta(THREAD_B, "/b"), codex_turn("u1", "gpt-5.5"), codex_usage(THREAD_B, "u1", "s1", "2026-11-09T11:00:00.000Z", 50, 25, 5)]);
    let mut totals = fx.totals();
    for step in 0..3 {
        match step {
            1 => append(&a, format!("{}\n", codex_usage(THREAD_A, "t1", "r2", "2026-11-09T10:05:00.000Z", 2_000, 1_000, 20)).as_bytes()),
            2 => fs::remove_file(&b).unwrap(),
            _ => {}
        }
        fx.refresh(&mut totals);
        let (records, cost) = fx.full_scan();
        assert_eq!(totals.record_count(), records, "step {step}");
        assert!((totals.total_cost() - cost).abs() < 1e-9, "step {step}: {} vs {cost}", totals.total_cost());
    }
    assert_eq!(totals.group_count(), 1, "a thread whose only rollout is gone is dropped");

    let mut rebuilt = UsageTotals::empty_for(&fx.store_dir.join("rebuilt"), sl_usage::Source::Codex);
    rebuilt.refresh_files(&fx.files(), pricing(), RefreshMode::Rebuild);
    assert_eq!(rebuilt.record_count(), totals.record_count());
    assert!((rebuilt.total_cost() - totals.total_cost()).abs() < 1e-12);
}

#[test]
fn a_rollout_compressed_in_place_is_not_counted_twice() {
    let fx = CodexFixture::new("compressed");
    let plain = fx.rollout(THREAD_A);
    let body = [codex_meta(THREAD_A, "/a"), codex_turn("t1", "gpt-6-astra"), codex_usage(THREAD_A, "t1", "r1", "2026-11-09T10:00:00.000Z", 100, 0, 1)].join("\n");
    fs::write(&plain, &body).unwrap();
    let mut totals = fx.totals();
    fx.refresh(&mut totals);
    assert_eq!(totals.record_count(), 1);

    // What Codex's compression worker does: write `.jsonl.zst`, drop `.jsonl`.
    // A single raw zstd block stands in for the compressor.
    let zst = PathBuf::from(format!("{}.zst", plain.display()));
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x60];
    frame.extend_from_slice(&u16::try_from(body.len() - 256).unwrap().to_le_bytes());
    frame.extend_from_slice(&((u32::try_from(body.len()).unwrap() << 3) | 1).to_le_bytes()[..3]);
    frame.extend_from_slice(body.as_bytes());
    fs::write(&zst, frame).unwrap();
    fs::remove_file(&plain).unwrap();

    fx.refresh(&mut totals);
    assert_eq!(totals.record_count(), 1, "the old path's rows and the compressed copy dedupe on (thread, response)");
    assert_eq!(totals.group_count(), 1);
}

#[test]
fn a_new_price_list_reprices_codex_priority_records_at_the_fast_multiplier() {
    let fx = CodexFixture::new("reprice");
    let settings = r#"{"timestamp":"2026-11-09T22:10:33.260Z","type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model":"gpt-6-astra","service_tier":"priority"}}}"#;
    write_lines(&fx.rollout(THREAD_A), &[codex_meta(THREAD_A, "/a"), settings.to_owned(), codex_usage(THREAD_A, "t1", "r1", "2026-11-09T10:00:00.000Z", 1000, 0, 10)]);
    let mut totals = fx.totals();
    fx.refresh(&mut totals);
    let parsed = totals.total_cost();
    assert!((parsed - 2.0 * (1000.0 * 10.0 + 10.0 * 50.0) / 1e6).abs() < 1e-12, "priority bills twice the standard price: {parsed}");

    // Force a reprice: forget the stored fingerprint, as a new catalog would.
    let conn = rusqlite_free_reset(&fx.store_dir.join("usage.sqlite"), "pricing:codex");
    drop(conn);
    let mut reopened = fx.totals();
    reopened.refresh_files(&fx.files(), pricing(), RefreshMode::Incremental);
    assert!(reopened.last_refresh().repriced);
    assert!((reopened.total_cost() - parsed).abs() < 1e-12, "a reprice keeps the multiplier: {}", reopened.total_cost());
}

/// Overwrite the stored pricing fingerprint with the `sqlite3` CLI, so the
/// next refresh reprices. Kept out of the crate's API on purpose.
fn rusqlite_free_reset(db: &Path, key: &str) -> std::process::Output {
    std::process::Command::new("sqlite3").arg(db).arg(format!("UPDATE meta SET value = 'stale' WHERE key = '{key}';")).output().expect("sqlite3 runs")
}

#[test]
fn claude_and_codex_share_one_database_without_touching_each_other() {
    let claude_fx = Fixture::new("shared-claude");
    let codex_fx = CodexFixture::new("shared-codex");
    write_lines(
        &claude_fx.transcript("proj", "sess-a"),
        &[line("sess-a", "m1", "q1", "2024-01-15T10:30:00.000Z", 10, 20), line("sess-a", "m2", "q2", "2024-01-15T10:31:00.000Z", 5, 5)],
    );
    write_lines(
        &codex_fx.rollout(THREAD_A),
        &[codex_meta(THREAD_A, "/a"), codex_turn("t1", "gpt-6-astra"), codex_usage(THREAD_A, "t1", "r1", "2026-11-09T10:00:00.000Z", 100, 0, 1)],
    );
    // Both stores in the Claude fixture's directory: one `usage.sqlite`.
    let dir = &claude_fx.store_dir;
    let mut claude = UsageTotals::empty(dir);
    let mut codex = UsageTotals::empty_for(dir, sl_usage::Source::Codex);
    let refresh_claude = |t: &mut UsageTotals, mode| t.refresh_files(&claude_fx.files(), pricing(), mode);
    let refresh_codex = |t: &mut UsageTotals, mode| t.refresh_files(&codex_fx.files(), pricing(), mode);
    refresh_claude(&mut claude, RefreshMode::Incremental);
    refresh_codex(&mut codex, RefreshMode::Incremental);
    let (claude_records, codex_records) = (claude.record_count(), codex.record_count());
    assert_eq!(codex_records, 1);
    assert!(claude_records > 0);
    assert_eq!(fs::read_dir(dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".sqlite")).count(), 1);

    // Each sees only its own, after the other committed and after reopening.
    refresh_claude(&mut claude, RefreshMode::Incremental);
    refresh_codex(&mut codex, RefreshMode::Incremental);
    assert_eq!((claude.record_count(), codex.record_count()), (claude_records, codex_records));
    assert!(claude.model_totals().keys().all(|m| !m.starts_with("gpt")));
    assert!(codex.model_totals().keys().all(|m| m.starts_with("gpt")));
    // And so do the reads off the time index: each source's counted rows,
    // and the tail the active block is folded from.
    let times = |t: &UsageTotals| t.entries_since(0).iter().map(|e| e.timestamp_ms).collect::<Vec<_>>();
    assert_eq!(times(&claude), [1_705_314_600_000, 1_705_314_660_000], "Claude's two, in order");
    assert_eq!(times(&codex), [1_794_218_400_000], "Codex's one");
    let after_codex = 1_794_218_460_000;
    assert_eq!(codex.active_block(after_codex).map(|b| b.start_time_ms), Some(1_794_218_400_000), "Codex's tail is its own");
    assert!(claude.active_block(after_codex).is_none(), "and Claude's has none of it");

    // A Claude rebuild leaves Codex's rows, and an empty Codex discovery
    // never wipes them either.
    refresh_claude(&mut claude, RefreshMode::Rebuild);
    let mut reopened = UsageTotals::empty_for(dir, sl_usage::Source::Codex);
    reopened.refresh_files(&[], pricing(), RefreshMode::Incremental);
    assert_eq!(reopened.record_count(), codex_records);
    assert_eq!(claude.record_count(), claude_records);

    // Repricing one source leaves the other's fingerprint, and costs, alone.
    rusqlite_free_reset(&dir.join("usage.sqlite"), "pricing:codex");
    let mut claude_again = UsageTotals::empty(dir);
    refresh_claude(&mut claude_again, RefreshMode::Incremental);
    assert!(!claude_again.last_refresh().repriced, "Codex's stale fingerprint is not Claude's");
    let mut codex_again = UsageTotals::empty_for(dir, sl_usage::Source::Codex);
    refresh_codex(&mut codex_again, RefreshMode::Incremental);
    assert!(codex_again.last_refresh().repriced);
    assert!((codex_again.total_cost() - codex.total_cost()).abs() < 1e-12);
    assert!((claude_again.total_cost() - claude.total_cost()).abs() < 1e-12);
}

#[test]
fn opening_a_current_database_does_not_wait_for_the_write_lock() {
    let fx = Fixture::new("open-unlocked");
    write_lines(&fx.transcript("proj", "sess-a"), &[line("sess-a", "m1", "q1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    fx.store().refresh();
    // Another process in the middle of a write.
    let writer = rusqlite::Connection::open(store_path(&fx.store_dir)).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let start = Instant::now();
    let mut store = fx.store();
    let stats = store.refresh();
    assert!(start.elapsed().as_secs() < 3, "a quiet refresh did not wait: {:?}", start.elapsed());
    assert_eq!((stats.rebuilt, stats.transactions, store.totals.record_count()), (false, 0, 1));
    writer.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn a_database_from_an_older_build_is_rebuilt() {
    let fx = Fixture::new("older-db");
    write_lines(&fx.transcript("proj", "sess-a"), &[line("sess-a", "m1", "q1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    let mut store = fx.store();
    store.refresh();
    let records = store.totals.record_count();
    assert_eq!(records, 1);
    drop(store);
    // Back to what a version-2 build wrote: no `rows.source` column.
    let db = store_path(&fx.store_dir);
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP INDEX rows_timeline; ALTER TABLE rows DROP COLUMN source; PRAGMA user_version = 2;").unwrap();
    }
    let inode = |path: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(path).unwrap());
    let before = inode(&db);
    // An older tray with the file open: its connection is left pointing at
    // the same file, now on the new schema.
    let older = rusqlite::Connection::open(&db).unwrap();
    let mut store = fx.store();
    let stats = store.refresh();
    assert!(stats.rebuilt, "an older schema is rebuilt, not migrated");
    assert_eq!(inode(&db), before, "in place: the file is not deleted from under an open connection");
    let seen: i64 = older.query_row("SELECT count(*) FROM rows WHERE source = 0", [], |row| row.get(0)).unwrap();
    assert_eq!(seen, 1, "the open connection sees the rebuilt store");
    assert_eq!(stats.files_read, 1, "from the transcripts");
    assert_eq!(store.totals.record_count(), records);
    store.assert_matches_oracle("rebuilt from an older schema");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, sl_usage::STORE_SCHEMA_VERSION, "at this build's version");
}

// ------------------------------------------------------ a watched refresh

/// Set `path`'s mtime an hour back, so that neither the walk's nor the
/// store's "changed moments ago" rule makes it look again: only an event can.
fn backdate(path: &Path) {
    let old = SystemTime::now() - std::time::Duration::from_secs(3600);
    File::open(path).unwrap().set_times(FileTimes::new().set_modified(old)).unwrap();
}

fn backdate_tree(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let path = entry.path();
        if path.is_dir() {
            backdate_tree(&path);
        }
        backdate(&path);
    }
    backdate(dir);
}

/// A refresh driven by file-system events finds what the oracle does after
/// every kind of change, and in between `stat`s no transcript.
#[test]
fn a_watched_refresh_follows_the_corpus_by_events_alone() {
    let fixture = Fixture::new("watched");
    let (a, b) = (fixture.transcript("proj", "sess-a"), fixture.transcript("proj", "sess-b"));
    write_lines(&a, &[line("sess-a", "msg-1", "req-1", "2024-01-15T10:30:00.000Z", 10, 20)]);
    write_lines(&b, &[line("sess-b", "msg-2", "req-2", "2024-01-15T10:31:00.000Z", 11, 21)]);
    let root = fixture.root.join("projects");
    backdate_tree(&root);
    let roots = [root.clone()];

    let mut store = Store { fixture: &fixture, totals: UsageTotals::empty(&fixture.store_dir).watched(), now_ms: NOW };
    let refresh = |store: &mut Store<'_>| {
        store.totals.refresh_under(&roots, pricing(), RefreshMode::Incremental);
        let stats = store.totals.last_refresh().clone();
        assert_eq!(stats.error, None, "a refresh must not fail");
        stats
    };
    // Refresh until the store holds what the oracle does: events arrive after
    // a delay, so wait on the condition.
    let converge = |store: &mut Store<'_>, step: &str| {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while store.totals.record_count() != store.fixture.oracle().record_count() {
            assert!(Instant::now() < deadline, "{step}: timed out waiting for the refresh to catch up");
            std::thread::sleep(std::time::Duration::from_millis(25));
            refresh(store);
        }
        store.assert_matches_oracle(step);
    };

    let first = refresh(&mut store);
    assert_eq!((first.files_statted, first.files_read), (2, 2), "the first refresh looks at everything");
    store.assert_matches_oracle("first load");

    // Quiet: the stray events of the set-up die down, and then nothing is
    // `stat`ed at all.
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while refresh(&mut store).files_statted != 0 {
        assert!(Instant::now() < deadline, "the refresh never went quiet");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    for _ in 0..3 {
        let quiet = refresh(&mut store);
        assert_eq!((quiet.files_statted, quiet.files_read, quiet.transactions), (0, 0, 0), "a quiet watched refresh stats nothing");
    }

    append(&a, format!("{}\n", line("sess-a", "msg-3", "req-3", "2024-01-15T10:32:00.000Z", 12, 22)).as_bytes());
    converge(&mut store, "an append");
    assert_eq!(store.totals.record_count(), 3);
    assert_eq!(store.offset(&a), len(&a));

    write_lines(&fixture.replay("proj2", "sess-c", "agent-1"), &[line("sess-c", "msg-4", "req-4", "2024-01-15T10:33:00.000Z", 13, 23)]);
    converge(&mut store, "a new file in a new subdirectory");
    assert_eq!(store.totals.record_count(), 4);

    fs::remove_file(&b).unwrap();
    fs::remove_dir_all(root.join("proj2")).unwrap();
    // A group whose files are all gone is dropped by the store; wait for the
    // walk to have noticed, then compare with the oracle.
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while store.totals.discovered().len() != 1 {
        assert!(Instant::now() < deadline, "removals never noticed");
        std::thread::sleep(std::time::Duration::from_millis(25));
        refresh(&mut store);
    }
    store.assert_matches_oracle("files and a directory removed");
}
