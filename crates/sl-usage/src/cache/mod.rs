//! Usage over the JSONL corpus: a persistent, incremental store, and an
//! in-memory oracle to check it against.
//!
//! The corpus is large (thousands of transcripts, gigabytes, hundreds of
//! thousands of usage records) but barely changes between refreshes: a few
//! transcripts grow by a few lines. [`UsageTotals`] keeps every parsed usage
//! line in SQLite (see [`store`] for the schema and how dedupe is settled) and
//! answers from rollups, so a refresh costs a `stat()` sweep (nothing, once
//! file-system events say what changed: see [`crate::watch`]) plus the bytes
//! that were actually appended.
//!
//! [`UsageCache`] is the other way to get the same answer: parse every file,
//! dedupe everything in one pass, hold every record. Nothing persistent, no
//! cleverness. It is what the store's tests measure against, and what the
//! record-level views in [`crate::aggregate`] work over.
//!
//! ## Session groups
//!
//! Dedupe is keyed on `(message.id, requestId, effective session id)`, so two
//! records in different sessions can never be duplicates. A **session group**
//! is every file sharing a *path-derived* session id: a session's
//! `<sessionId>.jsonl` and its `<sessionId>/subagents/*.jsonl` replays. Groups
//! are what per-session views are folded by.
//!
//! The *effective* session id (a line's own `sessionId`, falling back to the
//! path) can differ from the path-derived one: Claude Code writes lines carrying
//! a previous session's id into a new transcript when a session is resumed. So
//! the same billed message can sit in two groups. Both the store and the oracle
//! dedupe across the whole corpus, not per group, so it still counts once; the
//! surviving copy is the one the first file in path order holds.
//!
//! ## On disk
//!
//! ```text
//! $XDG_CACHE_HOME/statusline/usage.sqlite   (else ~/.cache/statusline/...)
//!                            usage.sqlite-wal, usage.sqlite-shm
//! ```

mod aggregate;
mod store;
mod totals;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pricing::PricingMap;
use crate::record::UsageRecord;

pub use aggregate::{ModelAggregate, UNKNOWN_MODEL, daily_model_aggregates};
pub use store::{BlockEntry, DB_FILE, RefreshStats, STORE_SCHEMA_VERSION};
pub use totals::{ActiveBlock, ModelTotals, TokenTotals, UsageTotals, active_block_of};

/// One session group's deduped records, as the oracle holds them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionGroupCache {
    pub session_id: String,
    /// Deduped records for the whole group, sorted by `timestamp_ms`.
    pub records: Vec<UsageRecord>,
    /// `min(records.timestamp_ms)`, or 0 when the group has no records.
    pub first_seen: i64,
    /// `max(records.timestamp_ms)`, or 0 when the group has no records.
    pub last_activity: i64,
}

impl SessionGroupCache {
    /// A group of `records`, sorted, with its timestamps filled in.
    pub fn new(session_id: &str, mut records: Vec<UsageRecord>) -> Self {
        records.sort_by_key(|record| record.timestamp_ms);
        let first_seen = records.first().map_or(0, |record| record.timestamp_ms);
        let last_activity = records.iter().map(|record| record.timestamp_ms).max().unwrap_or(0);
        Self { session_id: session_id.to_owned(), records, first_seen, last_activity }
    }

    /// Sum of every record's cost, in USD.
    pub fn total_cost(&self) -> f64 {
        self.records.iter().map(|record| record.cost).sum()
    }
}

/// Whose usage a store holds. Both are kept in the one `usage.sqlite`, each
/// file marked with its source: a store reads, reprices, rebuilds and guards
/// against an empty discovery only its own, and their dedupe partitions (a
/// session id with a message id) never meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Source {
    /// Claude Code's transcripts.
    #[default]
    Claude,
    /// OpenAI Codex's rollouts; see [`crate::codex`].
    Codex,
}

impl Source {
    /// Every file of this source on disk; none when it has no data directory.
    pub fn discover(self) -> Vec<PathBuf> {
        self.discover_with(&mut self.walk())
    }

    /// A [`Walk`](crate::paths::Walk) for this source's files, to keep
    /// between [`discover_with`](Self::discover_with) calls.
    pub fn walk(self) -> crate::paths::Walk {
        match self {
            Source::Claude => crate::paths::Walk::new(crate::paths::is_transcript),
            Source::Codex => crate::paths::Walk::new(crate::codex::is_rollout),
        }
    }

    /// [`walk`](Self::walk), listening for file-system events: what a
    /// long-lived process keeps. See [`crate::watch`].
    pub fn watched_walk(self) -> crate::paths::Walk {
        match self {
            Source::Claude => crate::paths::Walk::watched(crate::paths::is_transcript),
            Source::Codex => crate::paths::Walk::watched(crate::codex::is_rollout),
        }
    }

    /// [`discover`](Self::discover), re-reading only the directories that
    /// changed since `walk` last looked.
    pub fn discover_with(self, walk: &mut crate::paths::Walk) -> Vec<PathBuf> {
        match self {
            Source::Claude => crate::paths::claude_paths().map(|paths| walk.files(&crate::paths::transcript_roots(&paths))).unwrap_or_default(),
            Source::Codex => crate::codex::codex_home().map(|home| crate::codex::codex_files_with(&home, walk)).unwrap_or_default(),
        }
    }

    /// The session group and project a file belongs to, as known before it is
    /// read. A Codex rollout's project is its `session_meta.cwd`, filled in
    /// once it is read.
    pub(crate) fn group_of(self, path: &Path) -> (String, String) {
        match self {
            Source::Claude => crate::paths::extract_session_parts(path),
            Source::Codex => (crate::codex::thread_of(path).unwrap_or_else(|| "unknown".to_owned()), "unknown".to_owned()),
        }
    }
}

/// Whether a refresh builds on what is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Read only what changed since the last refresh.
    Incremental,
    /// `--rebuild`: discard everything stored and re-read every transcript.
    Rebuild,
}

/// Every session group's records, parsed from scratch: the oracle. See the
/// module docs.
#[derive(Debug, Clone, Default)]
pub struct UsageCache {
    /// Keyed by session id. A `BTreeMap` so iteration order is deterministic.
    groups: BTreeMap<String, SessionGroupCache>,
}

impl UsageCache {
    /// Parse every transcript discovery finds. No Claude data directory at all
    /// yields an empty cache, not an error.
    pub fn load(pricing: &PricingMap) -> Self {
        Self::from_files(&Source::Claude.discover(), pricing)
    }

    /// Parse `files` and dedupe them together, in path order -- one
    /// [`crate::dedupe_indices_to_fixpoint`] over the whole corpus, which is
    /// what ccusage does and what the store settles to. Every group with a
    /// file gets an entry, even one with no records.
    pub fn from_files(files: &[PathBuf], pricing: &PricingMap) -> Self {
        let mut ordered: Vec<&PathBuf> = files.iter().collect();
        ordered.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
        ordered.dedup();

        let mut groups: BTreeMap<String, Vec<UsageRecord>> = BTreeMap::new();
        let mut records: Vec<UsageRecord> = Vec::new();
        let mut origin: Vec<String> = Vec::new();
        for path in ordered {
            let (session_id, _project) = crate::paths::extract_session_parts(path);
            for record in crate::record::parse_file(path, pricing) {
                records.push(record);
                origin.push(session_id.clone());
            }
            groups.entry(session_id).or_default();
        }

        let keep = crate::dedupe::dedupe_indices_to_fixpoint(&records);
        let mut slots: Vec<Option<UsageRecord>> = records.into_iter().map(Some).collect();
        for index in keep {
            if let Some(record) = slots[index].take() {
                // Every origin was made a group in the loop above.
                groups.entry(std::mem::take(&mut origin[index])).or_default().push(record);
            }
        }
        Self { groups: groups.into_iter().map(|(session_id, records)| (session_id.clone(), SessionGroupCache::new(&session_id, records))).collect() }
    }

    /// All session groups, keyed by session id.
    pub fn groups(&self) -> &BTreeMap<String, SessionGroupCache> {
        &self.groups
    }

    /// Every record, across every group.
    pub fn all_records(&self) -> impl Iterator<Item = &UsageRecord> {
        self.groups.values().flat_map(|group| group.records.iter())
    }

    /// Total number of records, after dedupe.
    pub fn record_count(&self) -> usize {
        self.groups.values().map(|group| group.records.len()).sum()
    }

    /// Sum of every record's cost, in USD.
    pub fn total_cost(&self) -> f64 {
        self.groups.values().map(SessionGroupCache::total_cost).sum()
    }

    /// Day (`YYYY-MM-DD`, local) -> model -> aggregate.
    pub fn daily_model_aggregates(&self) -> std::collections::HashMap<String, std::collections::HashMap<String, ModelAggregate>> {
        daily_model_aggregates(self.all_records())
    }
}

/// Group files by their path-derived session id, each group's paths sorted so
/// a parse always sees them in the same order.
pub fn group_files_by_session(files: &[PathBuf]) -> BTreeMap<String, Vec<PathBuf>> {
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        let (session_id, _project_path) = crate::paths::extract_session_parts(file);
        groups.entry(session_id).or_default().push(file.clone());
    }
    for paths in groups.values_mut() {
        paths.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
        paths.dedup();
    }
    groups
}

/// `$XDG_CACHE_HOME/statusline` (falling back to `~/.cache/statusline`): the
/// directory [`UsageTotals::empty_default`] keeps `usage.sqlite` in. Shared with
/// the status line's own state.
pub fn default_store_dir() -> PathBuf {
    sl_core::paths::cache_dir()
}

/// Where a store kept in `dir` lives.
pub fn store_path(dir: &Path) -> PathBuf {
    dir.join(DB_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_puts_a_transcript_and_its_subagent_replays_in_one_group() {
        let files = vec![
            PathBuf::from("/root/projects/proj/sess-b.jsonl"),
            PathBuf::from("/root/projects/proj/sess-a/subagents/replay-2.jsonl"),
            PathBuf::from("/root/projects/proj/sess-a.jsonl"),
            PathBuf::from("/root/projects/proj/sess-a/subagents/replay-1.jsonl"),
        ];
        let groups = group_files_by_session(&files);
        assert_eq!(groups.keys().collect::<Vec<_>>(), vec!["sess-a", "sess-b"]);
        assert_eq!(
            groups["sess-a"],
            vec![
                PathBuf::from("/root/projects/proj/sess-a.jsonl"),
                PathBuf::from("/root/projects/proj/sess-a/subagents/replay-1.jsonl"),
                PathBuf::from("/root/projects/proj/sess-a/subagents/replay-2.jsonl"),
            ],
            "paths within a group are sorted, so a re-parse always sees the same file order"
        );
    }

    #[test]
    fn duplicate_input_paths_collapse_within_a_group() {
        let file = PathBuf::from("/root/projects/proj/sess-a.jsonl");
        let groups = group_files_by_session(&[file.clone(), file.clone()]);
        assert_eq!(groups["sess-a"], vec![file]);
    }
}
