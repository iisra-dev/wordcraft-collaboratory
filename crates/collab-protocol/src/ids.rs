//! Stable identifiers (design §5.1).
//!
//! WordCraft's `Pos`/`Path` locate a paragraph in one particular document structure; they shift
//! when blocks are inserted or removed, so they cannot name a paragraph across clients. A
//! [`BlockId`] names a paragraph for its whole life; `collab-model` keeps the mapping to paths.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::ProtocolError;

/// A paragraph for its whole life, independent of where it currently sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockId(pub Uuid);

/// A shared document on the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(pub Uuid);

/// One submitted operation. Unique per document: resubmitting the same id after a reconnect
/// returns the original commit instead of applying it twice (design §7, idempotence).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub Uuid);

/// One connected client installation (e.g. `desktop-a`). Chosen by the client, so validated.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientId(String);

impl ClientId {
    /// Longest accepted client id, in bytes.
    pub const MAX_LEN: usize = 64;

    /// A client id: 1–[`Self::MAX_LEN`] bytes of ASCII letters, digits, `-`, `_` or `.`.
    pub fn new(id: impl Into<String>) -> Result<Self, ProtocolError> {
        let id = id.into();
        let valid = !id.is_empty() && id.len() <= Self::MAX_LEN && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if valid { Ok(Self(id)) } else { Err(ProtocolError::InvalidClientId) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientId {
    type Error = ProtocolError;
    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl From<ClientId> for String {
    fn from(id: ClientId) -> Self {
        id.0
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Position in a document's authoritative sequence of commits. Revision 0 is the document as
/// created or imported; every commit adds exactly one.
pub type Revision = u64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_rejects_hostile_input() {
        assert!(ClientId::new("desktop-a").is_ok());
        assert!(ClientId::new("").is_err());
        assert!(ClientId::new("a b").is_err());
        assert!(ClientId::new("ñ").is_err());
        assert!(ClientId::new("x".repeat(ClientId::MAX_LEN + 1)).is_err());
        assert!(serde_json::from_str::<ClientId>("\"bad id\"").is_err());
    }
}
