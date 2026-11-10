//! Dedupe of parsed [`UsageRecord`]s from one or more files belonging to the
//! same session group (e.g. `<sessionId>.jsonl` plus its
//! `<sessionId>/subagents/*.jsonl` replays).
//!
//! Ported from <https://github.com/ccusage/ccusage/blob/8841f921/rust/adapters/claude/src/lib.rs>'s
//! `push_deduped_entry`/`should_replace_deduped_entry` and friends.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

use crate::record::UsageRecord;

/// Hash of the exact dedupe key `(message_id, request_id, session_id)`.
/// `request_id` is tri-state: `None` only matches another `None` (it is not a
/// wildcard), which the `Option::hash` impl gives us for free. Exposed `pub`
/// so a later incremental-cache layer can build its `seen_keys` index without
/// re-parsing already-cached records.
pub fn dedupe_hash(message_id: &str, request_id: Option<&str>, session_id: &str) -> u64 {
    let mut hasher = FxHasher::default();
    message_id.hash(&mut hasher);
    request_id.hash(&mut hasher);
    session_id.hash(&mut hasher);
    hasher.finish()
}

/// Hash of the broader sidechain-replay fallback key `(message_id,
/// session_id)`, ignoring `request_id`. Namespaced with a literal prefix so
/// its hash space doesn't alias [`dedupe_hash`]'s (a plain
/// `dedupe_hash(id, None, session)` would coincide with the exact hash of a
/// real `request_id: None` record; ccusage's `lib.rs` relies on that
/// coincidence, but its own `daily.rs` dedupe path uses this separately
/// namespaced form instead -- we do too, for a cleaner public API). Exposed
/// `pub` for the same reason as [`dedupe_hash`].
pub fn sidechain_dedupe_hash(message_id: &str, session_id: &str) -> u64 {
    let mut hasher = FxHasher::default();
    "sidechain-replay".hash(&mut hasher);
    message_id.hash(&mut hasher);
    session_id.hash(&mut hasher);
    hasher.finish()
}

/// Dedupe a `Vec<UsageRecord>` in a single parse pass, preserving input order
/// among survivors that don't collide.
///
/// - Records with no `message_id` are never deduped against anything --
///   always kept.
/// - Exact key: `(message_id, request_id, session_id)`.
/// - Sidechain-replay fallback: when the exact-key lookup misses, also check
///   `(message_id, session_id)` (ignoring `request_id`) for a record with the
///   *same exact timestamp*, when either the candidate or the existing match
///   has `is_sidechain: true`.
/// - Tie-break when a duplicate is found: non-sidechain wins over sidechain;
///   else higher total token count wins; else having `speed` set wins over
///   not having it set. See [`should_replace`].
///
/// **One pass is not a fixpoint** -- see [`dedupe_to_fixpoint`], which is what
/// every caller that owns a whole session group's records should use.
pub fn dedupe_records(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let keep = dedupe_pass(&records, 0..records.len());
    take_indexed(records, keep)
}

/// Run [`dedupe_records`] until it stops removing anything.
///
/// **A single [`dedupe_records`] pass is not a fixpoint.** When a duplicate
/// wins its tie-break it is written into the existing record's slot, which can
/// change that slot's `timestamp_ms` -- and the sidechain-replay fallback
/// matches on an *exact* timestamp. Records already emitted earlier in the
/// same pass are never re-checked against the updated slot, so a replay that
/// did not match the provisional timestamp can be left stranded beside the
/// finalised parent it should have collapsed into.
///
/// Concretely, and reachable on ordinary data: a subagent replay carries its
/// parent's *final* timestamp. Feed a pass `[parent-provisional, replay,
/// parent-final]` and the replay is checked while the parent slot still holds
/// the provisional timestamp (no match, so it survives), and only afterwards
/// does `parent-final` retime that slot -- leaving two records where a
/// whole-group parse, which sees the main transcript's two lines before the
/// subagent file's replay, leaves one.
///
/// No caller may rely on input order to avoid that: the incremental cache's
/// history is sorted by timestamp, not by file, and a public parse API cannot
/// constrain the file order it is handed. Settling to a fixpoint makes the
/// invariant "the result contains no collapsible pair" hold no matter what
/// order records arrived in, and keeps an incremental refresh and a
/// `--rebuild` in agreement.
///
/// Terminates: each pass either shortens the vector or ends the loop, and the
/// length is bounded below by zero. On real data the confirming second pass
/// finds nothing.
pub fn dedupe_to_fixpoint(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let keep = dedupe_indices_to_fixpoint(&records);
    take_indexed(records, keep)
}

/// The same fixpoint dedupe as [`dedupe_to_fixpoint`], but reported as the
/// indices of the survivors (in output order) rather than as the records
/// themselves.
///
/// For callers that must know *which* of their inputs lost, not just what the
/// deduped set is -- the cache's cross-session-group reconciliation has to map
/// each casualty back to the session group it has to be removed from. See
/// [`crate::cache`].
pub fn dedupe_indices_to_fixpoint(records: &[UsageRecord]) -> Vec<usize> {
    let mut keep = dedupe_pass(records, 0..records.len());
    loop {
        let previous_len = keep.len();
        keep = dedupe_pass(records, keep);
        if keep.len() == previous_len {
            return keep;
        }
    }
}

/// One dedupe pass over `candidates` (indices into `records`), returning the
/// surviving indices in output order. The single implementation of the dedupe
/// algorithm; every public entry point above is a thin wrapper over it.
fn dedupe_pass(records: &[UsageRecord], candidates: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut indexes: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
    let mut keep: Vec<usize> = Vec::with_capacity(records.len());
    for candidate in candidates {
        push_deduped_index(candidate, records, &mut indexes, &mut keep);
    }
    keep
}

/// Move the records at `keep` (each index at most once, in output order) out
/// of `records`.
fn take_indexed(records: Vec<UsageRecord>, keep: Vec<usize>) -> Vec<UsageRecord> {
    // Nothing was dropped, so `keep` is the identity permutation and the
    // input is already the answer -- the common case, and the one every
    // fixpoint loop ends on.
    if keep.len() == records.len() {
        return records;
    }
    let mut slots: Vec<Option<UsageRecord>> = records.into_iter().map(Some).collect();
    // Each kept index appears once, so every slot taken is still full.
    keep.into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}

/// `keep` holds indices into `records`; `indexes` maps a dedupe hash to
/// *positions within `keep`*, so a tie-break replacement can overwrite a
/// position without disturbing anything already indexed against it.
fn push_deduped_index(
    candidate: usize,
    records: &[UsageRecord],
    indexes: &mut FxHashMap<u64, Vec<usize>>,
    keep: &mut Vec<usize>,
) {
    let record = &records[candidate];
    let Some(message_id) = record.message_id.as_deref() else {
        // No message_id: never deduped against anything, and never registered
        // in the index (so nothing else can match against it either).
        keep.push(candidate);
        return;
    };

    let request_id = record.request_id.as_deref();
    let session_id = record.session_id.as_str();
    let exact_hash = dedupe_hash(message_id, request_id, session_id);

    let existing_position = indexes
        .get(&exact_hash)
        .and_then(|positions| {
            positions.iter().copied().find(|&position| {
                matches_exact_key(&records[keep[position]], message_id, request_id, session_id)
            })
        })
        .or_else(|| {
            // Sidechain logs can replay parent messages with new request IDs.
            let broad_hash = sidechain_dedupe_hash(message_id, session_id);
            let candidate_is_sidechain = record.is_sidechain;
            indexes.get(&broad_hash).and_then(|positions| {
                positions.iter().copied().find(|&position| {
                    matches_sidechain_key(
                        &records[keep[position]],
                        message_id,
                        session_id,
                        record.timestamp_ms,
                        candidate_is_sidechain,
                    )
                })
            })
        });

    if let Some(position) = existing_position {
        if should_replace(record, &records[keep[position]]) {
            keep[position] = candidate;
            push_index(indexes, exact_hash, position);
            push_index(
                indexes,
                sidechain_dedupe_hash(message_id, session_id),
                position,
            );
        }
        return;
    }

    let position = keep.len();
    keep.push(candidate);
    push_index(indexes, exact_hash, position);
    push_index(
        indexes,
        sidechain_dedupe_hash(message_id, session_id),
        position,
    );
}

fn matches_exact_key(
    record: &UsageRecord,
    message_id: &str,
    request_id: Option<&str>,
    session_id: &str,
) -> bool {
    record.message_id.as_deref() == Some(message_id)
        && record.request_id.as_deref() == request_id
        && record.session_id == session_id
}

fn matches_sidechain_key(
    record: &UsageRecord,
    message_id: &str,
    session_id: &str,
    timestamp_ms: i64,
    candidate_is_sidechain: bool,
) -> bool {
    record.message_id.as_deref() == Some(message_id)
        && record.session_id == session_id
        && record.timestamp_ms == timestamp_ms
        && (candidate_is_sidechain || record.is_sidechain)
}

fn push_index(indexes: &mut FxHashMap<u64, Vec<usize>>, hash: u64, index: usize) {
    let bucket = indexes.entry(hash).or_default();
    if !bucket.contains(&index) {
        bucket.push(index);
    }
}

/// Whether `candidate` should replace `existing` when both map to the same
/// dedupe key. Ported verbatim (comparison order included) from ccusage's
/// `should_replace_deduped_entry`:
/// 1. Non-sidechain wins over sidechain.
/// 2. Else higher total token count wins.
/// 3. Else having `speed` set at all (not specifically `Fast`) wins over not
///    having it set.
fn should_replace(candidate: &UsageRecord, existing: &UsageRecord) -> bool {
    if candidate.is_sidechain != existing.is_sidechain {
        return existing.is_sidechain;
    }

    let candidate_total = candidate.total_tokens();
    let existing_total = existing.total_tokens();
    if candidate_total != existing_total {
        return candidate_total > existing_total;
    }

    candidate.speed.is_some() && existing.speed.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Speed;

    fn record(message_id: Option<&str>, request_id: Option<&str>, session_id: &str) -> UsageRecord {
        UsageRecord {
            timestamp_ms: 1_000,
            date: "2024-01-15".to_string(),
            session_id: session_id.to_string(),
            project_path: "proj".to_string(),
            input_tokens: 10,
            output_tokens: 10,
            model: Some("claude-sonnet-4".to_string()),
            message_id: message_id.map(str::to_string),
            request_id: request_id.map(str::to_string),
            ..UsageRecord::default()
        }
    }

    #[test]
    fn same_message_request_session_collapses_to_one() {
        let records = vec![
            record(Some("msg-1"), Some("req-1"), "sess-1"),
            record(Some("msg-1"), Some("req-1"), "sess-1"),
        ];
        let deduped = dedupe_records(records);
        assert_eq!(deduped.len(), 1);
    }

    #[test]
    fn tie_break_non_sidechain_wins_over_sidechain() {
        let mut sidechain = record(Some("msg-1"), Some("req-1"), "sess-1");
        sidechain.is_sidechain = true;
        sidechain.input_tokens = 999; // higher token total, but should still lose
        let plain = record(Some("msg-1"), Some("req-1"), "sess-1");

        let deduped = dedupe_records(vec![sidechain, plain.clone()]);
        assert_eq!(deduped.len(), 1);
        assert!(!deduped[0].is_sidechain);
        assert_eq!(deduped[0].input_tokens, plain.input_tokens);

        // Order reversed: non-sidechain arrives first, sidechain later -- the
        // non-sidechain should still win.
        let mut sidechain2 = record(Some("msg-1"), Some("req-1"), "sess-1");
        sidechain2.is_sidechain = true;
        sidechain2.input_tokens = 999;
        let deduped2 = dedupe_records(vec![plain.clone(), sidechain2]);
        assert_eq!(deduped2.len(), 1);
        assert!(!deduped2[0].is_sidechain);
    }

    #[test]
    fn tie_break_higher_token_count_wins() {
        let mut low = record(Some("msg-1"), Some("req-1"), "sess-1");
        low.input_tokens = 5;
        let mut high = record(Some("msg-1"), Some("req-1"), "sess-1");
        high.input_tokens = 500;

        let deduped = dedupe_records(vec![low, high]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].input_tokens, 500);
    }

    #[test]
    fn tie_break_speed_present_wins_when_totals_and_sidechain_match() {
        let plain = record(Some("msg-1"), Some("req-1"), "sess-1");
        let mut with_speed = record(Some("msg-1"), Some("req-1"), "sess-1");
        with_speed.speed = Some(Speed::Standard);

        let deduped = dedupe_records(vec![plain, with_speed]);
        assert_eq!(deduped.len(), 1);
        assert!(deduped[0].speed.is_some());
    }

    #[test]
    fn sidechain_replay_with_different_request_id_same_timestamp_collapses() {
        let mut parent = record(Some("msg-1"), Some("req-1"), "sess-1");
        parent.timestamp_ms = 5_000;
        let mut sidechain_replay = record(Some("msg-1"), Some("req-2"), "sess-1");
        sidechain_replay.timestamp_ms = 5_000;
        sidechain_replay.is_sidechain = true;

        let deduped = dedupe_records(vec![parent, sidechain_replay]);
        assert_eq!(deduped.len(), 1);
    }

    #[test]
    fn sidechain_replay_with_different_timestamp_does_not_collapse() {
        let mut parent = record(Some("msg-1"), Some("req-1"), "sess-1");
        parent.timestamp_ms = 5_000;
        let mut sidechain_replay = record(Some("msg-1"), Some("req-2"), "sess-1");
        sidechain_replay.timestamp_ms = 6_000;
        sidechain_replay.is_sidechain = true;

        let deduped = dedupe_records(vec![parent, sidechain_replay]);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn request_id_none_matches_none_but_not_some() {
        let a = record(Some("msg-1"), None, "sess-1");
        let b = record(Some("msg-1"), None, "sess-1");
        let deduped = dedupe_records(vec![a, b]);
        assert_eq!(
            deduped.len(),
            1,
            "two None request_ids should count as duplicates"
        );

        let mut c = record(Some("msg-1"), None, "sess-1");
        c.timestamp_ms = 1_234; // distinct timestamp so the sidechain fallback can't match either
        let d = record(Some("msg-1"), Some("req-x"), "sess-1");
        let deduped2 = dedupe_records(vec![c, d]);
        assert_eq!(
            deduped2.len(),
            2,
            "None request_id should not match Some(x)"
        );
    }

    #[test]
    fn no_message_id_records_are_never_deduped() {
        let a = record(None, Some("req-1"), "sess-1");
        let b = record(None, Some("req-1"), "sess-1");
        let deduped = dedupe_records(vec![a, b]);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedupe_hash_none_and_some_request_id_differ() {
        assert_ne!(
            dedupe_hash("msg", None, "sess"),
            dedupe_hash("msg", Some("req"), "sess")
        );
    }
}
