//! Settings → Integrations: the GitHub token, stored in the Keychain.
//!
//! Saving checks the token with GitHub first (it must name a login), then
//! stores it; removing deletes it. Both run off the main thread, and say how
//! it went through [`github`], which the Settings window shows.
//!
//! While it holds a token GitHub accepts, the tray says so every minute in
//! the store ([`spawn_heartbeat`]): the status line then leaves GitHub to it.
//! A token GitHub starts refusing (expired, revoked) is reported once, by a
//! notification, and in Settings and the menu.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use sl_core::gh::{self, api::Failure};

/// GitHub's page for a new classic token, with the scopes the tray needs
/// (`repo`: PRs, reviews, CI, stacks, logs, re-runs; `read:org`: teams asked
/// to review) ticked and its note filled in.
pub(crate) const NEW_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo,read:org&description=Statusline%20Tray";

/// Where the GitHub token stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitHub {
    /// Not looked at yet.
    Unknown,
    /// Being read, checked or stored.
    Working,
    /// None stored.
    Missing,
    SignedIn(String),
    /// Stored, and refused by GitHub.
    Refused,
    /// Something else went wrong; the text says what.
    Failed(String),
}

impl GitHub {
    /// As the Settings window says it.
    pub(crate) fn text(&self) -> String {
        match self {
            GitHub::Unknown | GitHub::Working => "Checking…".to_owned(),
            GitHub::Missing => "No token stored".to_owned(),
            GitHub::SignedIn(login) => format!("Signed in as {login}"),
            GitHub::Refused => "GitHub refused the token: expired or revoked".to_owned(),
            GitHub::Failed(why) => why.clone(),
        }
    }
}

/// The GitHub token's standing, and a count that goes up with every change.
static GITHUB: Mutex<(u64, GitHub)> = Mutex::new((0, GitHub::Unknown));

/// Set the standing; the change count it gets.
fn set(state: GitHub) -> u64 {
    let mut github = GITHUB
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    github.0 += 1;
    github.1 = state;
    github.0
}

/// Set the standing a look begun at change `since` found, unless something
/// changed it meanwhile: a token saved while an older one was being checked
/// is not then reported refused.
fn settle(since: u64, state: GitHub) {
    let mut github = GITHUB
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if github.0 == since {
        github.0 += 1;
        github.1 = state;
    }
}

/// The GitHub token's standing, with its change count.
pub(crate) fn github() -> (u64, GitHub) {
    GITHUB
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn from_failure(failure: Failure) -> GitHub {
    match failure {
        Failure::Unauthorized => GitHub::Refused,
        Failure::NoToken(why) if why == gh::api::NO_TOKEN => GitHub::Missing,
        other => GitHub::Failed(other.to_string()),
    }
}

/// Look at the stored token again, and ask GitHub whose it is.
pub(crate) fn refresh() {
    let since = set(GitHub::Working);
    std::thread::spawn(move || {
        settle(
            since,
            match gh::api::login() {
                Ok(login) => GitHub::SignedIn(login),
                Err(failure) => from_failure(failure),
            },
        );
    });
}

/// How often the tray tells the status line it is asking GitHub; well under
/// [`gh::sync::TRAY_GONE_MS`].
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(60);

/// Every [`HEARTBEAT`], note in the store whether this tray can ask GitHub
/// (holds a token), so the status line asks by itself only when it cannot.
pub(crate) fn spawn_heartbeat() {
    std::thread::spawn(|| {
        loop {
            beat();
            std::thread::sleep(HEARTBEAT);
        }
    });
}

/// The refused token has been reported, so it is not again until a new one
/// is accepted.
static TOLD_REFUSED: AtomicBool = AtomicBool::new(false);

/// Branches neither asked about nor shown for this long are forgotten.
const FORGET_AFTER_MS: i64 = 30 * 24 * 60 * 60_000;

fn beat() {
    let refused = gh::api::token_refused();
    if refused && !TOLD_REFUSED.swap(true, Ordering::SeqCst) {
        set(GitHub::Refused);
        if std::env::var_os("SL_TRAY_NO_NOTIFY").is_none() {
            crate::notify::send(
                "GitHub token expired or revoked",
                "GitHub refused the tray's token. Replace it in Settings → Integrations.",
            );
        }
    } else if !refused {
        TOLD_REFUSED.store(false, Ordering::SeqCst);
    }
    let Some(store) = gh::Store::open_default() else {
        return;
    };
    let now_ms = crate::snapshot::now_ms();
    // A refused token asks nothing: the status line asks by itself meanwhile.
    if gh::api::can_ask() {
        store.mark_tray(now_ms);
    } else {
        store.clear_tray();
    }
    store.prune(now_ms - FORGET_AFTER_MS);
}

/// After the token changed: forget the old one, lift the holds failures
/// left, and ask about every session's PR again.
fn token_changed() {
    gh::api::token_replaced();
    if let Some(store) = gh::Store::open_default() {
        store.release_failed();
    }
    beat();
    crate::sessions::looked();
}

/// Check `token` with GitHub, and store it if GitHub knows whose it is.
pub(crate) fn save(token: &str) {
    if crate::effects::intercept(crate::effects::Effect::SaveToken) {
        return;
    }
    let token = token.trim().to_owned();
    if !crate::keychain::is_well_formed(&token) {
        set(GitHub::Failed(
            "That does not look like a GitHub token.".to_owned(),
        ));
        return;
    }
    let since = set(GitHub::Working);
    std::thread::spawn(move || {
        let state = match gh::api::login_of(&token) {
            Ok(login) => match crate::keychain::save(&token) {
                Ok(()) => {
                    token_changed();
                    GitHub::SignedIn(login)
                }
                Err(why) => GitHub::Failed(why),
            },
            Err(Failure::Unauthorized) => {
                GitHub::Failed("GitHub refused that token; nothing was stored.".to_owned())
            }
            Err(failure) => GitHub::Failed(failure.to_string()),
        };
        settle(since, state);
    });
}

/// Delete the stored token.
pub(crate) fn remove() {
    if crate::effects::intercept(crate::effects::Effect::RemoveToken) {
        return;
    }
    let since = set(GitHub::Working);
    std::thread::spawn(move || {
        settle(
            since,
            match crate::keychain::remove() {
                Ok(()) => {
                    token_changed();
                    GitHub::Missing
                }
                Err(why) => GitHub::Failed(why),
            },
        );
    });
}
