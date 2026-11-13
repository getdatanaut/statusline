//! The GitHub token and the Linear API key, each in a login Keychain item
//! only this app may read.
//!
//! Each item is made by the tray itself (the Security framework, not the
//! `security` tool), so the Keychain trusts the tray alone: any other program
//! asking for it, `security find-generic-password` included, is met with a
//! password prompt. The Keychain knows the tray by its code signature, which is
//! why `make install` signs with one fixed identity (`make signing-cert`):
//! an ad-hoc signed build would be a stranger to the items every time.

use security_framework::passwords;

/// One Keychain item: its service (the name Keychain Access shows); the
/// account is always `token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Item {
    service: &'static str,
}

pub(crate) const GITHUB: Item = Item {
    service: "com.jakub.statusline.github",
};
pub(crate) const LINEAR: Item = Item {
    service: "com.jakub.statusline.linear",
};

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

/// Whether `secret` looks like a GitHub token or a Linear key: letters,
/// digits and `_`, as every kind either issues is.
pub(crate) fn is_well_formed(secret: &str) -> bool {
    (20..=255).contains(&secret.len())
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The GitHub token: the tray's [`sl_core::gh::api::TokenSource`].
///
/// # Errors
///
/// As [`Item::read`].
pub(crate) fn github_token() -> Result<Option<String>, String> {
    GITHUB.read()
}

impl Item {
    /// What is stored, `None` when nothing is.
    ///
    /// # Errors
    ///
    /// The Keychain failing for another reason than the item not being there.
    pub(crate) fn read(self) -> Result<Option<String>, String> {
        if sl_core::http::is_offline() {
            return Ok(None);
        }
        match passwords::get_generic_password(self.service, ACCOUNT) {
            Ok(bytes) => Ok(String::from_utf8(bytes)
                .ok()
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(describe(error)),
        }
    }

    /// Store `secret`, replacing what was stored before. The old item is
    /// deleted rather than updated, so the secret always goes into an item
    /// this app made; should the new one then fail to be stored, the old one
    /// is put back.
    ///
    /// # Errors
    ///
    /// Text that does not look like a token, or the Keychain failing.
    pub(crate) fn save(self, secret: &str) -> Result<(), String> {
        if sl_core::http::is_offline() {
            return Err("offline".to_owned());
        }
        if !is_well_formed(secret) {
            return Err("That does not look like a token.".to_owned());
        }
        let old = self.read().ok().flatten();
        self.remove()?;
        passwords::set_generic_password(self.service, ACCOUNT, secret.as_bytes()).map_err(|error| {
            let restored = old.is_some_and(|old| {
                passwords::set_generic_password(self.service, ACCOUNT, old.as_bytes()).is_ok()
            });
            let kept = if restored {
                " The one stored before is kept."
            } else {
                ""
            };
            format!("{}{kept}", describe(error))
        })
    }

    /// Delete what is stored, if anything is.
    ///
    /// # Errors
    ///
    /// The Keychain failing for another reason than there being nothing to
    /// delete.
    pub(crate) fn remove(self) -> Result<(), String> {
        if sl_core::http::is_offline() {
            return Err("offline".to_owned());
        }
        match passwords::delete_generic_password(self.service, ACCOUNT) {
            Err(error) if error.code() != NOT_FOUND => Err(describe(error)),
            _ => Ok(()),
        }
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
        assert!(is_well_formed(
            "lin_api_0123456789abcdefABCDEF0123456789abcdefAB"
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
