//! The GitHub token, in a login Keychain item only this app may read.
//!
//! The item is made by the tray itself (the Security framework, not the
//! `security` tool), so the Keychain trusts the tray alone: any other program
//! asking for it, `security find-generic-password` included, is met with a
//! password prompt. The Keychain knows the tray by its code signature, which is
//! why `make install` signs with one fixed identity (`make signing-cert`):
//! an ad-hoc signed build would be a stranger to the item every time.

use security_framework::passwords;

/// The Keychain item: its service (the name Keychain Access shows) and account.
const SERVICE: &str = "com.jakub.statusline.github";
const ACCOUNT: &str = "token";
/// `errSecItemNotFound`.
const NOT_FOUND: i32 = -25_300;

fn describe(error: security_framework::base::Error) -> String {
    format!(
        "Keychain: {}",
        error
            .message()
            .unwrap_or_else(|| format!("error {}", error.code()))
    )
}

/// Whether `token` looks like a GitHub token: letters, digits and `_`, as
/// every kind GitHub issues is.
pub(crate) fn is_well_formed(token: &str) -> bool {
    (20..=255).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The token, `None` when none is stored. The tray's
/// [`sl_core::gh::api::TokenSource`].
///
/// # Errors
///
/// The Keychain failing for another reason than the item not being there.
pub(crate) fn read() -> Result<Option<String>, String> {
    if sl_core::gh::api::is_offline() {
        return Ok(None);
    }
    match passwords::get_generic_password(SERVICE, ACCOUNT) {
        Ok(bytes) => Ok(String::from_utf8(bytes)
            .ok()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())),
        Err(error) if error.code() == NOT_FOUND => Ok(None),
        Err(error) => Err(describe(error)),
    }
}

/// Store `token`, replacing any stored before. The old item is deleted
/// rather than updated, so the token always goes into an item this app made;
/// should the new one then fail to be stored, the old one is put back.
///
/// # Errors
///
/// A token that does not look like one, or the Keychain failing.
pub(crate) fn save(token: &str) -> Result<(), String> {
    if sl_core::gh::api::is_offline() {
        return Err("offline".to_owned());
    }
    if !is_well_formed(token) {
        return Err("That does not look like a GitHub token.".to_owned());
    }
    let old = read().ok().flatten();
    remove()?;
    passwords::set_generic_password(SERVICE, ACCOUNT, token.as_bytes()).map_err(|error| {
        let restored = old.is_some_and(|old| {
            passwords::set_generic_password(SERVICE, ACCOUNT, old.as_bytes()).is_ok()
        });
        let kept = if restored {
            " The token stored before is kept."
        } else {
            ""
        };
        format!("{}{kept}", describe(error))
    })
}

/// Delete the stored token, if there is one.
///
/// # Errors
///
/// The Keychain failing for another reason than there being nothing to delete.
pub(crate) fn remove() -> Result<(), String> {
    if sl_core::gh::api::is_offline() {
        return Err("offline".to_owned());
    }
    match passwords::delete_generic_password(SERVICE, ACCOUNT) {
        Err(error) if error.code() != NOT_FOUND => Err(describe(error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_token_shaped_text_is_stored() {
        assert!(is_well_formed("ghp_0123456789abcdefABCDEF0123456789abcd"));
        assert!(is_well_formed(
            "github_pat_11AAAAAAA0123456789_abcdefghijklmnopqrstuvwxyz"
        ));
        for bad in [
            "",
            "short_token",
            "ghp_0123456789abcdef 0123456789abcdef",
            "ghp_0123456789abcdef\n0123456789abcdef",
        ] {
            assert!(!is_well_formed(bad), "{bad:?}");
        }
    }
}
