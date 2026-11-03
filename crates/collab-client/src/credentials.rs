//! The desktop's sign-in kept in the operating system's credential store (design §12,
//! milestone C5.1c): Keychain on macOS, Credential Manager on Windows, the Secret Service
//! (GNOME Keyring, KWallet) on Linux and the BSDs. One entry, holding the identity provider,
//! the client id and the refresh token, so WordCraft signs in again without the browser after a
//! restart. Access tokens are never stored: they are short-lived and renewed from it.
//!
//! Calls may block (a store can ask the user to unlock it): call them off the UI thread. Where
//! no store is available, signing in still works for the session; nothing is written elsewhere.

use serde::{Deserialize, Serialize};

const SERVICE: &str = "WordCraft Collaborative";
const ACCOUNT: &str = "oidc-sign-in";

/// What is kept.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSignIn {
    pub issuer: String,
    pub client_id: String,
    pub refresh_token: String,
}

impl std::fmt::Debug for SavedSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedSignIn").field("issuer", &self.issuer).field("client_id", &self.client_id).finish_non_exhaustive()
    }
}

impl SavedSignIn {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A stored value read back; anything else (another app's, damaged) is `None`.
    pub fn decode(text: &str) -> Option<Self> {
        let saved: Self = serde_json::from_str(text).ok()?;
        (!saved.issuer.is_empty() && !saved.client_id.is_empty() && !saved.refresh_token.is_empty()).then_some(saved)
    }
}

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| format!("the system credential store is not available: {e}"))
}

/// Keep `saved`, replacing what was there.
pub fn save(saved: &SavedSignIn) -> Result<(), String> {
    entry()?.set_password(&saved.encode()).map_err(|e| format!("saving the sign-in: {e}"))
}

/// The kept sign-in, if any.
pub fn load() -> Result<Option<SavedSignIn>, String> {
    match entry()?.get_password() {
        Ok(text) => Ok(SavedSignIn::decode(&text)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("reading the sign-in: {e}")),
    }
}

/// Sign out: forget the kept sign-in.
pub fn forget() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("removing the sign-in: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_sign_ins_round_trip_and_hide_their_token() {
        let saved = SavedSignIn {
            issuer: "https://auth.example/application/o/wc/".into(),
            client_id: "wordcraft".into(),
            refresh_token: "secret-refresh".into(),
        };
        assert_eq!(SavedSignIn::decode(&saved.encode()), Some(saved.clone()));
        assert!(!format!("{saved:?}").contains("secret"));
        for junk in ["", "{}", "null", r#"{"issuer":"x","client_id":"y","refresh_token":""}"#, "secret-refresh"] {
            assert_eq!(SavedSignIn::decode(junk), None, "{junk}");
        }
    }
}
