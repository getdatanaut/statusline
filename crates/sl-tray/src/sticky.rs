//! Sessions that have closed, remembered so they can be found again.
//!
//! Claude Code's and Codex's alike, each marked with which it was.
//!
//! `claude agents` lists only running sessions, so a session closed after its
//! PR went up drops out of the tray just when its review comes back. Every
//! session seen running is remembered here, and once it stops running it is
//! listed as closed -- with its worktree and its id, to resume it by hand --
//! until it is dismissed, two weeks pass, or its transcript or worktree goes.
//!
//! Kept apart from the open sessions on purpose: nothing that counts, warns
//! about, or keeps the Mac awake for open sessions ever sees these.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::sessions::OpenSession;
use crate::view_prefs::Source;

/// A closed session is listed for this long after it was last seen running.
pub const KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// At most this many are remembered; the longest-gone go first.
pub const MAX_ENTRIES: usize = 200;
/// Where the list is kept, as a JSON string in the user defaults.
const DEFAULTS_KEY: &str = "stickySessions";

/// What is remembered of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remembered {
    pub session_id: String,
    /// Its title, as the open list last showed it.
    pub name: String,
    pub project: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub pr_number: Option<i64>,
    /// The PR its PR is based on, for piecing together hand-made stacks.
    /// Absent from lists saved before it was kept.
    #[serde(default)]
    pub base_pr: Option<i64>,
    /// When it was last seen running.
    pub last_seen_ms: i64,
    /// Claude Code's or Codex's. Absent from lists saved before Codex was
    /// supported, which held Claude Code's alone.
    #[serde(default, with = "source_key")]
    pub provider: Source,
}

/// [`Source`] as stored: `"claude"` or `"codex"`.
mod source_key {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::view_prefs::Source;

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` calls it with a reference"
    )]
    pub fn serialize<S: Serializer>(source: &Source, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match source {
            Source::Claude => "claude",
            Source::Codex => "codex",
        })
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Source, D::Error> {
        Ok(if String::deserialize(deserializer)? == "codex" {
            Source::Codex
        } else {
            Source::Claude
        })
    }
}

impl Remembered {
    /// The command that resumes it in a terminal opened in its directory.
    pub fn resume_command(&self) -> String {
        match self.provider {
            Source::Claude => format!("claude --resume {}", self.session_id),
            Source::Codex => format!("codex resume {}", self.session_id),
        }
    }

    fn from_open(session: &OpenSession, now_ms: i64) -> Self {
        Self {
            session_id: session.session_id.clone(),
            name: session.name.clone(),
            project: session.project.clone(),
            cwd: session.cwd.clone(),
            branch: session.branch.clone(),
            pr_number: session.gh.as_ref().and_then(|gh| gh.pr_number),
            base_pr: session.gh.as_ref().and_then(|gh| gh.base_pr),
            last_seen_ms: now_ms,
            provider: session.provider,
        }
    }

    /// Everything but when it was seen: what is worth writing down at once.
    fn same_facts(&self, other: &Self) -> bool {
        Self {
            last_seen_ms: 0,
            ..self.clone()
        } == Self {
            last_seen_ms: 0,
            ..other.clone()
        }
    }
}

/// Every session remembered, open or closed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Sticky {
    entries: Vec<Remembered>,
}

impl Sticky {
    /// The list as stored; empty when nothing is, or it cannot be read. An
    /// entry that cannot be read is left out alone: the next save would
    /// otherwise write the list back without all the others.
    pub fn from_json(json: &str) -> Self {
        let entries: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
        Self {
            entries: entries
                .into_iter()
                .filter_map(|entry| serde_json::from_value(entry).ok())
                .collect(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.entries).unwrap_or_else(|_| "[]".to_owned())
    }

    /// The list kept in the user defaults.
    pub fn load() -> Self {
        crate::view_prefs::read_string(DEFAULTS_KEY)
            .map(|json| Self::from_json(&json))
            .unwrap_or_default()
    }

    pub fn save(&self) {
        crate::view_prefs::write_string(DEFAULTS_KEY, &self.to_json());
    }

    /// Remember the running `live` sessions as they are now. Returns whether
    /// anything changed beyond when they were last seen, which is worth
    /// saving at once; the times alone can wait for the next save.
    pub fn observe(&mut self, live: &[OpenSession], now_ms: i64) -> bool {
        let mut changed = false;
        for session in live.iter().filter(|s| !s.session_id.is_empty()) {
            let fresh = Remembered::from_open(session, now_ms);
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.session_id == session.session_id)
            {
                changed |= !entry.same_facts(&fresh);
                *entry = fresh;
            } else {
                self.entries.push(fresh);
                changed = true;
            }
        }
        if self.entries.len() > MAX_ENTRIES {
            self.entries
                .sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms));
            self.entries.truncate(MAX_ENTRIES);
            changed = true;
        }
        changed
    }

    /// The closed sessions worth listing, most recently seen first: not
    /// running, seen within [`KEEP_MS`], and still there (`exists`).
    pub fn closed(
        &self,
        live_ids: &[&str],
        now_ms: i64,
        mut exists: impl FnMut(&Remembered) -> bool,
    ) -> Vec<Remembered> {
        let mut out: Vec<Remembered> = self
            .entries
            .iter()
            .filter(|e| {
                !live_ids.contains(&e.session_id.as_str()) && now_ms - e.last_seen_ms < KEEP_MS
            })
            .filter(|e| exists(e))
            .cloned()
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms));
        out
    }

    /// Forget closed sessions that are past [`KEEP_MS`] or gone (`exists`).
    /// Returns whether any was forgotten.
    pub fn prune(
        &mut self,
        live_ids: &[&str],
        now_ms: i64,
        mut exists: impl FnMut(&Remembered) -> bool,
    ) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| {
            live_ids.contains(&e.session_id.as_str())
                || (now_ms - e.last_seen_ms < KEEP_MS && exists(e))
        });
        self.entries.len() != before
    }

    /// Add `found` -- sessions from before any was seen running -- where they
    /// are not already known, keeping the newest [`MAX_ENTRIES`].
    pub fn adopt(&mut self, found: Vec<Remembered>) {
        for entry in found {
            if !self
                .entries
                .iter()
                .any(|e| e.session_id == entry.session_id)
            {
                self.entries.push(entry);
            }
        }
        if self.entries.len() > MAX_ENTRIES {
            self.entries
                .sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms));
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    /// Forget one session. Returns whether it was remembered.
    /// `session_id` is still remembered: seen running, and not dismissed.
    pub fn remembers(&self, session_id: &str) -> bool {
        self.entries.iter().any(|e| e.session_id == session_id)
    }

    pub fn dismiss(&mut self, session_id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.session_id != session_id);
        self.entries.len() != before
    }
}

/// Set once the transcripts have been scanned for sessions from before the
/// tray first ran, so they are never scanned again: a second scan would bring
/// back sessions dismissed since.
const BACKFILLED_KEY: &str = "stickyBackfilled";

pub fn backfill_done() -> bool {
    crate::view_prefs::read_string(BACKFILLED_KEY).is_some()
}

pub fn mark_backfill_done() {
    crate::view_prefs::write_string(BACKFILLED_KEY, "1");
}

/// What a transcript's tail says about its session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TranscriptFacts {
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// The name given with `/rename`, else the one Claude Code generated.
    pub name: Option<String>,
    /// `claude -p` and SDK runs say `sdk-cli`; a session that says nothing is
    /// from before the field and taken as interactive.
    pub interactive: bool,
    /// Something was asked and answered: an assistant turn, or a title, which
    /// Claude Code only gives after the first prompt.
    pub prompted: bool,
}

/// [`TranscriptFacts`] from the end of a transcript: the latest value of each.
pub fn transcript_facts(tail: &str) -> TranscriptFacts {
    let mut facts = TranscriptFacts {
        interactive: true,
        ..TranscriptFacts::default()
    };
    let (mut custom, mut generated) = (None, None);
    let mut entrypoint: Option<String> = None;
    for line in tail.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let text = |key: &str| {
            record
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        match record.get("type").and_then(|v| v.as_str()) {
            Some("custom-title") => custom = text("customTitle").or(custom),
            Some("ai-title") => generated = text("aiTitle").or(generated),
            Some("assistant") => facts.prompted = true,
            _ => {}
        }
        facts.cwd = text("cwd").or(facts.cwd.take());
        facts.branch = text("gitBranch")
            .filter(|b| b != "HEAD")
            .or(facts.branch.take());
        entrypoint = text("entrypoint").or(entrypoint);
    }
    facts.prompted |= custom.is_some() || generated.is_some();
    facts.name = custom.or(generated);
    facts.interactive = entrypoint.is_none_or(|e| e == "cli");
    facts
}

/// The sessions worth remembering from before the tray first ran: every
/// transcript modified within [`KEEP_MS`] that was an interactive session
/// with a prompt in it, not running now, in a directory still there. Read from
/// each transcript's tail; its PR only from the `gh` store, never the network.
pub fn backfill(live_ids: &[&str], now_ms: i64) -> Vec<Remembered> {
    let gh_store = sl_core::gh::Store::open_default();
    let mut found = Vec::new();
    for base in sl_usage::paths::claude_paths().unwrap_or_default() {
        let Ok(projects) = std::fs::read_dir(base.join("projects")) else {
            continue;
        };
        for project in projects.flatten() {
            // Only `<project>/<id>.jsonl`: subagents' are a level further down.
            let Ok(files) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                    continue;
                };
                let modified = file
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                let Some(last_seen_ms) =
                    modified.map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                else {
                    continue;
                };
                if now_ms - last_seen_ms >= KEEP_MS || live_ids.contains(&id.as_str()) {
                    continue;
                }
                let Some(tail) = crate::cache_ttl::tail_text(&path) else {
                    continue;
                };
                if let Some(entry) = remembered_from(
                    &id,
                    &transcript_facts(&tail),
                    last_seen_ms,
                    |cwd, branch| {
                        gh_store
                            .as_ref()
                            .and_then(|store| sl_core::gh::load(store, cwd, branch))
                    },
                ) {
                    found.push(entry);
                }
            }
        }
    }
    found.sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms));
    found.truncate(MAX_ENTRIES);
    found
}

/// A transcript's session as remembered, or `None` if it is not worth it.
fn remembered_from(
    id: &str,
    facts: &TranscriptFacts,
    last_seen_ms: i64,
    gh: impl Fn(&str, &str) -> Option<sl_core::gh::GhInfo>,
) -> Option<Remembered> {
    let cwd = facts.cwd.clone()?;
    if !facts.interactive || !facts.prompted || !std::path::Path::new(&cwd).is_dir() {
        return None;
    }
    let info = facts.branch.as_deref().and_then(|branch| gh(&cwd, branch));
    let dir_name = std::path::Path::new(&cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&cwd)
        .to_owned();
    let project = sl_core::git::common_dir(&cwd).map_or_else(
        || dir_name.clone(),
        |dir| crate::sessions::project_name(&dir),
    );
    Some(Remembered {
        session_id: id.to_owned(),
        name: facts.name.clone().unwrap_or(dir_name),
        project,
        cwd,
        branch: facts.branch.clone(),
        pr_number: info.as_ref().and_then(|i| i.pr_number),
        base_pr: info.as_ref().and_then(|i| i.base_pr),
        last_seen_ms,
        provider: Source::Claude,
    })
}

/// How long "no transcript" is trusted: looking for one means listing
/// every project directory, or every Codex rollout.
const EXISTS_FOR_MS: i64 = 5 * 60 * 1000;

/// What was found of one session's transcript.
enum Seen {
    /// Here; only `stat`ed from now on.
    At(std::path::PathBuf),
    /// Nowhere, as of this time.
    Missing(i64),
}

/// Whether each remembered session's worktree and transcript are still on
/// disk. A transcript once found is only `stat`ed after that; one not found
/// is looked for again every few minutes.
#[derive(Default)]
pub struct ExistsCache {
    seen: HashMap<String, Seen>,
}

impl ExistsCache {
    pub fn exists(&mut self, entry: &Remembered, now_ms: i64) -> bool {
        self.exists_with(entry, now_ms, || match entry.provider {
            Source::Claude => crate::cache_ttl::find_transcript(&entry.session_id),
            // A Codex thread's rollout, archived or not, plain or compressed.
            Source::Codex => sl_usage::codex::codex_home().and_then(|home| {
                sl_usage::codex::codex_files(&home)
                    .into_iter()
                    .find(|path| {
                        sl_usage::codex::thread_of(path).as_deref()
                            == Some(entry.session_id.as_str())
                    })
            }),
        })
    }

    /// As [`exists`](Self::exists), looking for the transcript with `find`.
    fn exists_with(
        &mut self,
        entry: &Remembered,
        now_ms: i64,
        find: impl FnOnce() -> Option<std::path::PathBuf>,
    ) -> bool {
        if !std::path::Path::new(&entry.cwd).is_dir() {
            return false;
        }
        match self.seen.get(&entry.session_id) {
            // A rollout that was archived has moved: look again.
            Some(Seen::At(path)) if path.is_file() => return true,
            Some(Seen::Missing(at)) if now_ms - at < EXISTS_FOR_MS => return false,
            _ => {}
        }
        let found = find();
        let there = found.is_some();
        self.seen.insert(
            entry.session_id.clone(),
            found.map_or(Seen::Missing(now_ms), Seen::At),
        );
        there
    }
}

/// `closed 5m`, `closed 3h`, `closed 2d`: how long ago, in its largest unit.
pub fn closed_ago(last_seen_ms: i64, now_ms: i64) -> String {
    format!(
        "closed {}",
        sl_core::format::ago_short(now_ms - last_seen_ms)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::Activity;

    const MIN: i64 = 60_000;
    const DAY: i64 = 24 * 60 * MIN;

    fn live(id: &str, name: &str, branch: Option<&str>, pr: Option<i64>) -> OpenSession {
        OpenSession {
            session_id: id.into(),
            name: name.into(),
            project: "statusline".into(),
            cwd: format!("/w/{id}"),
            activity: Activity::Idle,
            started_ms: 0,
            branch: branch.map(Into::into),
            gh: pr.map(|n| sl_core::gh::GhInfo {
                pr_number: Some(n),
                ..sl_core::gh::GhInfo::default()
            }),
            cache: None,
            remote_control: None,
            pid: None,
            context: None,
            linear: None,
            provider: crate::view_prefs::Source::Claude,
            last_activity_ms: None,
        }
    }

    #[test]
    fn running_sessions_are_remembered_with_their_name_branch_and_pr() {
        let mut sticky = Sticky::default();
        assert!(sticky.observe(
            &[live("a", "Ontology M4", Some("jakub/dn-479"), Some(781))],
            10
        ));
        assert_eq!(
            sticky.entries,
            [Remembered {
                session_id: "a".into(),
                name: "Ontology M4".into(),
                project: "statusline".into(),
                cwd: "/w/a".into(),
                branch: Some("jakub/dn-479".into()),
                pr_number: Some(781),
                base_pr: None,
                last_seen_ms: 10,
                provider: Source::Claude
            }]
        );
        assert!(
            !sticky.observe(
                &[live("a", "Ontology M4", Some("jakub/dn-479"), Some(781))],
                20
            ),
            "only the time moved"
        );
        assert_eq!(sticky.entries[0].last_seen_ms, 20);
        assert!(
            sticky.observe(&[live("a", "Renamed", Some("jakub/dn-479"), Some(781))], 30),
            "a new title is worth saving"
        );
        assert!(
            !sticky.observe(&[live("", "no id", None, None)], 40),
            "a session without an id cannot be resumed"
        );
        assert_eq!(sticky.entries.len(), 1);
    }

    #[test]
    fn closed_lists_the_gone_recent_and_present_newest_first() {
        let mut sticky = Sticky::default();
        sticky.observe(&[live("old", "old", None, None)], 0);
        sticky.observe(&[live("b", "b", None, None)], 15 * DAY - 2 * MIN);
        sticky.observe(&[live("c", "c", None, None)], 15 * DAY - MIN);
        sticky.observe(&[live("running", "running", None, None)], 15 * DAY - MIN);
        sticky.observe(&[live("deleted", "deleted", None, None)], 15 * DAY - MIN);
        let now = 15 * DAY;
        let closed = sticky.closed(&["running"], now, |e| e.session_id != "deleted");
        let ids: Vec<&str> = closed.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["c", "b"],
            "past two weeks, still running, and gone are all left out"
        );

        assert!(sticky.prune(&["running"], now, |e| e.session_id != "deleted"));
        let kept: Vec<&str> = sticky
            .entries
            .iter()
            .map(|e| e.session_id.as_str())
            .collect();
        assert_eq!(kept, ["b", "c", "running"]);
        assert!(
            !sticky.prune(&["running"], now, |_| true),
            "nothing more to forget"
        );
    }

    #[test]
    fn a_found_transcript_is_only_statted_and_a_missing_one_looked_for_every_few_minutes() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let transcript = dir.join("t.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let mut sticky = Sticky::default();
        sticky.observe(&[live("a", "a", None, None)], 0);
        let entry = Remembered {
            cwd: dir.to_string_lossy().into_owned(),
            ..sticky.closed(&[], MIN, |_| true).remove(0)
        };

        let mut cache = ExistsCache::default();
        let looked = std::cell::Cell::new(0);
        let mut ask = |now: i64, found: Option<&std::path::Path>| {
            cache.exists_with(&entry, now, || {
                looked.set(looked.get() + 1);
                found.map(std::path::Path::to_path_buf)
            })
        };
        assert!(ask(0, Some(&transcript)), "found");
        assert!(ask(10 * DAY, None), "still there: only statted");
        assert_eq!(looked.get(), 1, "not looked for again");
        std::fs::remove_file(&transcript).unwrap();
        assert!(!ask(10 * DAY, None), "gone, and nowhere else");
        assert_eq!(looked.get(), 2, "looked for once it was gone");
        assert!(
            !ask(10 * DAY + MIN, Some(&transcript)),
            "missing is trusted for a while"
        );
        assert_eq!(looked.get(), 2);
        std::fs::write(&transcript, "").unwrap();
        assert!(
            ask(10 * DAY + EXISTS_FOR_MS, Some(&transcript)),
            "and looked for again after it"
        );
        assert_eq!(looked.get(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            !ask(10 * DAY + EXISTS_FOR_MS, Some(&transcript)),
            "its worktree gone: gone, whatever the transcript"
        );
        assert_eq!(looked.get(), 3, "without looking");
    }

    #[test]
    fn a_dismissed_session_is_forgotten_until_it_runs_again() {
        let mut sticky = Sticky::default();
        sticky.observe(&[live("a", "a", None, None), live("b", "b", None, None)], 0);
        assert!(sticky.remembers("a"), "seen running");
        assert!(sticky.dismiss("a"));
        assert!(!sticky.dismiss("a"));
        assert!(!sticky.remembers("a"), "dismissed");
        assert!(sticky.remembers("b"), "the other is kept");
        assert_eq!(sticky.closed(&[], MIN, |_| true).len(), 1);
        sticky.observe(&[live("a", "a", None, None)], 2 * MIN);
        assert_eq!(
            sticky.closed(&[], 3 * MIN, |_| true).len(),
            2,
            "resumed and closed again: back"
        );
    }

    #[test]
    fn the_list_is_capped_by_dropping_the_longest_gone() {
        let mut sticky = Sticky::default();
        for i in 0..(i64::try_from(MAX_ENTRIES).unwrap_or(i64::MAX) + 5) {
            sticky.observe(&[live(&format!("s{i}"), "x", None, None)], i);
        }
        assert_eq!(sticky.entries.len(), MAX_ENTRIES);
        assert!(
            sticky.entries.iter().all(|e| e.last_seen_ms >= 5),
            "the five oldest went"
        );
    }

    #[test]
    fn the_list_survives_a_round_trip_and_bad_json_is_empty() {
        let mut sticky = Sticky::default();
        sticky.observe(&[live("a", "a", Some("main"), Some(1))], 7);
        assert_eq!(Sticky::from_json(&sticky.to_json()), sticky);
        assert_eq!(Sticky::from_json("not json"), Sticky::default());
    }

    #[test]
    fn an_entry_that_cannot_be_read_costs_only_itself() {
        let mut sticky = Sticky::default();
        sticky.observe(&[live("a", "a", Some("main"), Some(1))], 7);
        let json = sticky
            .to_json()
            .replacen('[', r#"[{"session_id":"bad","name":null},"#, 1);
        assert_eq!(Sticky::from_json(&json), sticky);
    }

    #[test]
    fn a_transcripts_tail_gives_its_latest_place_name_and_kind() {
        let tail = [
            r#"{"type":"user","cwd":"/w/old","gitBranch":"main","entrypoint":"cli","message":{"content":"hi"}}"#,
            r#"{"type":"ai-title","aiTitle":"First title"}"#,
            r#"{"type":"assistant","cwd":"/w/new","gitBranch":"jakub/dn-1","entrypoint":"cli"}"#,
            r#"{"type":"ai-title","aiTitle":"Better title"}"#,
            r#"{"type":"assistant","isSidechain":true,"cwd":"/w/subagent","gitBranch":"x"}"#,
        ]
        .join("\n");
        let facts = transcript_facts(&tail);
        assert_eq!(
            facts,
            TranscriptFacts {
                cwd: Some("/w/new".into()),
                branch: Some("jakub/dn-1".into()),
                name: Some("Better title".into()),
                interactive: true,
                prompted: true
            }
        );

        let renamed = format!(
            "{tail}\n{}",
            r#"{"type":"custom-title","customTitle":"ontology-supervisor"}"#
        );
        assert_eq!(
            transcript_facts(&renamed).name.as_deref(),
            Some("ontology-supervisor"),
            "a name given by hand wins"
        );
    }

    #[test]
    fn non_interactive_unprompted_and_detached_sessions_are_told_apart() {
        let sdk = r#"{"type":"assistant","cwd":"/w","entrypoint":"sdk-cli"}"#;
        assert!(!transcript_facts(sdk).interactive, "claude -p and SDK runs");
        let old = r#"{"type":"assistant","cwd":"/w"}"#;
        assert!(
            transcript_facts(old).interactive,
            "no entrypoint: from before the field"
        );
        let empty = r#"{"type":"last-prompt","leafUuid":"x"}
{"type":"user","cwd":"/w","gitBranch":"HEAD","entrypoint":"cli","message":{"content":"<command-name>/exit</command-name>"}}"#;
        let facts = transcript_facts(empty);
        assert!(
            !facts.prompted,
            "opened and left: nothing answered, no title"
        );
        assert_eq!(facts.branch, None, "a detached HEAD is no branch");
    }

    #[test]
    fn a_transcript_becomes_a_remembered_session_only_when_worth_it() {
        let here = std::env::temp_dir().display().to_string();
        let facts = TranscriptFacts {
            cwd: Some(here.clone()),
            branch: Some("jakub/dn-9".into()),
            name: None,
            interactive: true,
            prompted: true,
        };
        let gh = |_: &str, _: &str| {
            Some(sl_core::gh::GhInfo {
                pr_number: Some(9),
                base_pr: Some(8),
                ..sl_core::gh::GhInfo::default()
            })
        };
        let entry = remembered_from("abc", &facts, 5, gh).expect("a real session");
        assert_eq!(
            (entry.pr_number, entry.base_pr, entry.last_seen_ms),
            (Some(9), Some(8), 5)
        );
        assert!(
            !entry.name.is_empty(),
            "no title: named after its directory"
        );

        let skip = |f: TranscriptFacts| remembered_from("abc", &f, 5, gh).is_none();
        assert!(skip(TranscriptFacts {
            interactive: false,
            ..facts_clone(&facts)
        }));
        assert!(skip(TranscriptFacts {
            prompted: false,
            ..facts_clone(&facts)
        }));
        assert!(skip(TranscriptFacts {
            cwd: Some("/no/such/dir".into()),
            ..facts_clone(&facts)
        }));
        assert!(skip(TranscriptFacts {
            cwd: None,
            ..facts_clone(&facts)
        }));
    }

    fn facts_clone(f: &TranscriptFacts) -> TranscriptFacts {
        TranscriptFacts {
            cwd: f.cwd.clone(),
            branch: f.branch.clone(),
            name: f.name.clone(),
            interactive: f.interactive,
            prompted: f.prompted,
        }
    }

    #[test]
    fn adopting_keeps_what_is_known_and_the_cap() {
        let mut sticky = Sticky::default();
        sticky.observe(&[live("a", "live name", None, None)], 100);
        let old = |id: &str, at: i64| Remembered {
            session_id: id.into(),
            name: "old".into(),
            project: "p".into(),
            cwd: "/w".into(),
            branch: None,
            pr_number: None,
            base_pr: None,
            last_seen_ms: at,
            provider: Source::Claude,
        };
        sticky.adopt(vec![old("a", 1), old("b", 2)]);
        assert_eq!(sticky.entries.len(), 2);
        assert_eq!(
            sticky.entries[0].name, "live name",
            "a session already known keeps what was seen of it"
        );
        sticky.adopt(
            (0..i64::try_from(MAX_ENTRIES).unwrap_or(i64::MAX) + 10)
                .map(|i| old(&format!("x{i}"), 1000 + i))
                .collect(),
        );
        assert_eq!(sticky.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn lists_saved_before_codex_load_as_claude_and_a_codex_entry_round_trips() {
        let json = r#"[{"session_id":"a","name":"n","project":"p","cwd":"/w","branch":null,"pr_number":3,"last_seen_ms":1}]"#;
        let old = Sticky::from_json(json);
        assert_eq!(old.entries[0].provider, Source::Claude);
        assert_eq!(old.entries[0].resume_command(), "claude --resume a");
        let codex = Remembered {
            provider: Source::Codex,
            ..old.entries[0].clone()
        };
        let saved = Sticky {
            entries: vec![codex.clone()],
        }
        .to_json();
        assert!(saved.contains(r#""provider":"codex""#), "{saved}");
        assert_eq!(Sticky::from_json(&saved).entries, vec![codex.clone()]);
        assert_eq!(codex.resume_command(), "codex resume a");
    }

    #[test]
    fn a_codex_session_seen_running_is_remembered_as_codex() {
        let mut sticky = Sticky::default();
        let session = OpenSession {
            provider: Source::Codex,
            ..live("t", "Codex support", Some("feat/x"), None)
        };
        sticky.observe(&[session], 5);
        assert_eq!(sticky.entries[0].provider, Source::Codex);
    }

    #[test]
    fn lists_saved_before_base_prs_were_kept_still_load() {
        let json = r#"[{"session_id":"a","name":"n","project":"p","cwd":"/w","branch":null,"pr_number":3,"last_seen_ms":1}]"#;
        assert_eq!(Sticky::from_json(json).entries[0].base_pr, None);
    }

    #[test]
    fn closed_times_are_in_their_largest_unit() {
        assert_eq!(closed_ago(0, 30_000), "closed 0m");
        assert_eq!(closed_ago(0, 59 * MIN), "closed 59m");
        assert_eq!(closed_ago(0, 3 * 60 * MIN), "closed 3h");
        assert_eq!(closed_ago(0, 2 * DAY + MIN), "closed 2d");
    }
}
