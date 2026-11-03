//! Authentication and authorisation (design §12).
//!
//! **Now (C2.3): local tokens.** For setups without an identity provider, `wordcraft-collab-server
//! create-user <name>` creates a user and prints a random bearer token once; only its SHA-256 is
//! stored. Clients send it as `Authorization: Bearer <token>` — in a header, never in the URL or
//! logs — and the server maps it to a user before upgrading to WebSocket.
//!
//! **Later (C5.1): OIDC.** The desktop client signs in with Authorization Code + PKCE against
//! Authentik (system browser, loopback callback with `state`), then sends a short-lived access
//! token the same way; the server validates issuer, audience, signature and expiry and maps the
//! subject to `users.oidc_subject`.
//!
//! Either way, the member's role (`owner`, `editor`, `viewer`) is checked by the document actor
//! on every operation, and by every HTTP endpoint, per document.

use crate::persistence::{Database, PersistenceError, Timestamp, User, UserId, repository};
use crate::storage::sha256_hex;

/// Prefix of local tokens, so they are recognisable in a leaked config or a secret scanner.
pub const TOKEN_PREFIX: &str = "wct_";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no random source: {0}")]
    Random(String),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// A fresh token: the prefix and 32 random bytes in hex.
pub fn generate_token() -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AuthError::Random(e.to_string()))?;
    Ok(format!("{TOKEN_PREFIX}{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// A fresh random id (UUID v4 layout) for users, documents and blocks created by the server.
pub fn new_uuid() -> Result<uuid::Uuid, AuthError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| AuthError::Random(e.to_string()))?;
    Ok(uuid::Builder::from_random_bytes(bytes).into_uuid())
}

/// Create a local user named `display_name` and a token for them. The token is returned once
/// and never stored.
pub fn create_local_user(db: &Database, display_name: &str) -> Result<(UserId, String), AuthError> {
    let id = UserId(new_uuid()?);
    let token = generate_token()?;
    let conn = db.lock()?;
    let now = Timestamp::now();
    repository::create_user(&conn, &User { id, oidc_subject: None, display_name: display_name.into() }, &now)?;
    repository::add_token(&conn, id, &sha256_hex(token.as_bytes()), &now)?;
    Ok((id, token))
}

/// The user an `Authorization` header value names, if it is `Bearer <a known token>`.
pub fn authenticate(db: &Database, authorization: Option<&str>) -> Result<Option<UserId>, PersistenceError> {
    let Some(token) = authorization.and_then(|h| h.strip_prefix("Bearer ")).map(str::trim) else { return Ok(None) };
    // Cheap shape check before hashing and querying: prefix, then 64 hex digits.
    let well_formed = token.strip_prefix(TOKEN_PREFIX).is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    if !well_formed {
        return Ok(None);
    }
    repository::user_by_token(&*db.lock()?, &sha256_hex(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_sign_in_and_nothing_else_does() {
        let db = Database::open_in_memory().unwrap();
        let (ana, token) = create_local_user(&db, "Ana").unwrap();
        assert!(token.starts_with(TOKEN_PREFIX) && token.len() == TOKEN_PREFIX.len() + 64);
        assert_eq!(authenticate(&db, Some(&format!("Bearer {token}"))).unwrap(), Some(ana));
        let other = generate_token().unwrap();
        assert_ne!(other, token);
        for header in
            [None, Some(""), Some(token.as_str()), Some("Bearer "), Some("Basic abc"), Some(&format!("Bearer {other}")), Some("Bearer wct_zz")]
        {
            assert_eq!(authenticate(&db, header).unwrap(), None, "{header:?}");
        }
        // Only the hash is stored.
        let stored: String = db.lock().unwrap().query_row("SELECT token_sha256 FROM user_tokens", [], |r| r.get(0)).unwrap();
        assert_ne!(stored, token);
        assert_eq!(stored, sha256_hex(token.as_bytes()));
    }

    #[test]
    fn new_ids_are_random_v4() {
        let (a, b) = (new_uuid().unwrap(), new_uuid().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.get_version_num(), 4);
    }
}
