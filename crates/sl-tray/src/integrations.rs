//! Settings → Integrations: the GitHub token and the Linear API key, each
//! stored in its own Keychain item.
//!
//! Saving checks the secret with its service first (it must name an account),
//! then stores it; removing deletes it. Both run off the main thread, and say
//! how it went through [`standing`], which the Settings window shows.
//!
//! While it holds a token GitHub accepts, the tray says so every minute in
//! the store ([`spawn_heartbeat`]): the status line then leaves GitHub to it.
//! A secret its service starts refusing (expired, revoked) is reported once,
//! by a notification, and in Settings.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use sl_core::gh;

/// What Settings → Integrations has a section for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Service {
    GitHub,
    Linear,
}

/// Why an account could not be named.
enum Failure {
    /// Nothing is stored.
    Missing,
    /// The service refused what is stored.
    Refused,
    Other(String),
}

impl From<gh::api::Failure> for Failure {
    fn from(failure: gh::api::Failure) -> Self {
        match failure {
            gh::api::Failure::Unauthorized => Failure::Refused,
            gh::api::Failure::NoToken(why) if why == gh::api::NO_TOKEN => Failure::Missing,
            other => Failure::Other(other.to_string()),
        }
    }
}

impl From<crate::linear::Failure> for Failure {
    fn from(failure: crate::linear::Failure) -> Self {
        match failure {
            crate::linear::Failure::Refused => Failure::Refused,
            crate::linear::Failure::NoKey(why) if why == crate::linear::NO_KEY => Failure::Missing,
            other => Failure::Other(other.to_string()),
        }
    }
}

impl Service {
    const ALL: [Service; 2] = [Service::GitHub, Service::Linear];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Service::GitHub => "GitHub",
            Service::Linear => "Linear",
        }
    }

    /// What the service calls the secret.
    pub(crate) fn secret(self) -> &'static str {
        match self {
            Service::GitHub => "token",
            Service::Linear => "key",
        }
    }

    /// Where a new secret is made. GitHub's page comes with the scopes the
    /// tray needs (`repo`: PRs, reviews, CI, stacks, logs, re-runs;
    /// `read:org`: teams asked to review) ticked and its note filled in;
    /// Linear's cannot be filled in.
    pub(crate) fn new_secret_url(self) -> &'static str {
        match self {
            Service::GitHub => {
                "https://github.com/settings/tokens/new?scopes=repo,read:org&description=Statusline%20Tray"
            }
            Service::Linear => crate::linear::NEW_KEY_URL,
        }
    }

    fn item(self) -> crate::keychain::Item {
        match self {
            Service::GitHub => crate::keychain::GITHUB,
            Service::Linear => crate::keychain::LINEAR,
        }
    }

    fn index(self) -> usize {
        match self {
            Service::GitHub => 0,
            Service::Linear => 1,
        }
    }

    /// Whose `secret` is, asked of the service.
    fn account_of(self, secret: &str) -> Result<String, Failure> {
        Ok(match self {
            Service::GitHub => gh::api::login_of(secret)?,
            Service::Linear => crate::linear::account_of(secret)?,
        })
    }

    /// Whose the stored secret is.
    fn account(self) -> Result<String, Failure> {
        Ok(match self {
            Service::GitHub => gh::api::login()?,
            Service::Linear => crate::linear::account()?,
        })
    }

    /// After the secret changed: forget the old one, and ask again about
    /// everything it was asked about. For GitHub, lift the holds failures
    /// left too.
    fn changed(self) {
        match self {
            Service::GitHub => {
                gh::api::token_replaced();
                if let Some(store) = gh::Store::open_default() {
                    store.release_failed();
                }
                beat();
            }
            Service::Linear => crate::linear::key_replaced(),
        }
        crate::sessions::looked();
    }
}

/// Where a service's secret stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Not looked at yet.
    Unknown,
    /// Being read, checked or stored.
    Working,
    /// None stored.
    Missing,
    SignedIn(String),
    /// Stored, and refused by the service.
    Refused,
    /// Something else went wrong; the text says what.
    Failed(String),
}

impl Standing {
    /// As the Settings window says it, of `service`.
    pub(crate) fn text(&self, service: Service) -> String {
        match self {
            Standing::Unknown | Standing::Working => "Checking…".to_owned(),
            Standing::Missing => format!("No {} stored", service.secret()),
            Standing::SignedIn(account) => format!("Signed in as {account}"),
            Standing::Refused => format!(
                "{} refused the {}: expired or revoked",
                service.name(),
                service.secret()
            ),
            Standing::Failed(why) => why.clone(),
        }
    }
}

impl From<Failure> for Standing {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Missing => Standing::Missing,
            Failure::Refused => Standing::Refused,
            Failure::Other(why) => Standing::Failed(why),
        }
    }
}

/// Each service's standing, and a count that goes up with every change.
static STANDINGS: Mutex<[(u64, Standing); 2]> =
    Mutex::new([(0, Standing::Unknown), (0, Standing::Unknown)]);

fn standings() -> std::sync::MutexGuard<'static, [(u64, Standing); 2]> {
    STANDINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set `service`'s standing; the change count it gets.
fn set(service: Service, standing: Standing) -> u64 {
    let mut all = standings();
    let entry = &mut all[service.index()];
    entry.0 += 1;
    entry.1 = standing;
    entry.0
}

/// Set the standing a look begun at change `since` found, unless something
/// changed it meanwhile: a secret saved while an older one was being checked
/// is not then reported refused.
fn settle(service: Service, since: u64, standing: Standing) {
    let mut all = standings();
    let entry = &mut all[service.index()];
    if entry.0 == since {
        entry.0 += 1;
        entry.1 = standing;
    }
}

/// `service`'s standing, with its change count.
pub(crate) fn standing(service: Service) -> (u64, Standing) {
    standings()[service.index()].clone()
}

/// Look at every stored secret again, and ask each service whose it is.
pub(crate) fn refresh() {
    for service in Service::ALL {
        let since = set(service, Standing::Working);
        std::thread::spawn(move || {
            settle(
                service,
                since,
                service
                    .account()
                    .map_or_else(Standing::from, Standing::SignedIn),
            );
        });
    }
}

/// How often the tray tells the status line it is asking GitHub; well under
/// [`gh::sync::TRAY_GONE_MS`].
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(60);

/// Every [`HEARTBEAT`], note in the store whether this tray can ask GitHub
/// (holds a token), so the status line asks by itself only when it cannot;
/// and report a secret that has started to be refused.
pub(crate) fn spawn_heartbeat() {
    std::thread::spawn(|| {
        loop {
            beat();
            std::thread::sleep(HEARTBEAT);
        }
    });
}

/// Each service's refused secret has been reported, so it is not again until
/// a new one is accepted.
static TOLD_REFUSED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

/// Report `service`'s secret once when `refused` turns true.
fn tell_if_refused(service: Service, refused: bool) {
    let told = &TOLD_REFUSED[service.index()];
    if refused && !told.swap(true, Ordering::SeqCst) {
        set(service, Standing::Refused);
        let (name, secret) = (service.name(), service.secret());
        if std::env::var_os("SL_TRAY_NO_NOTIFY").is_none() {
            crate::notify::send(
                &format!("{name} {secret} expired or revoked"),
                &format!(
                    "{name} refused the tray's {secret}. Replace it in Settings → Integrations."
                ),
            );
        }
    } else if !refused {
        told.store(false, Ordering::SeqCst);
    }
}

/// Branches neither asked about nor shown for this long are forgotten.
const FORGET_AFTER_MS: i64 = 30 * 24 * 60 * 60_000;

fn beat() {
    tell_if_refused(Service::GitHub, gh::api::token_refused());
    tell_if_refused(Service::Linear, crate::linear::key_refused());
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

/// Check `secret` with `service`, and store it if the service knows whose
/// it is.
pub(crate) fn save(service: Service, secret: &str) {
    if crate::effects::intercept(crate::effects::Effect::SaveSecret(service)) {
        return;
    }
    let secret = secret.trim().to_owned();
    if !crate::keychain::is_well_formed(&secret) {
        set(
            service,
            Standing::Failed(format!(
                "That does not look like a {} {}.",
                service.name(),
                service.secret()
            )),
        );
        return;
    }
    let since = set(service, Standing::Working);
    std::thread::spawn(move || {
        let standing = match service.account_of(&secret) {
            Ok(account) => match service.item().save(&secret) {
                Ok(()) => {
                    service.changed();
                    Standing::SignedIn(account)
                }
                Err(why) => Standing::Failed(why),
            },
            Err(Failure::Refused) => Standing::Failed(format!(
                "{} refused that {}; nothing was stored.",
                service.name(),
                service.secret()
            )),
            Err(failure) => Standing::from(failure),
        };
        settle(service, since, standing);
    });
}

/// Delete `service`'s stored secret.
pub(crate) fn remove(service: Service) {
    if crate::effects::intercept(crate::effects::Effect::RemoveSecret(service)) {
        return;
    }
    let since = set(service, Standing::Working);
    std::thread::spawn(move || {
        settle(
            service,
            since,
            match service.item().remove() {
                Ok(()) => {
                    service.changed();
                    Standing::Missing
                }
                Err(why) => Standing::Failed(why),
            },
        );
    });
}
