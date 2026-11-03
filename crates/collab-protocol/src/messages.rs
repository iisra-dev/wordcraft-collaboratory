//! Versioned WebSocket messages (design §7). JSON in the MVP, for easy debugging.
//!
//! Every message is an [`Envelope`]: `{"protocol_version": 1, "type": "submit", …fields}`.
//! Presence is not a document operation: it is never persisted and never triggers a save (§10).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::errors::{ProtocolError, RejectReason};
use crate::ids::{BlockId, ClientId, DocumentId, OperationId, Revision};
use crate::operations::DocumentOperation;
use crate::{MAX_MESSAGE_BYTES, PROTOCOL_VERSION};

/// A message with its protocol version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub message: T,
}

impl<T> Envelope<T> {
    /// A message in this build's protocol version.
    pub fn new(message: T) -> Self {
        Self { protocol_version: PROTOCOL_VERSION, message }
    }
}

impl<T: Serialize> Envelope<T> {
    pub fn encode(&self) -> Result<String, ProtocolError> {
        Ok(serde_json::to_string(self)?)
    }
}

impl<T: DeserializeOwned> Envelope<T> {
    /// Decode a text frame: size limit first, then the version, then the message itself, so a
    /// peer on another version gets `UnsupportedVersion` rather than a parse error.
    pub fn decode(text: &str) -> Result<Self, ProtocolError> {
        if text.len() > MAX_MESSAGE_BYTES {
            return Err(ProtocolError::TooLarge { size: text.len(), max: MAX_MESSAGE_BYTES });
        }
        #[derive(Deserialize)]
        struct VersionOnly {
            protocol_version: u32,
        }
        let VersionOnly { protocol_version } = serde_json::from_str(text)?;
        if protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion { got: protocol_version, expected: PROTOCOL_VERSION });
        }
        Ok(serde_json::from_str(text)?)
    }
}

/// A structural caret position: a paragraph and a UTF-8 byte offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Anchor {
    pub block_id: BlockId,
    pub offset: u32,
}

/// Client → server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Version negotiation; the first message on a connection.
    Hello { client_id: ClientId },
    /// Open a shared document; answered with a `snapshot`.
    Join { document_id: DocumentId },
    /// Propose an operation made against `base_revision`.
    Submit { document_id: DocumentId, operation_id: OperationId, client_id: ClientId, base_revision: Revision, operation: DocumentOperation },
    /// This client's caret, rate-limited by the sender.
    Presence { document_id: DocumentId, cursor: Option<Anchor> },
    /// After a reconnect: everything committed after `since_revision` (or a snapshot).
    Resync { document_id: DocumentId, since_revision: Revision },
}

/// Someone editing a document, as shown in the participants panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub client_id: ClientId,
    pub display_name: String,
    pub cursor: Option<Anchor>,
}

/// Server → client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// `hello` accepted.
    Welcome {},
    /// The full shared state at `revision`. `snapshot` is the WordCraft document plus the block
    /// identity registry, encoded by `collab-model`; opaque at this layer.
    Snapshot { document_id: DocumentId, revision: Revision, snapshot: serde_json::Value },
    /// An operation persisted as `revision`, sent to every participant including its author.
    Committed { document_id: DocumentId, revision: Revision, operation_id: OperationId, client_id: ClientId, operation: DocumentOperation },
    /// A submission (or the connection) was refused; the document is unchanged.
    Rejected { document_id: Option<DocumentId>, operation_id: Option<OperationId>, reason: RejectReason, message: String },
    /// Who is in the document and where their carets are.
    Presence { document_id: DocumentId, participants: Vec<Participant> },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from docs/collab/design.md §7 decodes as written.
    const SUBMIT_EXAMPLE: &str = r#"{
      "protocol_version": 1,
      "type": "submit",
      "document_id": "0192f7a0-0000-7000-8000-000000000001",
      "operation_id": "0192f7a1-0000-7000-8000-000000000002",
      "client_id": "desktop-a",
      "base_revision": 42,
      "operation": {
        "kind": "insert_text",
        "block_id": "0192f7a2-0000-7000-8000-000000000003",
        "offset": 6,
        "text": "collaborative "
      }
    }"#;

    #[test]
    fn design_example_round_trips() {
        let env = Envelope::<ClientMessage>::decode(SUBMIT_EXAMPLE).unwrap();
        let ClientMessage::Submit { base_revision, operation, .. } = &env.message else {
            panic!("not a submit: {env:?}");
        };
        assert_eq!(*base_revision, 42);
        assert!(matches!(operation, DocumentOperation::InsertText { offset: 6, text, .. } if text == "collaborative "));
        let again = Envelope::<ClientMessage>::decode(&env.encode().unwrap()).unwrap();
        assert_eq!(again, env);
    }

    #[test]
    fn other_versions_are_refused_before_parsing() {
        let err = Envelope::<ClientMessage>::decode(r#"{"protocol_version": 2, "type": "from_the_future"}"#).unwrap_err();
        assert_eq!(err.reason(), RejectReason::UnsupportedVersion);
    }

    #[test]
    fn oversized_and_malformed_messages_are_errors() {
        let big = format!(r#"{{"protocol_version":1,"type":"hello","client_id":"{}"}}"#, "a".repeat(MAX_MESSAGE_BYTES));
        assert_eq!(Envelope::<ClientMessage>::decode(&big).unwrap_err().reason(), RejectReason::TooLarge);
        for junk in ["", "null", "[]", r#"{"protocol_version":1}"#, r#"{"protocol_version":1,"type":"submit"}"#] {
            assert!(Envelope::<ClientMessage>::decode(junk).is_err(), "{junk}");
        }
    }

    #[test]
    fn unknown_operation_fields_are_refused() {
        let extra = SUBMIT_EXAMPLE.replace(r#""offset": 6,"#, r#""offset": 6, "selection": 3,"#);
        assert!(Envelope::<ClientMessage>::decode(&extra).is_err());
    }
}
