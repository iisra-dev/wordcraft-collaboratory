//! Protocol errors and rejection reasons.

use serde::{Deserialize, Serialize};

/// Why the server refused a submission. Sent to the client in a `rejected` message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// No tested transformation exists against a concurrent commit (design §5.3, safety rule).
    Conflict,
    /// The operation does not fit the document (unknown block, offset out of range or not on a
    /// char boundary…).
    InvalidOperation,
    /// The operation kind is not supported on shared documents yet.
    UnsupportedOperation,
    /// The base revision is newer than the server's or older than the history it keeps.
    StaleRevision,
    /// Not signed in, or the token expired.
    Unauthenticated,
    /// Signed in but not allowed (a viewer editing, a non-member joining).
    Forbidden,
    /// Message or document over the size limits.
    TooLarge,
    /// The peer speaks another protocol version.
    UnsupportedVersion,
    /// The server could not complete the request (e.g. storage failed). Nothing changed; retry.
    Unavailable,
    /// Too many messages, documents or connections for this connection or user (C5.4).
    /// Nothing changed; slow down or close something.
    RateLimited,
}

/// A message that could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message is {size} bytes, over the {max}-byte limit")]
    TooLarge { size: usize, max: usize },
    #[error("protocol version {got} is not supported (expected {expected})")]
    UnsupportedVersion { got: u32, expected: u32 },
    #[error("client id must be 1–64 ASCII letters, digits, '-', '_' or '.'")]
    InvalidClientId,
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
}

impl ProtocolError {
    /// The rejection to report back to the peer.
    pub fn reason(&self) -> RejectReason {
        match self {
            Self::TooLarge { .. } => RejectReason::TooLarge,
            Self::UnsupportedVersion { .. } => RejectReason::UnsupportedVersion,
            Self::InvalidClientId | Self::Malformed(_) => RejectReason::InvalidOperation,
        }
    }
}
