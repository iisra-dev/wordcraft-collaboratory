//! Collaborative snapshots (design §9): the WordCraft document plus its block identity registry.
//!
//! This is the authoritative shared state. The server stores it (with a checksum, C2.4) and sends
//! it to a client that joins (`ServerMessage::Snapshot`); the client restores it before applying
//! any commit. DOCX is import/export only, never the source of truth.
//!
//! Snapshots come from disk and from the network, so [`SharedDocument::from_json`] treats them as
//! hostile: unknown format versions, unknown fields, a registry that does not match the body or
//! repeats an id are all refused, and the document is normalised exactly as the native JSON
//! opener does, so every party starts from the same bytes.

use serde::{Deserialize, Serialize};
use wordcraft_collab_protocol::{BlockId, DocumentOperation};
use wordcraft_doc::Document;

use crate::error::ModelError;
use crate::identity::BlockIdentityMap;

/// Version of the snapshot layout. Bumped on any incompatible change; old snapshots then need a
/// migration, never a silent reinterpretation.
pub const SNAPSHOT_FORMAT: u32 = 1;

/// A shared document: what every replica (server working copy, each client) holds.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedDocument {
    document: Document,
    ids: BlockIdentityMap,
}

/// The serialised layout, borrowed for writing.
#[derive(Serialize)]
struct SnapshotOut<'a> {
    format: u32,
    document: &'a Document,
    blocks: &'a [BlockId],
}

/// The serialised layout, owned for reading.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotIn {
    format: u32,
    document: Document,
    blocks: Vec<BlockId>,
}

/// Just the version, read first so another format is reported as such rather than as a parse
/// error.
#[derive(Deserialize)]
struct FormatOnly {
    format: u32,
}

impl SharedDocument {
    /// Start sharing `document` (created or imported): every body block gets a fresh id.
    pub fn new(mut document: Document, new_id: impl FnMut() -> BlockId) -> Result<Self, ModelError> {
        document.normalize_runs();
        let ids = BlockIdentityMap::assign(&document, new_id)?;
        Ok(Self { document, ids })
    }

    /// A shared document from a document and its registry kept elsewhere (an editor session and
    /// its bridge); the registry must match the document's body.
    pub fn from_parts(document: Document, ids: BlockIdentityMap) -> Result<Self, ModelError> {
        ids.check(&document)?;
        Ok(Self { document, ids })
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn ids(&self) -> &BlockIdentityMap {
        &self.ids
    }

    /// Give the document back (e.g. "disconnect and keep a local copy").
    pub fn into_document(self) -> Document {
        self.document
    }

    /// Apply a committed operation; on error nothing changes (see [`crate::apply`]).
    pub fn apply(&mut self, op: &DocumentOperation) -> Result<(), ModelError> {
        crate::apply::apply(&mut self.document, &mut self.ids, op)
    }

    /// The snapshot as a JSON value, for `ServerMessage::Snapshot`.
    pub fn to_json(&self) -> Result<serde_json::Value, ModelError> {
        Ok(serde_json::to_value(self.layout())?)
    }

    /// The snapshot as bytes, for storage.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ModelError> {
        Ok(serde_json::to_vec(&self.layout())?)
    }

    /// Restore a snapshot received in `ServerMessage::Snapshot`.
    pub fn from_json(value: serde_json::Value) -> Result<Self, ModelError> {
        let FormatOnly { format } = FormatOnly::deserialize(&value)?;
        check_format(format)?;
        Self::restore(serde_json::from_value(value)?)
    }

    /// Restore a stored snapshot.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        let FormatOnly { format } = serde_json::from_slice(bytes)?;
        check_format(format)?;
        Self::restore(serde_json::from_slice(bytes)?)
    }

    fn layout(&self) -> SnapshotOut<'_> {
        SnapshotOut { format: SNAPSHOT_FORMAT, document: &self.document, blocks: self.ids.ids() }
    }

    fn restore(snapshot: SnapshotIn) -> Result<Self, ModelError> {
        let SnapshotIn { format, mut document, blocks } = snapshot;
        check_format(format)?;
        document.normalize_runs();
        let ids = BlockIdentityMap::restore(&document, blocks)?;
        Ok(Self { document, ids })
    }
}

fn check_format(format: u32) -> Result<(), ModelError> {
    if format == SNAPSHOT_FORMAT { Ok(()) } else { Err(ModelError::SnapshotFormat { got: format, expected: SNAPSHOT_FORMAT }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use wordcraft_doc::{CharProps, Pos};

    fn counter() -> impl FnMut() -> BlockId {
        let mut n = 0u128;
        move || {
            n += 1;
            BlockId(Uuid::from_u128(n))
        }
    }

    /// Three paragraphs with multibyte text.
    fn sample() -> Document {
        let mut doc = Document::default();
        doc.insert_text(&Pos::body(0, 0), "Hola ñandú — 🦀 world", &CharProps::default()).unwrap();
        doc.split_paragraph(&Pos::body(0, 5)).unwrap();
        doc.split_paragraph(&Pos::body(1, 0)).unwrap();
        doc
    }

    #[test]
    fn snapshots_round_trip_through_json_and_bytes() {
        let shared = SharedDocument::new(sample(), counter()).unwrap();
        assert_eq!(shared.ids().len(), 3);
        let json = shared.to_json().unwrap();
        assert_eq!(json["format"], SNAPSHOT_FORMAT);
        let back = SharedDocument::from_json(json).unwrap();
        assert_eq!(back, shared);
        let back = SharedDocument::from_bytes(&shared.to_bytes().unwrap()).unwrap();
        assert_eq!(back, shared);
        assert_eq!(back.ids().ids(), shared.ids().ids());
    }

    #[test]
    fn hostile_snapshots_are_refused() {
        let shared = SharedDocument::new(sample(), counter()).unwrap();
        let good = shared.to_json().unwrap();

        let mut other_format = good.clone();
        other_format["format"] = 2.into();
        assert!(matches!(SharedDocument::from_json(other_format), Err(ModelError::SnapshotFormat { got: 2, .. })));

        let mut short = good.clone();
        short["blocks"].as_array_mut().unwrap().pop();
        assert!(matches!(SharedDocument::from_json(short), Err(ModelError::RegistryMismatch { ids: 2, blocks: 3 })));

        let mut repeated = good.clone();
        let first = repeated["blocks"][0].clone();
        repeated["blocks"][1] = first;
        assert!(matches!(SharedDocument::from_json(repeated), Err(ModelError::DuplicateBlock(_))));

        let mut extra = good.clone();
        extra["selection"] = 3.into();
        assert!(SharedDocument::from_json(extra).is_err());

        for junk in [serde_json::Value::Null, serde_json::json!([]), serde_json::json!({"format": 1}), serde_json::json!({"format": "1"})] {
            assert!(SharedDocument::from_json(junk.clone()).is_err(), "{junk}");
        }
        for junk in [&b""[..], b"{", b"\xff\xfe", br#"{"format":1,"document":{},"blocks":"x"}"#] {
            assert!(SharedDocument::from_bytes(junk).is_err());
        }
    }

    #[test]
    fn restored_runs_are_normalised_like_the_native_opener() {
        let shared = SharedDocument::new(sample(), counter()).unwrap();
        let mut json = shared.to_json().unwrap();
        // A file that lies about its run lengths is repaired on restore, not trusted.
        json["document"]["body"][0]["runs"][0]["len"] = 9999.into();
        let back = SharedDocument::from_json(json).unwrap();
        assert_eq!(back.document(), shared.document());
    }
}
