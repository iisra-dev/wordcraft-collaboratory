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

/// Whether the guest sessions use the system store (C6.5b); tests turn it off so they never
/// write to the store of the machine running them.
static GUEST_STORE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Keep guest sessions in memory only, for this process (tests).
pub fn disable_guest_store() {
    GUEST_STORE.store(false, std::sync::atomic::Ordering::Relaxed);
}

fn guest_store_on() -> Result<(), String> {
    if GUEST_STORE.load(std::sync::atomic::Ordering::Relaxed) { Ok(()) } else { Err("the system credential store is off".into()) }
}
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

/// The store entry of a guest session (C6.5b): one per server and document, since a guest
/// session only opens the document it was invited to.
fn guest_entry(server: &str, document: uuid::Uuid) -> Result<keyring::Entry, String> {
    guest_store_on()?;
    keyring::Entry::new(SERVICE, &format!("guest:{}:{document}", server.trim()))
        .map_err(|e| format!("the system credential store is not available: {e}"))
}

/// Keep a guest session token for `document` on `server`.
pub fn save_guest(server: &str, document: uuid::Uuid, token: &str) -> Result<(), String> {
    guest_entry(server, document)?.set_password(token).map_err(|e| format!("saving the guest session: {e}"))
}

/// The guest session kept for `document` on `server`, if any.
pub fn load_guest(server: &str, document: uuid::Uuid) -> Result<Option<String>, String> {
    match guest_entry(server, document)?.get_password() {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("reading the guest session: {e}")),
    }
}

/// The guest sessions are listed by link, not document, when a link is opened again: the
/// entry for the link's server remembers the last document joined through it.
fn last_guest_entry(server: &str) -> Result<keyring::Entry, String> {
    guest_store_on()?;
    keyring::Entry::new(SERVICE, &format!("guest-last:{}", server.trim())).map_err(|e| format!("the system credential store is not available: {e}"))
}

/// Remember that the guest session for `document` is the latest one on `server`.
pub fn save_last_guest(server: &str, document: uuid::Uuid) -> Result<(), String> {
    last_guest_entry(server)?.set_password(&document.to_string()).map_err(|e| format!("saving the guest session: {e}"))
}

/// The latest guest session kept for `server`: its document and token.
pub fn load_last_guest(server: &str) -> Result<Option<(uuid::Uuid, String)>, String> {
    let document = match last_guest_entry(server)?.get_password() {
        Ok(text) => match uuid::Uuid::parse_str(text.trim()) {
            Ok(d) => d,
            Err(_) => return Ok(None),
        },
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(e) => return Err(format!("reading the guest session: {e}")),
    };
    Ok(load_guest(server, document)?.map(|token| (document, token)))
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
