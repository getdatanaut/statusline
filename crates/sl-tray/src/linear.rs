//! The Linear ticket behind a session's branch, if there is one.
//!
//! Asked of the `linear` command line tool (`linear issue view --json`), which
//! is signed in on its own: the tray holds no key and talks to no API itself.
//! A branch named for a ticket (`jakub/dn-479-ontology-m4`) names it; any
//! other branch has no ticket, and a machine without the tool shows nothing.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::sessions::Tone;

/// A ticket's lookup is abandoned after this long: it runs on the sessions
/// thread, which must not wait on a stuck process.
const TIMEOUT: Duration = Duration::from_secs(10);
/// A ticket that could not be found is asked about this many times less often
/// than one that could.
const MISSING_SLOWDOWN: i64 = 5;

/// What the tray shows of a ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearInfo {
    /// `DN-479`.
    pub id: String,
    pub url: String,
    /// The workflow state's name: `In Progress`, `In Review`...
    pub state: String,
}

/// First words of branch names that look like `word-123` but are not teams.
const NOT_TEAMS: [&str; 9] = [
    "fix", "feat", "feature", "chore", "bug", "hotfix", "release", "pr", "v",
];

/// The ticket a branch is named for: `DN-479` from `jakub/dn-479-ontology-m4`.
/// The team key and a number open the last path segment, after any `user/`.
pub fn ticket_id(branch: &str) -> Option<String> {
    let segment = branch.rsplit('/').next()?;
    let (team, rest) = segment.split_once('-')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest.get(digits.len()..).unwrap_or("");
    let team_ok = (1..=10).contains(&team.len())
        && team.chars().all(|c| c.is_ascii_alphanumeric())
        && team.starts_with(|c: char| c.is_ascii_alphabetic());
    let ends_ok = after.is_empty() || after.starts_with('-');
    if !team_ok
        || digits.is_empty()
        || digits.len() > 6
        || !ends_ok
        || NOT_TEAMS.contains(&team.to_ascii_lowercase().as_str())
    {
        return None;
    }
    Some(format!("{}-{digits}", team.to_ascii_uppercase()))
}

/// The ticket from `linear issue view --json`; `None` if it is not one.
fn parse(output: &str) -> Option<LinearInfo> {
    // Anything before the JSON (a banner, a warning) is skipped.
    let json = output.get(output.find('{')?..)?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(LinearInfo {
        id: value.get("identifier")?.as_str()?.to_owned(),
        url: value.get("url")?.as_str()?.to_owned(),
        state: value.get("state")?.get("name")?.as_str()?.to_owned(),
    })
}

/// The colour a state is shown in: calm to start, warm in flight, green when done.
pub fn state_tone(state: &str) -> Tone {
    let state = state.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| state.contains(word));
    if has(&["done", "complete", "shipped", "released"]) {
        Tone::Green
    } else if has(&["cancel", "duplicate", "backlog"]) {
        Tone::Dim
    } else if has(&["review", "qa", "testing", "progress", "started", "doing"]) {
        // Orange, not yellow: yellow text is faint on a light menu.
        Tone::Orange
    } else {
        Tone::Plain
    }
}

/// Ask Linear about ticket `id`, from `cwd` so a project's own
/// `.linear.toml` picks its workspace. Takes up to [`TIMEOUT`].
pub fn fetch(id: &str, cwd: &str) -> Option<LinearInfo> {
    let output = crate::actions::run_for(
        "linear",
        &[
            "issue",
            "view",
            id,
            "--json",
            "--no-comments",
            "--no-download",
        ],
        cwd,
        TIMEOUT,
    )
    .ok()?;
    parse(&output)
}

/// Tickets already looked up, each reused for a while.
#[derive(Default)]
pub struct LinearCache {
    /// Ticket id to when it was asked about and what came back.
    entries: HashMap<String, (i64, Option<LinearInfo>)>,
}

impl LinearCache {
    /// What is known of the ticket `branch` is named for, however old: shown
    /// at once while a fresh answer is asked for.
    pub fn cached(&self, branch: &str) -> Option<LinearInfo> {
        self.entries
            .get(&ticket_id(branch)?)
            .and_then(|(_, info)| info.clone())
    }

    /// Whether ticket `id` is to be asked of Linear again: never asked, or
    /// the last answer is older than `max_age_ms` (five times that for a miss).
    pub fn due(&self, id: &str, now_ms: i64, max_age_ms: i64) -> bool {
        self.entries.get(id).is_none_or(|(at, info)| {
            now_ms - at
                >= if info.is_some() {
                    max_age_ms
                } else {
                    max_age_ms * MISSING_SLOWDOWN
                }
        })
    }

    /// Keep what Linear said of `id`. A failed lookup (`None`) keeps what
    /// was known: a brief outage is not a ticket vanishing.
    pub fn store(&mut self, id: String, now_ms: i64, fetched: Option<LinearInfo>) {
        let info = fetched.or_else(|| self.entries.get(&id).and_then(|(_, info)| info.clone()));
        self.entries.insert(id, (now_ms, info));
    }

    /// Forget tickets no open session is on.
    pub fn retain(&mut self, open: &HashSet<String>) {
        self.entries.retain(|id, _| open.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_named_by_the_branchs_last_segment() {
        assert_eq!(
            ticket_id("jakub/dn-479-ontology-m4-base").as_deref(),
            Some("DN-479")
        );
        assert_eq!(ticket_id("DN-12").as_deref(), Some("DN-12"));
        assert_eq!(ticket_id("feature/eng-7-thing").as_deref(), Some("ENG-7"));
    }

    #[test]
    fn branches_without_a_ticket_have_none() {
        for branch in [
            "main",
            "feat/tray-stats-window",
            "fix-123-typo",
            "jakub/dn-notes",
            "dn-12abc",
            "jakub/dn-1234567-x",
            "release/v-1",
            "-5",
        ] {
            assert_eq!(ticket_id(branch), None, "{branch}");
        }
    }

    #[test]
    fn a_ticket_is_read_from_the_tools_json() {
        let json = "{\"identifier\":\"DN-479\",\"title\":\"x\",\"url\":\"https://linear.app/datanaut/issue/DN-479/x\",\"state\":{\"name\":\"In Progress\",\"color\":\"#f2c94c\"}}";
        assert_eq!(
            parse(json),
            Some(LinearInfo {
                id: "DN-479".into(),
                url: "https://linear.app/datanaut/issue/DN-479/x".into(),
                state: "In Progress".into()
            })
        );
        assert_eq!(
            parse(&format!("warning\n{json}")),
            parse(json),
            "a banner before it is skipped"
        );
        assert_eq!(parse("✗ Failed to view issue"), None);
        assert_eq!(
            parse("{\"identifier\":\"DN-1\"}"),
            None,
            "a ticket needs its url and state"
        );
    }

    #[test]
    fn states_are_toned_by_where_they_are_in_the_flow() {
        assert_eq!(state_tone("In Progress"), Tone::Orange);
        assert_eq!(state_tone("In Review"), Tone::Orange);
        assert_eq!(state_tone("Done"), Tone::Green);
        assert_eq!(state_tone("Canceled"), Tone::Dim);
        assert_eq!(state_tone("Todo"), Tone::Plain);
    }
}
