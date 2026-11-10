//! Ingest of Claude Code's and Codex's on-disk JSONL usage transcripts:
//! discovery of the files, parsing of each line into a priced
//! [`UsageRecord`], and dedupe of the records the same message left more than
//! once.
//!
//! [`cache::UsageTotals`] keeps them in an incremental SQLite store, so a
//! repeat run is a `stat()` sweep plus a tail read of whatever grew rather than
//! a full re-parse of the corpus; [`cache::UsageCache`] parses everything from
//! scratch, and is the oracle the store is tested against.

#![forbid(unsafe_code)]

pub mod aggregate;
pub mod cache;
pub mod codex;
pub mod dedupe;
pub mod paths;
pub mod pricing;
pub mod record;
pub mod watch;

pub use aggregate::{
    BlockBurnRate, SessionBlock, SessionSummary, block_burn_rate, identify_blocks, monday_of_week,
    month_of_date, session_summaries,
};
pub use cache::{
    ActiveBlock, ModelAggregate, ModelTotals, RefreshMode, RefreshStats, STORE_SCHEMA_VERSION,
    SessionGroupCache, Source, TokenTotals, UsageCache, UsageTotals, daily_model_aggregates,
    default_store_dir, group_files_by_session, store_path,
};
pub use dedupe::{
    dedupe_hash, dedupe_indices_to_fixpoint, dedupe_records, dedupe_to_fixpoint,
    sidechain_dedupe_hash,
};
pub use paths::{claude_paths, claude_paths_with, extract_session_parts, usage_files};
pub use pricing::{Pricing, PricingMap, UsageTokens};
pub use record::{
    Speed, UsageEntry, UsageRecord, local_date, parse_bytes, parse_file, parse_lines,
};

/// 64-bit FNV-1a over `parts`, each followed by a `0xff` separator (a byte no
/// UTF-8 string contains, so `["ab", "c"]` and `["a", "bc"]` differ).
///
/// For values that are *persisted*: unlike `FxHasher` or `DefaultHasher`, its
/// output is fixed by this definition and can never change under a dependency
/// upgrade, which would silently orphan every stored hash.
pub(crate) fn stable_hash(parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for part in parts {
        for &byte in part.iter().chain(std::iter::once(&0xff)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// One session group's files (a transcript and its `subagents/`
    /// replays), parsed in the given order and deduped to a fixpoint.
    fn parse_session_files(paths: &[PathBuf], pricing: &PricingMap) -> Vec<UsageRecord> {
        dedupe::dedupe_to_fixpoint(
            paths
                .iter()
                .flat_map(|path| record::parse_file(path, pricing))
                .collect(),
        )
    }

    #[test]
    fn parse_session_files_parses_and_dedupes_across_files() {
        let tmp_guard = tempfile::tempdir().unwrap();
        let tmp = tmp_guard.path().to_path_buf();
        let projects = tmp.join("projects").join("my-project");
        let subagents = projects.join("sess-1").join("subagents");
        fs::create_dir_all(&projects).unwrap();
        fs::create_dir_all(&subagents).unwrap();

        let main_file = projects.join("sess-1.jsonl");
        fs::write(
            &main_file,
            r#"{"timestamp":"2024-01-15T10:30:00.000Z","sessionId":"sess-1","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}
{"timestamp":"2024-01-15T10:31:00.000Z","sessionId":"sess-1","requestId":"req-2","message":{"id":"msg-2","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}
"#,
        )
        .unwrap();

        // Sidechain replay of msg-1 with a different requestId but the same
        // exact timestamp -- should collapse with the main file's entry.
        let replay_file = subagents.join("replay.jsonl");
        fs::write(
            &replay_file,
            r#"{"timestamp":"2024-01-15T10:30:00.000Z","sessionId":"sess-1","requestId":"req-3","isSidechain":true,"message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20}}}
"#,
        )
        .unwrap();

        let pricing = pricing::PricingMap::new();
        let records = parse_session_files(&[main_file, replay_file], &pricing);
        assert_eq!(
            records.len(),
            2,
            "msg-1 (parent + sidechain replay) counts once, msg-2 separately"
        );
        assert!(
            records
                .iter()
                .any(|r| r.message_id.as_deref() == Some("msg-1") && !r.is_sidechain)
        );
        assert!(
            records
                .iter()
                .any(|r| r.message_id.as_deref() == Some("msg-2"))
        );
    }

    /// Parsing a session group must not depend on the order it is handed its
    /// files. A single [`dedupe::dedupe_records`] pass does: feed it the
    /// subagent replay *before* the main transcript and the replay is checked
    /// against the parent's provisional timestamp (no match), only for the
    /// parent's finalising rewrite to retime that slot afterwards -- leaving
    /// two records where one whole-group parse in the favourable order leaves
    /// one. Deduping to a fixpoint inside `parse_session_files` closes it.
    #[test]
    fn parse_session_files_is_order_independent() {
        let tmp_guard = tempfile::tempdir().unwrap();
        let tmp = tmp_guard.path().to_path_buf();
        let projects = tmp.join("projects").join("my-project");
        let subagents = projects.join("sess-1").join("subagents");
        fs::create_dir_all(&projects).unwrap();
        fs::create_dir_all(&subagents).unwrap();

        // The provisional record written when the stream opens, then the real
        // one written ~seconds later under the same key and a later timestamp.
        let main_file = projects.join("sess-1.jsonl");
        fs::write(
            &main_file,
            r#"{"timestamp":"2024-01-15T10:30:00.000Z","sessionId":"sess-1","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":2}}}
{"timestamp":"2024-01-15T10:30:07.000Z","sessionId":"sess-1","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":5064}}}
"#,
        )
        .unwrap();
        // The replay carries the parent's *final* timestamp.
        let replay_file = subagents.join("replay.jsonl");
        fs::write(
            &replay_file,
            r#"{"timestamp":"2024-01-15T10:30:07.000Z","sessionId":"sess-1","requestId":"req-9","isSidechain":true,"message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":5064}}}
"#,
        )
        .unwrap();

        let pricing = pricing::PricingMap::new();
        let favourable = parse_session_files(&[main_file.clone(), replay_file.clone()], &pricing);
        let unfavourable = parse_session_files(&[replay_file, main_file], &pricing);
        assert_eq!(
            favourable.len(),
            1,
            "one message, however many times it was written"
        );
        assert_eq!(
            unfavourable.len(),
            1,
            "and the reversed file order must agree"
        );
        assert_eq!(
            unfavourable[0].output_tokens, 5064,
            "the finalised record wins the tie-break either way"
        );
        assert!(!unfavourable[0].is_sidechain);
    }
}
