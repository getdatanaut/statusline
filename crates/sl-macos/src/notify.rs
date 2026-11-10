//! Native notifications through `UNUserNotificationCenter`.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter,
};

#[derive(Debug)]
pub enum Error {
    /// No bundle identifier: `UNUserNotificationCenter` would abort the process.
    NoIdentity,
    /// The user declined (or has since turned off) notifications.
    Denied,
    Failed(String),
}

/// How long to wait on the permission prompt: a person has to click it.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// What went wrong, from an error pointer a completion handler was passed;
/// empty for none.
///
/// # Safety
///
/// `error` is null or points at a live `NSError`, as a framework callback
/// guarantees for the duration of the call.
unsafe fn describe(error: *mut NSError) -> String {
    // SAFETY: the caller guarantees a non-null `error` is a live `NSError`.
    NonNull::new(error).map_or_else(String::new, |e| unsafe { e.as_ref() }.localizedDescription().to_string())
}

/// Deliver a notification titled `title` saying `body`, under the request
/// identifier `id`. Blocks until the notification center answers, so call it
/// off the main thread.
///
/// # Errors
///
/// [`Error::NoIdentity`] for a process with no bundle identifier,
/// [`Error::Denied`] when notifications are turned off for the app, and
/// [`Error::Failed`] when the notification center says why it would not.
pub fn send(id: &str, title: &str, body: &str) -> Result<(), Error> {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return Err(Error::NoIdentity);
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();

    // Idempotent: prompts the first time only, and answers from the stored
    // choice after that.
    let (tx, rx) = mpsc::channel();
    let auth = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        // The receiver gave up waiting; nobody is left to tell.
        // SAFETY: `error` is the framework's, live for the callback.
        tx.send((granted.as_bool(), unsafe { describe(error) })).ok();
    });
    center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &auth);
    match rx.recv_timeout(PROMPT_TIMEOUT) {
        Ok((true, _)) => {}
        Ok((false, error)) if error.is_empty() => return Err(Error::Denied),
        Ok((false, error)) => return Err(Error::Failed(format!("authorization: {error}"))),
        Err(_) => return Err(Error::Failed("authorization: no answer to the permission request".to_owned())),
    }

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(id), &content, None);
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: as above.
        tx.send(unsafe { describe(error) }).ok();
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    match rx.recv_timeout(DELIVERY_TIMEOUT) {
        Ok(error) if error.is_empty() => Ok(()),
        Ok(error) => Err(Error::Failed(format!("delivery: {error}"))),
        Err(_) => Err(Error::Failed("no answer from the notification center".to_owned())),
    }
}
