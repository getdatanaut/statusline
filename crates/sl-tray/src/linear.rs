//! The Linear ticket behind a session's branch, if there is one.
//!
//! Asked of Linear's GraphQL API with a personal API key the tray keeps in its
//! own Keychain item ([`crate::keychain::LINEAR`], set in Settings →
//! Integrations). A branch named for a ticket (`jakub/dn-479-ontology-m4`)
//! names it; any other branch has no ticket. Without a key nothing is shown.
//! One request asks about every ticket that is due, so however many sessions
//! are open, a Linear that hangs holds a pass up for [`TIMEOUT_SECS`] at most.
//!
//! One workspace only: a key belongs to one, and there is one key. Several
//! would take a key per workspace, each ticket sent to the workspace whose
//! teams include its team key (`DN`); not built, as none is needed yet.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use sl_core::branch::ticket_id;

use crate::sessions::Tone;

const API: &str = "https://api.linear.app/graphql";
/// Linear's page for personal API keys.
pub(crate) const NEW_KEY_URL: &str = "https://linear.app/settings/account/security";
/// A lookup is abandoned after this long: it runs on the sessions thread,
/// which must not wait on a stuck Linear.
const TIMEOUT_SECS: u64 = 10;
/// A ticket that could not be found is asked about this many times less often
/// than one that could.
const MISSING_SLOWDOWN: i64 = 5;
/// The most tickets one request asks about: more than are ever open at once.
const MAX_TICKETS: usize = 100;

/// Why Linear gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// No key stored, or it could not be read; the text says which.
    NoKey(String),
    /// Linear refused the key: revoked, or expired.
    Refused,
    /// The request failed or timed out, or Linear answered with an error.
    Request(String),
}

/// What [`Failure::NoKey`] says when no key is stored.
pub(crate) const NO_KEY: &str = "No Linear key. Add one in Settings → Integrations.";

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoKey(why) | Failure::Request(why) => write!(f, "{why}"),
            Failure::Refused => write!(
                f,
                "Linear refused the key. Replace it in Settings → Integrations."
            ),
        }
    }
}

/// What the last look at the key found.
#[derive(Debug, Clone)]
enum Read {
    NotYet,
    Key(String),
    /// None stored: not looked for again until one is.
    Missing,
    /// It could not be read (the Keychain locked, or its prompt denied):
    /// not tried again until the instant passes.
    Unreadable(String, Instant),
}

static KEY: Mutex<Read> = Mutex::new(Read::NotYet);
/// Linear refused the stored key at the last request made with it.
static REFUSED: AtomicBool = AtomicBool::new(false);

fn cached_key() -> std::sync::MutexGuard<'static, Read> {
    KEY.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The stored key, kept in memory once read; never written anywhere else.
/// The lock is held while it is read, so callers at once wait for one read
/// instead of each reading (and each prompting).
fn key() -> Result<String, Failure> {
    let mut cached = cached_key();
    match &*cached {
        Read::Key(key) => return Ok(key.clone()),
        Read::Missing => return Err(Failure::NoKey(NO_KEY.to_owned())),
        Read::Unreadable(why, until) if Instant::now() < *until => {
            return Err(Failure::NoKey(why.clone()));
        }
        Read::NotYet | Read::Unreadable(..) => {}
    }
    let (read, found) = match crate::keychain::LINEAR.read() {
        Ok(Some(key)) => (Read::Key(key.clone()), Ok(key)),
        Ok(None) => (Read::Missing, Err(Failure::NoKey(NO_KEY.to_owned()))),
        Err(why) => (
            Read::Unreadable(
                why.clone(),
                Instant::now() + sl_core::gh::api::RETRY_UNREADABLE,
            ),
            Err(Failure::NoKey(why)),
        ),
    };
    *cached = read;
    found
}

/// Forget `used` if it is still the key in memory: a request answered after
/// the key was replaced says nothing about the new one.
fn forget_refused(used: &str) -> bool {
    let mut cached = cached_key();
    let current = matches!(&*cached, Read::Key(key) if key == used);
    if current {
        *cached = Read::NotYet;
    }
    current
}

/// Whether Linear refused the stored key at the last request made with it;
/// cleared by the next it accepts, or by [`key_replaced`].
pub(crate) fn key_refused() -> bool {
    REFUSED.load(Ordering::SeqCst)
}

/// A new key was stored, or the old one removed: read it again, and forget
/// that the old one was refused.
pub(crate) fn key_replaced() {
    *cached_key() = Read::NotYet;
    REFUSED.store(false, Ordering::SeqCst);
    KEY_CHANGED.store(true, Ordering::SeqCst);
}

/// The key changed since the sessions thread last looked.
static KEY_CHANGED: AtomicBool = AtomicBool::new(false);

/// Whether the key changed since this was last called: the sessions thread
/// then drops the tickets it knew, so each is asked about again at once.
pub(crate) fn take_key_change() -> bool {
    KEY_CHANGED.swap(false, Ordering::SeqCst)
}

/// POST `query` (with `variables`) as `key`, returning the answer's `data`.
fn post(key: &str, query: &str, variables: &Value, secs: u64) -> Result<Value, Failure> {
    let body = json!({ "query": query, "variables": variables }).to_string();
    let response = sl_core::http::send(
        "POST",
        API,
        Some(&body),
        // A personal API key goes in bare: `Bearer` is for OAuth tokens.
        Some(&format!("Authorization: {key}")),
        &["User-Agent: statusline"],
        secs,
    )
    .map_err(Failure::Request)?;
    read_answer(response.status, &response.body)
}

/// The `data` of Linear's answer, `status` being its HTTP status.
fn read_answer(status: u16, body: &str) -> Result<Value, Failure> {
    let value: Value = serde_json::from_str(body).map_err(|_not_json| {
        Failure::Request(format!(
            "HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ))
    })?;
    let first_error = value.pointer("/errors/0");
    let code = first_error
        .and_then(|e| e.pointer("/extensions/code"))
        .and_then(Value::as_str);
    if status == 401 || code == Some("AUTHENTICATION_ERROR") {
        return Err(Failure::Refused);
    }
    match value.get("data") {
        Some(data) if !data.is_null() && (200..300).contains(&status) => Ok(data.clone()),
        _ => Err(Failure::Request(format!(
            "HTTP {status}: {}",
            first_error
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("no data in Linear's answer")
        ))),
    }
}

/// [`post`] with the stored key; a refused key is forgotten, so the next
/// request reads the Keychain again.
fn post_signed(query: &str, variables: &Value, secs: u64) -> Result<Value, Failure> {
    let key = key()?;
    let answer = post(&key, query, variables, secs);
    match &answer {
        // About the key in memory only: one replaced while this ran is not
        // the one Linear answered about.
        Err(Failure::Refused) => {
            if forget_refused(&key) {
                REFUSED.store(true, Ordering::SeqCst);
            }
        }
        Ok(_) => REFUSED.store(false, Ordering::SeqCst),
        Err(_) => {}
    }
    answer
}

const ACCOUNT_QUERY: &str = "query { viewer { name } organization { name } }";

/// `Name (Workspace)` from the account query's data.
fn parse_account(data: &Value) -> Option<String> {
    let name = data.pointer("/viewer/name")?.as_str()?;
    let workspace = data.pointer("/organization/name")?.as_str()?;
    Some(format!("{name} ({workspace})"))
}

/// Whose `key` is, and in which workspace: asked before it is stored.
///
/// # Errors
///
/// A refused key, or Linear not answering.
pub(crate) fn account_of(key: &str) -> Result<String, Failure> {
    let data = post(key, ACCOUNT_QUERY, &json!({}), TIMEOUT_SECS)?;
    parse_account(&data).ok_or_else(|| Failure::Request("no account in Linear's answer".to_owned()))
}

/// Whose the stored key is.
///
/// # Errors
///
/// As [`account_of`], or no key.
pub(crate) fn account() -> Result<String, Failure> {
    let data = post_signed(ACCOUNT_QUERY, &json!({}), TIMEOUT_SECS)?;
    parse_account(&data).ok_or_else(|| Failure::Request("no account in Linear's answer".to_owned()))
}

/// What the tray shows of a ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearInfo {
    /// `DN-479`.
    pub id: String,
    pub url: String,
    /// The workflow state's name: `In Progress`, `In Review`...
    pub state: String,
}

/// Every ticket in `ids`, by identifier, in one request. Linear's filter
/// takes identifiers (`DN-479`) as ids, and leaves out those it has not got,
/// where `issue(id:)` would fail the whole answer for one missing ticket.
const TICKETS_QUERY: &str = "query($ids: [ID!]) { issues(first: 100, filter: { id: { in: $ids } }) \
    { nodes { identifier url state { name } } } }";

/// The tickets in the tickets query's data.
fn parse_tickets(data: &Value) -> Vec<LinearInfo> {
    data.pointer("/issues/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some(LinearInfo {
                id: node.get("identifier")?.as_str()?.to_owned(),
                url: node.get("url")?.as_str()?.to_owned(),
                state: node.pointer("/state/name")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Whether Linear takes `id` as an identifier: a team key of at most seven
/// characters and a number without a leading zero. It refuses the whole
/// request over one it does not take (`ABCDEFGH-1`, `DN-01`), so those are
/// never sent; a branch can name one, as [`ticket_id`] is more lenient.
fn linear_takes(id: &str) -> bool {
    id.split_once('-').is_some_and(|(team, number)| {
        (1..=7).contains(&team.len()) && (number == "0" || !number.starts_with('0'))
    })
}

/// Ask Linear about tickets `ids` (`DN-479`), all in one request. A ticket
/// Linear has not got (or this key's workspace has not) is left out, as is
/// one it would not take.
///
/// # Errors
///
/// No key, a refused one, or Linear not answering.
pub(crate) fn fetch(ids: &[String]) -> Result<HashMap<String, LinearInfo>, Failure> {
    let ids: Vec<&String> = ids
        .iter()
        .filter(|id| linear_takes(id))
        .take(MAX_TICKETS)
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let data = post_signed(TICKETS_QUERY, &json!({ "ids": ids }), TIMEOUT_SECS)?;
    Ok(parse_tickets(&data)
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect())
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

/// What one lookup of a ticket came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(LinearInfo),
    /// Linear answered, without it: no such ticket (in this workspace).
    Missing,
    /// The request failed (no key, a refused one, Linear not answering):
    /// nothing was learned about it.
    Failed,
}

/// Tickets already looked up, each reused for a while.
#[derive(Default)]
pub struct LinearCache {
    /// Ticket id to when it was asked about, what is known of it, and
    /// whether Linear last said it has no such ticket.
    entries: HashMap<String, (i64, Option<LinearInfo>, bool)>,
}

impl LinearCache {
    /// What is known of the ticket `branch` is named for, however old: shown
    /// at once while a fresh answer is asked for.
    pub fn cached(&self, branch: &str) -> Option<LinearInfo> {
        self.entries
            .get(&ticket_id(branch)?)
            .and_then(|(_, info, _)| info.clone())
    }

    /// Whether ticket `id` is to be asked of Linear again: never asked, or
    /// the last ask is older than `max_age_ms` (five times that for a ticket
    /// Linear said it has not got).
    pub fn due(&self, id: &str, now_ms: i64, max_age_ms: i64) -> bool {
        self.entries.get(id).is_none_or(|(at, _, missing)| {
            now_ms - at
                >= if *missing {
                    max_age_ms * MISSING_SLOWDOWN
                } else {
                    max_age_ms
                }
        })
    }

    /// Keep what a lookup of `id` came to. A failed one keeps what was
    /// known (a brief outage is not a ticket vanishing) and is asked again
    /// as soon as any ticket would be, not as late as a missing one.
    pub fn store(&mut self, id: String, now_ms: i64, lookup: Lookup) {
        let known = self.entries.get(&id);
        let entry = match lookup {
            Lookup::Found(info) => (now_ms, Some(info), false),
            Lookup::Missing => (now_ms, None, true),
            Lookup::Failed => (
                now_ms,
                known.and_then(|(_, info, _)| info.clone()),
                known.is_some_and(|(_, _, missing)| *missing),
            ),
        };
        self.entries.insert(id, entry);
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
    fn tickets_are_read_from_linears_answer() {
        let body = r#"{"data":{"issues":{"nodes":[
            {"identifier":"DN-479","url":"https://linear.app/datanaut/issue/DN-479/x","state":{"name":"In Progress"}},
            {"identifier":"DN-1","url":"https://linear.app/datanaut/issue/DN-1/y"}
        ]}}}"#;
        let data = read_answer(200, body).unwrap();
        assert_eq!(
            parse_tickets(&data),
            vec![LinearInfo {
                id: "DN-479".into(),
                url: "https://linear.app/datanaut/issue/DN-479/x".into(),
                state: "In Progress".into()
            }],
            "a ticket needs its url and state"
        );
        let none = read_answer(200, r#"{"data":{"issues":{"nodes":[]}}}"#).unwrap();
        assert!(parse_tickets(&none).is_empty());
    }

    #[test]
    fn a_refused_key_is_told_from_other_failures() {
        let refused = r#"{"errors":[{"message":"Authentication required, not authenticated","extensions":{"code":"AUTHENTICATION_ERROR"}}]}"#;
        assert_eq!(read_answer(401, refused), Err(Failure::Refused));
        assert_eq!(read_answer(400, refused), Err(Failure::Refused));
        assert_eq!(
            read_answer(
                400,
                r#"{"errors":[{"message":"Argument Validation Error"}],"data":null}"#
            ),
            Err(Failure::Request(
                "HTTP 400: Argument Validation Error".into()
            ))
        );
        assert_eq!(
            read_answer(502, "<html>bad gateway</html>"),
            Err(Failure::Request(
                "HTTP 502: <html>bad gateway</html>".into()
            ))
        );
    }

    #[test]
    fn only_ids_linear_takes_are_sent() {
        // As Linear's filter answered, 2026-11-13.
        for taken in [
            "DN-479",
            "ABCDEFG-1",
            "A1B-12",
            "FIX2-1",
            "X-0",
            "Q-1234567",
        ] {
            assert!(linear_takes(taken), "{taken}");
        }
        for refused in ["ABCDEFGH-1", "ABCDEFGHIJ-1", "ZZ-01", "ZZ-000001"] {
            assert!(!linear_takes(refused), "{refused}");
        }
    }

    #[test]
    #[ignore = "asks Linear"]
    fn linear_refuses_a_key_it_never_issued() {
        assert_eq!(
            account_of("lin_api_0000000000000000000000000000000000000000"),
            Err(Failure::Refused)
        );
    }

    #[test]
    fn the_account_names_the_person_and_the_workspace() {
        let data = read_answer(
            200,
            r#"{"data":{"viewer":{"name":"Ada"},"organization":{"name":"Datanaut"}}}"#,
        )
        .unwrap();
        assert_eq!(parse_account(&data).as_deref(), Some("Ada (Datanaut)"));
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
