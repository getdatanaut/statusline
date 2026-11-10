//! The usage store: every parsed usage line in one SQLite database, plus the
//! rollups the menu reads.
//!
//! ## What is stored
//!
//! - `files`: every transcript seen, with how far it has been read
//!   (`parsed_offset`, always on a line boundary) and the `size` / `mtime` /
//!   `inode` that tell an untouched file from a grown or replaced one.
//! - `rows`: one per usage line, keyed by `(file_id, byte_offset)`, carrying the
//!   record's fields and `counted`: whether it survives dedupe.
//! - `buckets`: per session group, day and model, the sums of the counted rows.
//!   Everything the tray shows is a fold over these.
//!
//! ## Dedupe
//!
//! Both of [`crate::dedupe`]'s match predicates require an equal `message_id`
//! and an equal effective `session_id`, so rows fall into partitions by that
//! pair and dedupe never crosses one. A refresh re-settles only the partitions
//! it touched: load the partition's rows in `(path, byte_offset)` order, run
//! [`dedupe_indices_to_fixpoint`] over them, set `counted` to match. That is the
//! same function over the same rows in the same order however the refreshes
//! were interleaved, so an incremental store and a rebuilt one agree by
//! construction; and it is exactly what one dedupe pass over the whole corpus
//! in path order gives, which is what ccusage does. Rows without a
//! `message_id` are never deduped and always count.
//!
//! Losers are kept, not deleted. A replaced file deletes its own rows and
//! re-settles the partitions they were in, and a loser elsewhere that was only
//! hidden by one of them counts again.
//!
//! ## Rollups are rebuilt, never adjusted
//!
//! A bucket whose rows changed is recomputed from those rows in the same
//! transaction. Nothing is added or subtracted, so a streamed message whose
//! final record replaces its provisional one cannot leave a stale contribution
//! behind.
//!
//! ## I/O
//!
//! A quiet refresh is a `stat()` sweep -- or, with a watched walk, only the
//! files file-system events named -- and opens no transaction. A refresh that
//! finds appended bytes reads those bytes, writes their rows and the few
//! buckets they land in, and nothing else. Startup reads the buckets, not the
//! rows.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use memchr::memrchr;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::aggregate::ModelAggregate;
use super::{RefreshMode, Source};
use crate::aggregate::BLOCK_DURATION_MS;
use crate::dedupe::dedupe_indices_to_fixpoint;
use crate::pricing::{PricingMap, UsageTokens, cost_for_usage};
use crate::record::{Speed, UsageRecord, parse_lines};

/// The database file, inside the store directory.
pub const DB_FILE: &str = "usage.sqlite";

/// `PRAGMA user_version` of a current database. A database of an older
/// version is emptied and rebuilt from the transcripts rather than migrated:
/// it is a cache.
/// Never lowered -- a higher version than this build's reads as a newer build's
/// database, which is left alone.
pub const STORE_SCHEMA_VERSION: i64 = 3;

/// The schema. Every file is Claude Code's or Codex's (`source`), and its
/// group, rows and buckets are its file's; a row repeats its file's source so
/// a source's counted rows are read in time order off one index.
const SCHEMA: &str = r"
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
    CREATE TABLE groups (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE);
    CREATE TABLE files (
        id INTEGER PRIMARY KEY,
        group_id INTEGER NOT NULL,
        path TEXT NOT NULL UNIQUE,
        project TEXT NOT NULL,
        size INTEGER NOT NULL,
        mtime_ms INTEGER NOT NULL,
        inode INTEGER NOT NULL,
        parsed_offset INTEGER NOT NULL,
        gone INTEGER NOT NULL DEFAULT 0,
        source INTEGER NOT NULL
    );
    CREATE TABLE strings (id INTEGER PRIMARY KEY, value TEXT NOT NULL UNIQUE);
    CREATE TABLE rows (
        id INTEGER PRIMARY KEY,
        file_id INTEGER NOT NULL,
        group_id INTEGER NOT NULL,
        source INTEGER NOT NULL,
        byte_offset INTEGER NOT NULL,
        partition_hash INTEGER,
        timestamp_ms INTEGER NOT NULL,
        day TEXT NOT NULL,
        session INTEGER NOT NULL,
        model INTEGER NOT NULL,
        message_id TEXT,
        request_id TEXT,
        input INTEGER NOT NULL,
        output INTEGER NOT NULL,
        cc_5m INTEGER NOT NULL,
        cc_1h INTEGER NOT NULL,
        cache_read INTEGER NOT NULL,
        is_sidechain INTEGER NOT NULL,
        speed INTEGER,
        cost REAL NOT NULL,
        missing_pricing INTEGER NOT NULL,
        counted INTEGER NOT NULL,
        UNIQUE (file_id, byte_offset)
    );
    CREATE INDEX rows_partition ON rows (partition_hash) WHERE partition_hash IS NOT NULL;
    CREATE INDEX rows_bucket ON rows (group_id, day, model) WHERE counted = 1;
    CREATE INDEX rows_timeline ON rows (source, timestamp_ms) WHERE counted = 1;
    CREATE TABLE buckets (
        group_id INTEGER NOT NULL,
        day TEXT NOT NULL,
        model INTEGER NOT NULL,
        input INTEGER NOT NULL,
        output INTEGER NOT NULL,
        cc_5m INTEGER NOT NULL,
        cc_1h INTEGER NOT NULL,
        cache_read INTEGER NOT NULL,
        cost REAL NOT NULL,
        messages INTEGER NOT NULL,
        unpriced INTEGER NOT NULL,
        first_ts INTEGER NOT NULL,
        last_ts INTEGER NOT NULL,
        project TEXT NOT NULL,
        PRIMARY KEY (group_id, day, model)
    ) WITHOUT ROWID;
    CREATE INDEX files_source ON files (source);
";

/// `strings` id meaning "no model" (a `<synthetic>` record). Real ids start at 1.
const NO_MODEL: i64 = 0;

/// A transaction is committed after this many transcript bytes, so a first run
/// over gigabytes of history neither holds it all in one WAL nor loses it all
/// to an interruption.
const CHUNK_BYTES: u64 = 256 * 1024 * 1024;

/// A refresh reading at least this much is a bulk load (a first run, a
/// rebuild) and gets a bigger page cache while it runs.
const BULK_BYTES: u64 = 64 * 1024 * 1024;

/// Transcripts are read this much at a time, so a 250 MB transcript does not
/// have to fit in memory at once. A line longer than this grows the buffer.
const READ_BLOCK: usize = 8 * 1024 * 1024;

/// One counted record, as the active 5-hour block is folded from. A store keeps
/// every one since its last idle gap longer than a block, ascending.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockEntry {
    pub timestamp_ms: i64,
    pub cost: f64,
    pub tokens: u64,
}

type TailEntry = BlockEntry;

/// A rollup row: one session group's counted records on one day for one model.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Bucket {
    pub aggregate: ModelAggregate,
    pub unpriced: bool,
    pub first_ts: i64,
    pub last_ts: i64,
    /// The project of the bucket's earliest row.
    pub project: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct BucketKey {
    pub group: i64,
    pub day: String,
    pub model: i64,
}

/// What one refresh did, for tracing and for the tests that pin "a quiet
/// refresh does nothing".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Transcripts `stat`ed: all of them without a watched walk's hint, and
    /// only the ones events named (and new or recently modified ones) with it.
    pub files_statted: usize,
    /// Transcripts whose bytes were read.
    pub files_read: usize,
    /// Transcript bytes read.
    pub bytes_read: u64,
    /// Rows inserted and removed.
    pub rows_written: usize,
    pub rows_removed: usize,
    /// Dedupe partitions re-settled and rollup buckets rebuilt.
    pub partitions: usize,
    pub buckets: usize,
    /// Transactions committed.
    pub transactions: usize,
    /// Every stored cost was recomputed under a new pricing fingerprint.
    pub repriced: bool,
    /// The store started from nothing: first run, `--rebuild`, or a database
    /// that could not be used.
    pub rebuilt: bool,
    /// The last error, if the refresh hit one. The store is left as of the last
    /// committed transaction, which is consistent.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStat {
    size: u64,
    mtime_ms: i64,
    inode: u64,
}

#[derive(Debug, Clone)]
struct FileRow {
    id: i64,
    group: i64,
    size: u64,
    mtime_ms: i64,
    inode: u64,
    parsed_offset: u64,
    gone: bool,
}

/// What the store holds in memory: the small tables, and the rollups.
#[derive(Debug, Default)]
pub(super) struct State {
    files: HashMap<String, FileRow>,
    groups: HashMap<String, i64>,
    group_names: HashMap<i64, String>,
    strings: HashMap<String, i64>,
    string_values: HashMap<i64, String>,
    pub buckets: BTreeMap<BucketKey, Bucket>,
    pub tail: Vec<TailEntry>,
}

impl State {
    pub fn group_name(&self, group: i64) -> Option<&str> {
        self.group_names.get(&group).map(String::as_str)
    }

    /// The model a bucket or row id stands for; `None` for no model.
    pub fn model_name(&self, model: i64) -> Option<&str> {
        (model != NO_MODEL).then(|| self.string_values.get(&model).map(String::as_str)).flatten()
    }

    /// Session groups with at least one transcript still on disk.
    pub fn live_groups(&self) -> usize {
        self.files.values().filter(|file| !file.gone).map(|file| file.group).collect::<HashSet<_>>().len()
    }

    /// How far a transcript has been read, for tests.
    pub fn parsed_offset(&self, path: &Path) -> Option<u64> {
        self.files.get(path_key(path).as_str()).map(|file| file.parsed_offset)
    }
}

/// A unit of work found by the `stat()` sweep.
enum Work {
    /// Read `[from, size)` of a transcript; `replace` drops all its rows first.
    Read { path: PathBuf, key: String, group: String, project: String, stat: FileStat, from: u64, replace: bool },
    /// Metadata moved but there is nothing new to read.
    Touch { key: String, stat: FileStat },
    /// Gone from disk while its session group lives on: its rows stay.
    Gone { key: String },
    /// Every transcript of the group is gone: drop it.
    DropGroup { group: i64 },
}

impl Work {
    fn key(&self) -> Option<&str> {
        match self {
            Work::Read { key, .. } | Work::Touch { key, .. } | Work::Gone { key } => Some(key),
            Work::DropGroup { .. } => None,
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Work::Read { stat, from, .. } => stat.size.saturating_sub(*from),
            _ => 0,
        }
    }
}

/// The store: a connection, opened on first use, and what is held of it.
pub(super) struct Store {
    dir: PathBuf,
    source: Source,
    conn: Option<Connection>,
    /// `PRAGMA data_version` as of the last time `state` was in step with the
    /// database. It changes when *another* connection commits -- a second tray
    /// process sharing the file -- and then `state` is reloaded.
    data_version: i64,
    /// The files the last refresh found work for. A watched walk's hint is
    /// spent once it is handed over, so these are looked at again next time
    /// whatever it says: a read that failed is retried.
    retry: HashSet<String>,
    pub state: State,
}

impl Store {
    pub fn new(dir: &Path, source: Source) -> Self {
        Self { dir: dir.to_path_buf(), source, conn: None, data_version: 0, retry: HashSet::new(), state: State::default() }
    }

    /// Refresh against `files`. `changed`, when given, is what a watched walk
    /// says was touched since the last refresh: only those files (and new,
    /// recently modified or retried ones) are `stat`ed. `None` stats every one.
    pub fn refresh(&mut self, files: &[PathBuf], changed: Option<&HashSet<PathBuf>>, pricing: &PricingMap, mode: RefreshMode) -> RefreshStats {
        let mut stats = RefreshStats::default();
        if let Err(error) = self.try_refresh(files, changed, pricing, mode, &mut stats) {
            // Whatever the hint said was lost with the refresh.
            self.retry.clear();
            stats.error = Some(error.to_string());
            // Whatever was held may now be ahead of what was committed.
            if let Some(conn) = &self.conn
                && let Ok(state) = load_state(conn, self.source)
            {
                self.state = state;
            }
        }
        stats
    }

    fn try_refresh(&mut self, files: &[PathBuf], changed: Option<&HashSet<PathBuf>>, pricing: &PricingMap, mode: RefreshMode, stats: &mut RefreshStats) -> rusqlite::Result<()> {
        let source = self.source;
        let conn = if let Some(conn) = self.conn.take() {
            self.conn.insert(conn)
        } else {
            let conn = open(&self.dir.join(DB_FILE));
            stats.rebuilt = is_empty(&conn, source)?;
            self.state = load_state(&conn, source)?;
            self.data_version = data_version(&conn)?;
            self.conn.insert(conn)
        };

        let version = data_version(conn)?;
        if version != self.data_version {
            self.state = load_state(conn, source)?;
        }

        if mode == RefreshMode::Rebuild {
            wipe(conn, source)?;
            self.state = State::default();
            stats.rebuilt = true;
        }

        let fingerprint = pricing.fingerprint().to_string();
        let pricing_key = pricing_key(source);
        let stored: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [pricing_key], |row| row.get(0)).optional()?;
        if stored.as_deref() != Some(fingerprint.as_str()) {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if stored.is_some() {
                reprice(&tx, &mut self.state, pricing, source, stats)?;
                stats.repriced = true;
            }
            tx.execute("INSERT INTO meta (key, value) VALUES (?2, ?1) ON CONFLICT (key) DO UPDATE SET value = ?1", [fingerprint.as_str(), pricing_key])?;
            tx.commit()?;
            stats.transactions += 1;
        }

        // A rebuild reads everything.
        let changed = changed.filter(|_| mode == RefreshMode::Incremental);
        let work = plan(&self.state, files, source, changed.map(|changed| Hint { changed, retry: &self.retry, now_ms: now_ms() }).as_ref(), &mut stats.files_statted);
        self.retry = work.iter().filter_map(Work::key).map(str::to_owned).collect();
        // A first run inserts rows all over every index; with SQLite's default
        // 2 MB page cache that is mostly evicting and re-reading pages. A big
        // cache for the bulk load only, handed back afterwards, so a tray that
        // has settled into refreshing a few lines does not keep it.
        let bulk = work.iter().map(Work::bytes).sum::<u64>() >= BULK_BYTES;
        if bulk {
            conn.execute_batch("PRAGMA cache_size = -262144;")?;
        }
        let result = apply_all(conn, &mut self.state, work, pricing, source, stats);
        if bulk {
            conn.execute_batch("PRAGMA cache_size = -2000; PRAGMA shrink_memory;")?;
            // Fold the bulk load into the database now, and give the WAL's
            // space back, rather than leaving it to the next checkpoint.
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        }
        result?;
        // Our own commits leave `data_version` alone, so if it moved since the
        // start of this refresh another process committed meanwhile, and what
        // is held may be missing it.
        let after = data_version(conn)?;
        if after != version {
            self.state = load_state(conn, source)?;
        } else if stats.transactions > 0 {
            self.state.tail = load_tail(conn, source)?;
        }
        self.data_version = after;
        Ok(())
    }

    /// Counted records since `since_ms`, ascending. `None` before the store is
    /// opened or when the read fails.
    pub fn entries_since(&self, since_ms: i64) -> Option<Vec<BlockEntry>> {
        let conn = self.conn.as_ref()?;
        let mut statement = conn.prepare_cached(ENTRIES_SINCE).ok()?;
        let rows = statement
            .query_map([since_ms, source_code(self.source)], |row| Ok(BlockEntry { timestamp_ms: row.get(0)?, cost: row.get(1)?, tokens: row.get::<_, i64>(2)?.cast_unsigned() }))
            .ok()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().ok()
    }
}

/// Apply `work` a chunk at a time, one transaction per chunk.
fn apply_all(conn: &mut Connection, state: &mut State, work: Vec<Work>, pricing: &PricingMap, source: Source, stats: &mut RefreshStats) -> rusqlite::Result<()> {
    {
        let mut chunk: Vec<Work> = Vec::new();
        let mut chunk_bytes = 0;
        for item in work {
            chunk_bytes += item.bytes();
            chunk.push(item);
            if chunk_bytes >= CHUNK_BYTES {
                apply(conn, state, std::mem::take(&mut chunk), pricing, source, stats)?;
                chunk_bytes = 0;
            }
        }
        if !chunk.is_empty() {
            apply(conn, state, chunk, pricing, source, stats)?;
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Opening
// --------------------------------------------------------------------------

/// Open (creating) the database at `path`. One of an older schema is emptied
/// and recreated in place. One that cannot be used -- corrupt, not a
/// database, or of a newer schema this build does not know -- is set aside
/// rather than trusted: a corrupt one is deleted and recreated, a newer one is
/// left for the build that owns it and this process runs on an in-memory
/// store. Never fails: the last resort is in memory.
fn open(path: &Path) -> Connection {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    match try_open(path) {
        Ok(conn) => return conn,
        Err(OpenError::Newer) => {}
        Err(OpenError::Unusable) => {
            for suffix in ["", "-wal", "-shm"] {
                std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
            }
            if let Ok(conn) = try_open(path) {
                return conn;
            }
        }
    }
    #[expect(clippy::expect_used, reason = "an in-memory database always opens; there is no further fallback")]
    let conn = Connection::open_in_memory().expect("an in-memory database always opens");
    // Best effort: a store whose schema cannot be created reads as empty.
    create_schema(&conn).ok();
    conn
}

enum OpenError {
    Newer,
    Unusable,
}

fn try_open(path: &Path) -> Result<Connection, OpenError> {
    let conn = Connection::open(path).map_err(|_error| OpenError::Unusable)?;
    // A second tray process may hold the write lock for a moment.
    conn.busy_timeout(Duration::from_secs(10)).map_err(|_error| OpenError::Unusable)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(|_error| OpenError::Unusable)?;
    if version > STORE_SCHEMA_VERSION {
        return Err(OpenError::Newer);
    }
    conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0)).map_err(|_error| OpenError::Unusable)?;
    // With WAL, NORMAL can lose the last commits on power loss but never
    // corrupts; a lost commit is re-read from the transcripts next time.
    // SQLite leaves a WAL file at the largest size it ever reached; cap what
    // stays on disk after a checkpoint.
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA journal_size_limit = 8388608;").map_err(|_error| OpenError::Unusable)?;
    replace_older_schema(&conn).map_err(|_error| OpenError::Unusable)?;
    create_schema(&conn).map_err(|_error| OpenError::Unusable)?;
    Ok(conn)
}

/// A database an older build wrote is emptied and given this build's schema,
/// in one transaction: it is rebuilt from the transcripts like a new one. In
/// place, rather than deleting its files, so an older tray that still has it
/// open is not left writing to files no one else can see.
fn replace_older_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 || version >= STORE_SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let replaced = (|| {
        // Again under the write lock: another process may have done it.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 || version >= STORE_SCHEMA_VERSION {
            return Ok(());
        }
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for table in tables {
            conn.execute_batch(&format!("DROP TABLE \"{}\";", table.replace('"', "\"\"")))?;
        }
        conn.execute_batch(&format!("{SCHEMA} PRAGMA user_version = {STORE_SCHEMA_VERSION};"))
    })();
    match replaced {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(error) => {
            conn.execute_batch("ROLLBACK").ok();
            Err(error)
        }
    }
}

/// Create the schema in a new, empty database; one already at this version is
/// left as it is.
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        conn.execute_batch(&format!("BEGIN IMMEDIATE; {SCHEMA} PRAGMA user_version = {STORE_SCHEMA_VERSION}; COMMIT;"))?;
    }
    Ok(())
}

fn is_empty(conn: &Connection, source: Source) -> rusqlite::Result<bool> {
    conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM files WHERE source = ?1)", [source_code(source)], |row| row.get(0))
}

/// How a [`Source`] is stored in `files.source`.
fn source_code(source: Source) -> i64 {
    match source {
        Source::Claude => 0,
        Source::Codex => 1,
    }
}

/// The `meta` key a source's pricing fingerprint is kept under. Claude Code's
/// is the one this store has always had.
fn pricing_key(source: Source) -> &'static str {
    match source {
        Source::Claude => "pricing",
        Source::Codex => "pricing:codex",
    }
}

fn data_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA data_version", [], |row| row.get(0))
}

/// Discard `source`'s files, rows and rollups; the other source's are kept.
fn wipe(conn: &mut Connection, source: Source) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let code = source_code(source);
    tx.execute("DELETE FROM rows WHERE source = ?1", [code])?;
    tx.execute("DELETE FROM buckets WHERE group_id IN (SELECT group_id FROM files WHERE source = ?1)", [code])?;
    tx.execute("DELETE FROM files WHERE source = ?1", [code])?;
    tx.execute("DELETE FROM groups WHERE id NOT IN (SELECT group_id FROM files)", [])?;
    tx.execute("DELETE FROM meta WHERE key = ?1", [pricing_key(source)])?;
    tx.commit()
}

/// Everything held in memory, read back from the database.
/// Everything `source`'s store holds in memory. The string table is shared.
fn load_state(conn: &Connection, source: Source) -> rusqlite::Result<State> {
    let mut state = State::default();
    let mut statement = conn.prepare("SELECT id, session_id FROM groups")?;
    for row in statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))? {
        let (id, name) = row?;
        state.groups.insert(name.clone(), id);
        state.group_names.insert(id, name);
    }
    let mut statement = conn.prepare("SELECT id, value FROM strings")?;
    for row in statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))? {
        let (id, value) = row?;
        state.strings.insert(value.clone(), id);
        state.string_values.insert(id, value);
    }
    let code = source_code(source);
    let mut statement = conn.prepare("SELECT id, group_id, path, size, mtime_ms, inode, parsed_offset, gone FROM files WHERE source = ?1")?;
    for row in statement.query_map([code], |row| {
        Ok((
            row.get::<_, String>(2)?,
            FileRow {
                id: row.get(0)?,
                group: row.get(1)?,
                size: row.get::<_, i64>(3)?.cast_unsigned(),
                mtime_ms: row.get(4)?,
                inode: row.get::<_, i64>(5)?.cast_unsigned(),
                parsed_offset: row.get::<_, i64>(6)?.cast_unsigned(),
                gone: row.get(7)?,
            },
        ))
    })? {
        let (path, file) = row?;
        state.files.insert(path, file);
    }
    let mut statement = conn.prepare(
        "SELECT group_id, day, model, input, output, cc_5m, cc_1h, cache_read, cost, messages, unpriced, first_ts, last_ts, project FROM buckets
         WHERE group_id IN (SELECT group_id FROM files WHERE source = ?1)",
    )?;
    for row in statement.query_map([code], |row| {
        Ok((BucketKey { group: row.get(0)?, day: row.get(1)?, model: row.get(2)? }, bucket_from_row(row, 3)?))
    })? {
        let (key, bucket) = row?;
        state.buckets.insert(key, bucket);
    }
    state.tail = load_tail(conn, source)?;
    Ok(state)
}

/// A [`Bucket`] from the columns `input .. project` starting at `at`.
fn bucket_from_row(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Bucket> {
    let count = |i: usize| -> rusqlite::Result<u64> { Ok(row.get::<_, i64>(at + i)?.cast_unsigned()) };
    Ok(Bucket {
        aggregate: ModelAggregate {
            input_tokens: count(0)?,
            output_tokens: count(1)?,
            cache_creation_5m_tokens: count(2)?,
            cache_creation_1h_tokens: count(3)?,
            cache_read_tokens: count(4)?,
            cost: row.get(at + 5)?,
            messages: count(6)?,
        },
        unpriced: row.get(at + 7)?,
        first_ts: row.get(at + 8)?,
        last_ts: row.get(at + 9)?,
        project: row.get(at + 10)?,
    })
}

/// A source's (`?2`) counted records since `?1`, oldest first, off
/// `rows_timeline`.
const ENTRIES_SINCE: &str = "SELECT timestamp_ms, cost, input + output + cc_5m + cc_1h + cache_read FROM rows
     WHERE source = ?2 AND counted = 1 AND timestamp_ms >= ?1 ORDER BY timestamp_ms";

/// A source's (`?1`) counted records, newest first, off `rows_timeline`.
const TAIL: &str = "SELECT timestamp_ms, cost, input + output + cc_5m + cc_1h + cache_read FROM rows
     WHERE source = ?1 AND counted = 1 ORDER BY timestamp_ms DESC";

/// Counted records since the last idle gap longer than a 5-hour block, oldest
/// first. The block chain restarts at any such gap, so nothing before it can
/// move where the active block starts.
fn load_tail(conn: &Connection, source: Source) -> rusqlite::Result<Vec<TailEntry>> {
    let mut statement = conn.prepare_cached(TAIL)?;
    let mut rows = statement.query([source_code(source)])?;
    let mut tail = Vec::new();
    let mut previous: Option<i64> = None;
    while let Some(row) = rows.next()? {
        let timestamp_ms: i64 = row.get(0)?;
        if previous.is_some_and(|previous| previous - timestamp_ms > BLOCK_DURATION_MS) {
            break;
        }
        tail.push(TailEntry { timestamp_ms, cost: row.get(1)?, tokens: row.get::<_, i64>(2)?.cast_unsigned() });
        previous = Some(timestamp_ms);
    }
    tail.reverse();
    Ok(tail)
}

// --------------------------------------------------------------------------
// Planning: the stat() sweep
// --------------------------------------------------------------------------

/// The key a transcript is stored under. Paths are ordered by it too, as
/// discovery orders them.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn stat_file(path: &Path) -> Option<FileStat> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| match modified.duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_millis()).ok(),
            Err(before) => i64::try_from(before.duration().as_millis()).ok().map(|ms| -ms),
        })
        .unwrap_or(0);
    Some(FileStat { size: metadata.len(), mtime_ms, inode: metadata.ino() })
}

/// What a watched walk said about the files, for [`plan`].
struct Hint<'a> {
    /// The paths touched since the last refresh.
    changed: &'a HashSet<PathBuf>,
    /// The files the last refresh found work for.
    retry: &'a HashSet<String>,
    now_ms: i64,
}

impl Hint<'_> {
    /// Whether `file`, stored under `key`, can be taken as unchanged without
    /// a `stat`: nothing named it, it did not change in the moments before the
    /// last refresh (the walk's own settle time, for the same reason), and it
    /// was there last time.
    fn is_quiet(&self, path: &Path, key: &str, file: &FileRow) -> bool {
        !file.gone && !self.changed.contains(path) && !self.retry.contains(key) && self.now_ms.saturating_sub(file.mtime_ms) >= SETTLE_MS
    }
}

/// A file modified this recently is `stat`ed whatever the events said.
const SETTLE_MS: i64 = 2_000;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|since| i64::try_from(since.as_millis()).ok()).unwrap_or(0)
}

/// Compare discovery against what is stored. Only `stat()`s; nothing is read.
///
/// A Codex rollout is never read from the middle: a record's model can be
/// named by a line written after it, so any change to one re-reads it whole
/// (see [`crate::codex`]).
fn plan(state: &State, files: &[PathBuf], source: Source, hint: Option<&Hint<'_>>, statted: &mut usize) -> Vec<Work> {
    let mut work = Vec::new();
    let mut live_groups: HashSet<String> = HashSet::new();
    let mut keys: Vec<(String, &PathBuf)> = files.iter().map(|path| (path_key(path), path)).collect();
    keys.sort();
    keys.dedup_by(|a, b| a.0 == b.0);

    for (key, path) in &keys {
        let (group, project) = source.group_of(path);
        // A stored file that nothing touched is as stored: no `stat`. Its
        // group lives, as discovery found it.
        if let Some(hint) = hint
            && state.files.get(key.as_str()).is_some_and(|file| hint.is_quiet(path, key, file))
        {
            live_groups.insert(group);
            continue;
        }
        *statted += 1;
        let Some(stat) = stat_file(path) else { continue };
        live_groups.insert(group.clone());
        let read = |from: u64, replace: bool| Work::Read {
            path: (*path).clone(),
            key: key.clone(),
            group: group.clone(),
            project: project.clone(),
            stat,
            from,
            replace,
        };
        match state.files.get(key.as_str()) {
            None => work.push(read(0, false)),
            Some(file) => {
                // Truncated or replaced: offsets into it mean nothing now. Any
                // shrink counts -- a file can shrink to somewhere between
                // `parsed_offset` and its old size and then the bytes at
                // `parsed_offset` are not a continuation.
                let unchanged = file.size == stat.size && file.mtime_ms == stat.mtime_ms && file.inode == stat.inode;
                // A Codex rollout that changed at all, or any transcript
                // truncated or replaced.
                if (source == Source::Codex && !unchanged) || file.inode != stat.inode || stat.size < file.size || stat.size < file.parsed_offset {
                    work.push(read(0, true));
                } else if file.size == stat.size && file.mtime_ms == stat.mtime_ms {
                    if file.gone {
                        work.push(Work::Touch { key: key.clone(), stat });
                    }
                } else if stat.size > file.parsed_offset {
                    work.push(read(file.parsed_offset, false));
                } else {
                    work.push(Work::Touch { key: key.clone(), stat });
                }
            }
        }
    }

    // A transiently missing Claude directory must not read as "every
    // transcript was deleted".
    if keys.is_empty() {
        return work;
    }
    let mut dropped: HashSet<i64> = HashSet::new();
    for (key, file) in &state.files {
        if keys.binary_search_by(|(k, _)| k.as_str().cmp(key)).is_ok() {
            continue;
        }
        let group_alive = state.group_names.get(&file.group).is_some_and(|name| live_groups.contains(name));
        if group_alive {
            if !file.gone {
                work.push(Work::Gone { key: key.clone() });
            }
        } else if dropped.insert(file.group) {
            work.push(Work::DropGroup { group: file.group });
        }
    }
    work
}

// --------------------------------------------------------------------------
// Applying
// --------------------------------------------------------------------------

/// What a transaction touched, to be settled before it commits.
#[derive(Default)]
struct Touched {
    partitions: HashSet<i64>,
    buckets: HashSet<BucketKey>,
}

fn apply(conn: &mut Connection, state: &mut State, work: Vec<Work>, pricing: &PricingMap, source: Source, stats: &mut RefreshStats) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut insert = tx.prepare_cached(INSERT_ROW)?;
    let mut touched = Touched::default();
    let mut file_updates: Vec<(String, FileRow)> = Vec::new();
    let mut dropped_groups: Vec<i64> = Vec::new();

    for item in work {
        match item {
            Work::Read { path, key, group, project, stat, from, replace } => {
                let group_id = ensure_group(&tx, state, &group)?;
                let file_id = ensure_file(&tx, state, &key, group_id, &project, source)?;
                if replace {
                    stats.rows_removed += remove_rows(&tx, "file_id = ?1", params![file_id], &mut touched)?;
                } else {
                    // Rows at or past `from` were parsed out of an unterminated
                    // last line; it is about to be read again.
                    stats.rows_removed += remove_rows(&tx, "file_id = ?1 AND byte_offset >= ?2", params![file_id, from.cast_signed()], &mut touched)?;
                }
                let read = match source {
                    Source::Claude => read_records(&path, from, stat.size, &group, &project, pricing),
                    Source::Codex => {
                        let rollout = crate::codex::read_rollout(&path, pricing).unwrap_or_default();
                        // The project is known only now; the buckets rebuilt
                        // below read it off the file row.
                        if let Some(cwd) = &rollout.cwd {
                            tx.prepare_cached("UPDATE files SET project = ?2 WHERE id = ?1")?.execute(params![file_id, cwd])?;
                        }
                        // Read whole, every time: its offset is its size.
                        Parsed { records: rollout.records, consumed: stat.size, bytes: stat.size }
                    }
                };
                stats.files_read += 1;
                stats.bytes_read += read.bytes;
                for (offset, record) in &read.records {
                    insert_row(&tx, &mut insert, state, RowAt { file: file_id, group: group_id, source, offset: from + offset }, record, &mut touched)?;
                }
                stats.rows_written += read.records.len();
                let file = FileRow { id: file_id, group: group_id, size: stat.size, mtime_ms: stat.mtime_ms, inode: stat.inode, parsed_offset: from + read.consumed, gone: false };
                write_file(&tx, &file)?;
                file_updates.push((key, file));
            }
            Work::Touch { key, stat } => {
                if let Some(old) = state.files.get(&key) {
                    let file = FileRow { size: stat.size, mtime_ms: stat.mtime_ms, inode: stat.inode, gone: false, ..old.clone() };
                    write_file(&tx, &file)?;
                    file_updates.push((key, file));
                }
            }
            Work::Gone { key } => {
                if let Some(old) = state.files.get(&key) {
                    let file = FileRow { gone: true, ..old.clone() };
                    write_file(&tx, &file)?;
                    file_updates.push((key, file));
                }
            }
            Work::DropGroup { group } => {
                for file in state.files.values().filter(|file| file.group == group) {
                    stats.rows_removed += remove_rows(&tx, "file_id = ?1", params![file.id], &mut touched)?;
                }
                tx.execute("DELETE FROM files WHERE group_id = ?1", [group])?;
                tx.execute("DELETE FROM buckets WHERE group_id = ?1", [group])?;
                tx.execute("DELETE FROM groups WHERE id = ?1", [group])?;
                dropped_groups.push(group);
            }
        }
    }

    drop(insert);
    for partition in std::mem::take(&mut touched.partitions) {
        settle_partition(&tx, partition, &mut touched.buckets)?;
        stats.partitions += 1;
    }
    let mut rebuilt: Vec<(BucketKey, Option<Bucket>)> = Vec::new();
    for key in touched.buckets {
        let bucket = rebuild_bucket(&tx, &key)?;
        rebuilt.push((key, bucket));
        stats.buckets += 1;
    }
    tx.commit()?;
    stats.transactions += 1;

    // Only now that it is committed does what is held follow.
    for (key, file) in file_updates {
        state.files.insert(key, file);
    }
    for group in dropped_groups {
        state.files.retain(|_, file| file.group != group);
        state.buckets.retain(|key, _| key.group != group);
        if let Some(name) = state.group_names.remove(&group) {
            state.groups.remove(&name);
        }
    }
    for (key, bucket) in rebuilt {
        match bucket {
            Some(bucket) => state.buckets.insert(key, bucket),
            None => state.buckets.remove(&key),
        };
    }
    Ok(())
}

fn ensure_group(tx: &Transaction<'_>, state: &mut State, session_id: &str) -> rusqlite::Result<i64> {
    if let Some(&id) = state.groups.get(session_id) {
        return Ok(id);
    }
    let id: i64 = tx.query_row(
        "INSERT INTO groups (session_id) VALUES (?1) ON CONFLICT (session_id) DO UPDATE SET session_id = excluded.session_id RETURNING id",
        [session_id],
        |row| row.get(0),
    )?;
    state.groups.insert(session_id.to_owned(), id);
    state.group_names.insert(id, session_id.to_owned());
    Ok(id)
}

fn ensure_string(tx: &Transaction<'_>, state: &mut State, value: &str) -> rusqlite::Result<i64> {
    if let Some(&id) = state.strings.get(value) {
        return Ok(id);
    }
    let id: i64 = tx.query_row(
        "INSERT INTO strings (value) VALUES (?1) ON CONFLICT (value) DO UPDATE SET value = excluded.value RETURNING id",
        [value],
        |row| row.get(0),
    )?;
    state.strings.insert(value.to_owned(), id);
    state.string_values.insert(id, value.to_owned());
    Ok(id)
}

/// The file's id, creating its row if this is the first sighting. Its sizes
/// and offset are written once the read is done.
fn ensure_file(tx: &Transaction<'_>, state: &State, key: &str, group: i64, project: &str, source: Source) -> rusqlite::Result<i64> {
    if let Some(file) = state.files.get(key) {
        return Ok(file.id);
    }
    tx.query_row(
        "INSERT INTO files (group_id, path, project, size, mtime_ms, inode, parsed_offset, source) VALUES (?1, ?2, ?3, 0, 0, 0, 0, ?4)
         ON CONFLICT (path) DO UPDATE SET group_id = excluded.group_id RETURNING id",
        params![group, key, project, source_code(source)],
        |row| row.get(0),
    )
}

fn write_file(tx: &Transaction<'_>, file: &FileRow) -> rusqlite::Result<()> {
    tx.prepare_cached("UPDATE files SET size = ?2, mtime_ms = ?3, inode = ?4, parsed_offset = ?5, gone = ?6 WHERE id = ?1")?.execute(params![
        file.id,
        file.size.cast_signed(),
        file.mtime_ms,
        file.inode.cast_signed(),
        file.parsed_offset.cast_signed(),
        file.gone
    ])?;
    Ok(())
}

/// Delete the rows matching `filter`, noting the partitions they leave to be
/// re-settled and the buckets they leave to be rebuilt.
fn remove_rows(tx: &Transaction<'_>, filter: &str, args: impl rusqlite::Params + Copy, touched: &mut Touched) -> rusqlite::Result<usize> {
    let mut statement = tx.prepare_cached(&format!("SELECT partition_hash, group_id, day, model, counted FROM rows WHERE {filter}"))?;
    let mut rows = statement.query(args)?;
    while let Some(row) = rows.next()? {
        if let Some(partition) = row.get::<_, Option<i64>>(0)? {
            touched.partitions.insert(partition);
        }
        if row.get::<_, bool>(4)? {
            touched.buckets.insert(BucketKey { group: row.get(1)?, day: row.get(2)?, model: row.get(3)? });
        }
    }
    tx.prepare_cached(&format!("DELETE FROM rows WHERE {filter}"))?.execute(args)
}

/// The partition a record dedupes within, or `None` when it has no message id
/// and is never deduped.
fn partition_of(session_id: &str, message_id: Option<&str>) -> Option<i64> {
    message_id.map(|message_id| crate::stable_hash(&[session_id.as_bytes(), message_id.as_bytes()]).cast_signed())
}

fn speed_code(speed: Option<Speed>) -> Option<i64> {
    speed.map(|speed| match speed {
        Speed::Standard => 0,
        Speed::Fast => 1,
    })
}

fn speed_of(code: Option<i64>) -> Option<Speed> {
    code.map(|code| if code == 1 { Speed::Fast } else { Speed::Standard })
}

/// Where a new row goes: its file, the file's group and source, and its byte
/// offset in the file.
#[derive(Clone, Copy)]
struct RowAt {
    file: i64,
    group: i64,
    source: Source,
    offset: u64,
}

/// Every row's insert, prepared once a transaction (see [`insert_row`]).
const INSERT_ROW: &str = "INSERT INTO rows (file_id, group_id, byte_offset, partition_hash, timestamp_ms, day, session, model, message_id, request_id,
                   input, output, cc_5m, cc_1h, cache_read, is_sidechain, speed, cost, missing_pricing, counted, source)
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)";

/// Store one record, by `insert` (prepared from [`INSERT_ROW`]): held for
/// the transaction rather than looked up in the statement cache per row,
/// which hashes the whole SQL text each time.
fn insert_row(tx: &Transaction<'_>, insert: &mut rusqlite::Statement<'_>, state: &mut State, at: RowAt, record: &UsageRecord, touched: &mut Touched) -> rusqlite::Result<()> {
    let RowAt { file, group, source, offset } = at;
    let session = ensure_string(tx, state, &record.session_id)?;
    let model = match &record.model {
        Some(model) => ensure_string(tx, state, model)?,
        None => NO_MODEL,
    };
    let partition = partition_of(&record.session_id, record.message_id.as_deref());
    // Never deduped, so it counts from the start; everything else is settled
    // with its partition before the transaction commits.
    let counted = partition.is_none();
    insert.execute(params![
        file,
        group,
        offset.cast_signed(),
        partition,
        record.timestamp_ms,
        record.date,
        session,
        model,
        record.message_id,
        record.request_id,
        record.input_tokens.cast_signed(),
        record.output_tokens.cast_signed(),
        record.cache_creation_5m_tokens.cast_signed(),
        record.cache_creation_1h_tokens.cast_signed(),
        record.cache_read_tokens.cast_signed(),
        record.is_sidechain,
        speed_code(record.speed),
        record.cost,
        record.missing_pricing,
        counted,
        source_code(source),
    ])?;
    match partition {
        Some(partition) => {
            touched.partitions.insert(partition);
        }
        None => {
            touched.buckets.insert(BucketKey { group, day: record.date.clone(), model });
        }
    }
    Ok(())
}

/// Re-run dedupe over one partition and set `counted` to its answer.
/// One dedupe partition's rows, for [`settle_partition`]. `+?1`, not `?1`:
/// checking a join against the partial indexes, SQLite compares a bare
/// parameter's value with their `counted = 1`, and a statement planned on a
/// value is planned again whenever the statement cache clears its bindings
/// -- once per partition, most of a cold build. `+` keeps the parameter out
/// of that comparison and does not stop `rows_partition` being used.
const SETTLE_MEMBERS: &str = "SELECT r.id, r.counted, f.path, r.byte_offset, r.group_id, r.day, r.model, r.timestamp_ms, s.value, r.message_id, r.request_id,
        r.input, r.output, r.cc_5m, r.cc_1h, r.cache_read, r.is_sidechain, r.speed
 FROM rows r JOIN files f ON f.id = r.file_id JOIN strings s ON s.id = r.session
 WHERE r.partition_hash = +?1";

fn settle_partition(tx: &Transaction<'_>, partition: i64, buckets: &mut HashSet<BucketKey>) -> rusqlite::Result<()> {
    struct Member {
        id: i64,
        counted: bool,
        path: String,
        offset: i64,
        bucket: BucketKey,
        record: UsageRecord,
    }
    let mut statement = tx.prepare_cached(SETTLE_MEMBERS)?;
    let mut members: Vec<Member> = statement
        .query_map([partition], |row| {
            Ok(Member {
                id: row.get(0)?,
                counted: row.get(1)?,
                path: row.get(2)?,
                offset: row.get(3)?,
                bucket: BucketKey { group: row.get(4)?, day: row.get(5)?, model: row.get(6)? },
                record: UsageRecord {
                    timestamp_ms: row.get(7)?,
                    date: String::new(),
                    session_id: row.get(8)?,
                    project_path: String::new(),
                    input_tokens: row.get::<_, i64>(11)?.cast_unsigned(),
                    output_tokens: row.get::<_, i64>(12)?.cast_unsigned(),
                    cache_creation_5m_tokens: row.get::<_, i64>(13)?.cast_unsigned(),
                    cache_creation_1h_tokens: row.get::<_, i64>(14)?.cast_unsigned(),
                    cache_read_tokens: row.get::<_, i64>(15)?.cast_unsigned(),
                    model: None,
                    message_id: row.get(9)?,
                    request_id: row.get(10)?,
                    is_sidechain: row.get(16)?,
                    speed: speed_of(row.get(17)?),
                    cost: 0.0,
                    missing_pricing: false,
                },
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // The order a single pass over the corpus would see them in. Two
    // partitions sharing a hash are simply settled together: dedupe compares
    // the real ids, so they cannot affect each other.
    members.sort_by(|a, b| (&a.path, a.offset).cmp(&(&b.path, b.offset)));
    let records: Vec<UsageRecord> = members.iter().map(|member| member.record.clone()).collect();
    let keep: HashSet<usize> = dedupe_indices_to_fixpoint(&records).into_iter().collect();

    let mut update = tx.prepare_cached("UPDATE rows SET counted = ?2 WHERE id = ?1")?;
    for (index, member) in members.into_iter().enumerate() {
        let counted = keep.contains(&index);
        if counted != member.counted {
            update.execute(params![member.id, counted])?;
            buckets.insert(member.bucket);
        }
    }
    Ok(())
}

/// Recompute one bucket from its counted rows; `None` (and no row) when none
/// are left.
fn rebuild_bucket(tx: &Transaction<'_>, key: &BucketKey) -> rusqlite::Result<Option<Bucket>> {
    let args = params![key.group, key.day, key.model];
    let bucket = tx
        .prepare_cached(
            "SELECT TOTAL(input), TOTAL(output), TOTAL(cc_5m), TOTAL(cc_1h), TOTAL(cache_read), TOTAL(cost), COUNT(*),
                    COALESCE(MAX(missing_pricing), 0), MIN(timestamp_ms), MAX(timestamp_ms),
                    (SELECT f.project FROM rows r JOIN files f ON f.id = r.file_id
                     WHERE r.group_id = ?1 AND r.day = ?2 AND r.model = ?3 AND r.counted = 1
                     ORDER BY r.timestamp_ms, f.path, r.byte_offset LIMIT 1)
             FROM rows WHERE group_id = ?1 AND day = ?2 AND model = ?3 AND counted = 1",
        )?
        .query_row(args, |row| {
            let messages: i64 = row.get(6)?;
            if messages == 0 {
                return Ok(None);
            }
            // TOTAL() is a float; token sums are exact in a double far past
            // anything a corpus reaches (2^53).
            let count = |i: usize| -> rusqlite::Result<u64> { Ok(sl_core::num::trunc_u64(row.get::<_, f64>(i)?)) };
            Ok(Some(Bucket {
                aggregate: ModelAggregate {
                    input_tokens: count(0)?,
                    output_tokens: count(1)?,
                    cache_creation_5m_tokens: count(2)?,
                    cache_creation_1h_tokens: count(3)?,
                    cache_read_tokens: count(4)?,
                    cost: row.get(5)?,
                    messages: messages.cast_unsigned(),
                },
                unpriced: row.get(7)?,
                first_ts: row.get(8)?,
                last_ts: row.get(9)?,
                project: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
            }))
        })?;
    match &bucket {
        Some(bucket) => {
            let a = &bucket.aggregate;
            tx.prepare_cached(
                "INSERT OR REPLACE INTO buckets (group_id, day, model, input, output, cc_5m, cc_1h, cache_read, cost, messages, unpriced, first_ts, last_ts, project)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?
            .execute(params![
                key.group,
                key.day,
                key.model,
                a.input_tokens.cast_signed(),
                a.output_tokens.cast_signed(),
                a.cache_creation_5m_tokens.cast_signed(),
                a.cache_creation_1h_tokens.cast_signed(),
                a.cache_read_tokens.cast_signed(),
                a.cost,
                a.messages.cast_signed(),
                bucket.unpriced,
                bucket.first_ts,
                bucket.last_ts,
                bucket.project,
            ])?;
        }
        None => {
            tx.prepare_cached("DELETE FROM buckets WHERE group_id = ?1 AND day = ?2 AND model = ?3")?.execute(args)?;
        }
    }
    Ok(bucket)
}

/// Recompute every stored cost under `pricing`, then every bucket. Every
/// input of a cost is a column, so a new price list needs no transcript read.
fn reprice(tx: &Transaction<'_>, state: &mut State, pricing: &PricingMap, source: Source, stats: &mut RefreshStats) -> rusqlite::Result<()> {
    {
        let mut select = tx.prepare(
            "SELECT id, model, input, output, cc_5m, cc_1h, cache_read, speed, timestamp_ms FROM rows WHERE source = ?1",
        )?;
        let mut update = tx.prepare("UPDATE rows SET cost = ?2, missing_pricing = ?3 WHERE id = ?1")?;
        let mut rows = select.query([source_code(source)])?;
        while let Some(row) = rows.next()? {
            let model: i64 = row.get(1)?;
            let tokens = UsageTokens {
                input: row.get::<_, i64>(2)?.cast_unsigned(),
                output: row.get::<_, i64>(3)?.cast_unsigned(),
                cache_create_5m: row.get::<_, i64>(4)?.cast_unsigned(),
                cache_create_1h: row.get::<_, i64>(5)?.cast_unsigned(),
                cache_read: row.get::<_, i64>(6)?.cast_unsigned(),
            };
            let (cost, missing) = match source {
                Source::Claude => cost_for_usage(pricing, state.model_name(model), &tokens),
                Source::Codex => crate::codex::codex_cost(pricing, state.model_name(model), &tokens, speed_of(row.get(7)?), row.get(8)?),
            };
            update.execute(params![row.get::<_, i64>(0)?, cost, missing])?;
        }
    }
    let mut keys: Vec<BucketKey> = Vec::new();
    {
        let mut statement =
            tx.prepare("SELECT DISTINCT group_id, day, model FROM rows WHERE counted = 1 AND source = ?1")?;
        for key in statement.query_map([source_code(source)], |row| Ok(BucketKey { group: row.get(0)?, day: row.get(1)?, model: row.get(2)? }))? {
            keys.push(key?);
        }
    }
    tx.execute("DELETE FROM buckets WHERE group_id IN (SELECT group_id FROM files WHERE source = ?1)", [source_code(source)])?;
    state.buckets.clear();
    for key in keys {
        if let Some(bucket) = rebuild_bucket(tx, &key)? {
            state.buckets.insert(key, bucket);
        }
        stats.buckets += 1;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Reading transcripts
// --------------------------------------------------------------------------

struct Parsed {
    /// Records with their line offsets relative to `from`.
    records: Vec<(u64, UsageRecord)>,
    /// Bytes from `from` up to and including the last newline.
    consumed: u64,
    bytes: u64,
}

/// Parse `[from, size)` of `path`, a block at a time.
///
/// Bounded by `size` (what `stat` reported this cycle) rather than reading to
/// EOF, so a file that grows between the stat and the read cannot push the
/// recorded offset past the recorded size. A last line with no newline is
/// parsed too -- a record whose JSON is complete counts now -- but is not
/// counted as consumed, so it is read again (and its row replaced) once the
/// newline arrives.
fn read_records(path: &Path, from: u64, size: u64, fallback_session_id: &str, project: &str, pricing: &PricingMap) -> Parsed {
    let mut out = Parsed { records: Vec::new(), consumed: 0, bytes: 0 };
    let Ok(mut file) = std::fs::File::open(path) else { return out };
    if file.seek(SeekFrom::Start(from)).is_err() {
        return out;
    }
    let mut remaining = size.saturating_sub(from);
    let mut buffer: Vec<u8> = Vec::new();
    // Offset of `buffer[0]` relative to `from`.
    let mut base: u64 = 0;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(READ_BLOCK as u64)).unwrap_or(READ_BLOCK);
        let start = buffer.len();
        buffer.resize(start + want, 0);
        let got = match file.read(&mut buffer[start..]) {
            Ok(0) | Err(_) => {
                buffer.truncate(start);
                break;
            }
            Ok(got) => got,
        };
        buffer.truncate(start + got);
        remaining -= got as u64;
        out.bytes += got as u64;
        if let Some(last_newline) = memrchr(b'\n', &buffer) {
            let complete = last_newline + 1;
            for (offset, record) in parse_lines(&buffer[..complete], fallback_session_id, project, pricing) {
                out.records.push((base + offset, record));
            }
            base += complete as u64;
            out.consumed = base;
            buffer.drain(..complete);
        }
    }
    if !buffer.is_empty() {
        for (offset, record) in parse_lines(&buffer, fallback_session_id, project, pricing) {
            out.records.push((base + offset, record));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The query run once per partition is compiled once, through the
    /// statement cache as `settle_partition` runs it, and still finds a
    /// partition off its index.
    #[test]
    fn settling_partitions_does_not_recompile_the_query() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        for partition in 1..=20_i64 {
            conn.prepare_cached(SETTLE_MEMBERS).unwrap().query([partition]).unwrap().next().unwrap();
        }
        assert_eq!(conn.prepare_cached(SETTLE_MEMBERS).unwrap().get_status(rusqlite::StatementStatus::RePrepare), 0, "recompiled for new hashes");
        let plan = query_plan(&conn, SETTLE_MEMBERS);
        assert!(plan.iter().any(|step| step.contains("USING INDEX rows_partition (partition_hash=?)")), "{plan:?}");
    }

    /// The plan SQLite picks for `sql`, one detail line per step.
    fn query_plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let params = vec![0_i64; statement.parameter_count()];
        statement.query_map(rusqlite::params_from_iter(params), |row| row.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    /// The two reads that run on every refresh walk a source's counted rows in
    /// time order off `rows_timeline`: no scan of the source's files, no sort.
    /// On a store of ~900k rows the old plans (a per-file scan and a temp
    /// b-tree) took about 200 ms and 80 ms; this one about 5 ms.
    #[test]
    fn a_sources_counted_rows_are_read_in_order_off_the_timeline_index() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        let check = |when: &str| {
            for sql in [TAIL, ENTRIES_SINCE] {
                let plan = query_plan(&conn, sql);
                // Searched by source, not scanned past the other source's rows.
                assert!(plan.iter().any(|step| step.starts_with("SEARCH rows USING INDEX rows_timeline (source=?")), "{when}: {sql}: {plan:?}");
                assert!(!plan.iter().any(|step| step.contains("TEMP B-TREE")), "{when}: {sql}: no sort step: {plan:?}");
                assert!(!plan.iter().any(|step| step.contains("files")), "{when}: {sql}: files are not read: {plan:?}");
            }
        };
        check("an empty store");
        // Statistics shaped like the live store's (918k rows, 441k counted),
        // so the planner weighs it as it would that one.
        conn.execute_batch(
            "ANALYZE;
             DELETE FROM sqlite_stat1;
             INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
               ('rows', 'rows_timeline', '441000 220000 1'),
               ('rows', 'rows_bucket', '441000 50 5 2'),
               ('rows', 'rows_partition', '900000 2'),
               ('rows', 'sqlite_autoindex_rows_1', '918000 400 1'),
               ('files', 'files_source', '2000 1000'),
               ('files', 'sqlite_autoindex_files_1', '2000 1');
             ANALYZE sqlite_schema;",
        )
        .unwrap();
        check("a store the size of the live one");
    }

    /// Without a hint every stored file is `stat`ed; with one, only what it
    /// names, what is new, what changed moments ago and what was retried.
    #[test]
    fn a_hint_limits_the_stat_sweep_to_what_it_names() {
        let guard = tempfile::tempdir().unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        let mut state = State::default();
        let mut paths = Vec::new();
        for name in ["a", "b", "c"] {
            let path = guard.path().join(format!("{name}.jsonl"));
            std::fs::write(&path, "0123456789").unwrap();
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
            let stat = stat_file(&path).unwrap();
            let row = FileRow { id: 1, group: 1, size: stat.size, mtime_ms: stat.mtime_ms, inode: stat.inode, parsed_offset: stat.size, gone: false };
            state.files.insert(path_key(&path), row);
            paths.push(path);
        }
        // `a` grows; `b` grows too, but nothing says so.
        for path in &paths[..2] {
            std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(b"more\n").unwrap();
        }
        let key = |path: &Path| path_key(path);
        let reads = |work: &[Work]| -> Vec<String> { work.iter().filter_map(Work::key).map(str::to_owned).collect() };
        let (changed, retry) = (HashSet::from([paths[0].clone()]), HashSet::new());

        let mut statted = 0;
        let work = plan(&state, &paths, Source::Claude, None, &mut statted);
        assert_eq!((statted, reads(&work)), (3, vec![key(&paths[0]), key(&paths[1])]), "no hint: all stat'ed, both growths found");

        let mut statted = 0;
        let work = plan(&state, &paths, Source::Claude, Some(&Hint { changed: &changed, retry: &retry, now_ms: now_ms() }), &mut statted);
        assert_eq!((statted, reads(&work)), (1, vec![key(&paths[0])]), "a hint: only the named file");

        let retry = HashSet::from([key(&paths[1])]);
        let mut statted = 0;
        let work = plan(&state, &paths, Source::Claude, Some(&Hint { changed: &changed, retry: &retry, now_ms: now_ms() }), &mut statted);
        assert_eq!((statted, reads(&work)), (2, vec![key(&paths[0]), key(&paths[1])]), "a retried file is looked at again");

        // A file new to the store is always looked at; so is one modified moments ago.
        let new = guard.path().join("d.jsonl");
        std::fs::write(&new, "x").unwrap();
        let mut all = paths.clone();
        all.push(new.clone());
        state.files.get_mut(&key(&paths[2])).unwrap().mtime_ms = now_ms();
        let mut statted = 0;
        let work = plan(&state, &all, Source::Claude, Some(&Hint { changed: &changed, retry: &HashSet::new(), now_ms: now_ms() }), &mut statted);
        assert_eq!(statted, 3, "a, the recent c, the new d");
        assert!(reads(&work).contains(&key(&new)));
    }

    #[test]
    fn a_new_database_is_created_at_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, STORE_SCHEMA_VERSION, "created at this build's version");
        create_schema(&conn).unwrap();
    }
}
