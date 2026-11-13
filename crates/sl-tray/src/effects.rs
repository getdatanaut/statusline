//! Every effect the tray has on the world outside its own windows, in one
//! place: opening a URL or a worktree, the clipboard, the GitHub actions, the
//! confirm dialog, popups, notifications and `caffeinate`.
//!
//! In normal running nothing here changes anything: each effect's own
//! function asks [`intercept`] first, gets `false`, and goes on to do the
//! real thing. Under `--self-test` a recorder is installed instead, and each
//! effect is written down rather than performed -- so a test can click every
//! control the tray has and see what it would have done, without a browser
//! opening, the clipboard changing or a CI job being re-run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// One effect, as the recorder writes it down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    OpenUrl(String),
    OpenInZed(String),
    Copy(String),
    CopyReview {
        cwd: String,
        pr: i64,
    },
    OpenReviewInZed {
        cwd: String,
        pr: i64,
    },
    ShowLogs {
        cwd: String,
        job: String,
        link: Option<String>,
    },
    RetryJob {
        cwd: String,
        job: String,
        link: Option<String>,
    },
    Confirm(String),
    Popup(String),
    Notify {
        title: String,
        body: String,
    },
    Caffeinate(bool),
    /// Storing a GitHub token or Linear key (never shown), or removing it.
    SaveSecret(crate::integrations::Service),
    RemoveSecret(crate::integrations::Service),
    Quit,
}

impl std::fmt::Display for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::OpenUrl(url) => write!(f, "open {url}"),
            Effect::OpenInZed(path) => write!(f, "zed {path}"),
            Effect::Copy(text) => write!(f, "copy {text:?}"),
            Effect::CopyReview { cwd, pr } => write!(f, "copy-review #{pr} in {cwd}"),
            Effect::OpenReviewInZed { cwd, pr } => write!(f, "review-in-zed #{pr} in {cwd}"),
            Effect::ShowLogs { cwd, job, link } => write!(
                f,
                "logs {job:?} in {cwd} ({})",
                link.as_deref().unwrap_or("no link")
            ),
            Effect::RetryJob { cwd, job, link } => write!(
                f,
                "rerun {job:?} in {cwd} ({})",
                link.as_deref().unwrap_or("no link")
            ),
            Effect::Confirm(message) => write!(f, "confirm {message:?}"),
            Effect::Popup(title) => write!(f, "popup {title:?}"),
            Effect::Notify { title, body } => write!(f, "notify {title:?}: {body:?}"),
            Effect::Caffeinate(on) => write!(f, "caffeinate {}", if *on { "on" } else { "off" }),
            Effect::SaveSecret(service) => {
                write!(f, "save {} {}", service.name(), service.secret())
            }
            Effect::RemoveSecret(service) => {
                write!(f, "remove {} {}", service.name(), service.secret())
            }
            Effect::Quit => write!(f, "quit"),
        }
    }
}

struct Recorder {
    log: Mutex<Vec<Effect>>,
    /// What a confirm dialog answers.
    confirm: AtomicBool,
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// From now on, write every effect down instead of performing it, and answer
/// every confirm dialog with `confirm`. For the self-test; there is no way back.
/// GitHub and Linear go offline too: the Keychain and curl are at fixed
/// paths the self-test's stubbed `PATH` cannot catch.
pub(crate) fn record(confirm: bool) {
    sl_core::gh::api::go_offline();
    let recorder = RECORDER.get_or_init(|| Recorder {
        log: Mutex::new(Vec::new()),
        confirm: AtomicBool::new(confirm),
    });
    recorder.confirm.store(confirm, Ordering::Relaxed);
}

/// Called by each effect before it acts: `true` if it was recorded, and so
/// must not be performed.
pub(crate) fn intercept(effect: Effect) -> bool {
    let Some(recorder) = RECORDER.get() else {
        return false;
    };
    recorder
        .log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(effect);
    true
}

/// A confirm dialog's scripted answer while recording (and the dialog
/// itself recorded); `None` when the real dialog should be shown.
pub(crate) fn scripted_confirm(message: &str) -> Option<bool> {
    let recorder = RECORDER.get()?;
    intercept(Effect::Confirm(message.to_owned()));
    Some(recorder.confirm.load(Ordering::Relaxed))
}

/// Everything recorded so far, oldest first, leaving the log empty.
pub(crate) fn take() -> Vec<Effect> {
    RECORDER
        .get()
        .map(|recorder| {
            std::mem::take(
                &mut *recorder
                    .log
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effects_read_as_one_line_each() {
        assert_eq!(
            Effect::Copy("claude --resume x".into()).to_string(),
            "copy \"claude --resume x\""
        );
        assert_eq!(
            Effect::RetryJob {
                cwd: "/x".into(),
                job: "t".into(),
                link: None
            }
            .to_string(),
            "rerun \"t\" in /x (no link)"
        );
        assert_eq!(Effect::Caffeinate(true).to_string(), "caffeinate on");
    }
}
