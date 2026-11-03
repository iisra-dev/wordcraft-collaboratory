//! Stable block identities (design §5.1).
//!
//! A registry kept *beside* the document (persisted in collaborative snapshots) so the WordCraft
//! file format stays unchanged. It is updated explicitly by every insert, split, merge and delete:
//! never rebuilt from indexes alone, because a paragraph can move.
//!
//! MVP scope: top-level blocks of the body story, in document order. Tables, headers, footers and
//! notes are not shared-editable yet (design §1, MVP restrictions); their blocks still get an id
//! so the registry stays aligned with the body.

use std::collections::HashSet;

use wordcraft_collab_protocol::BlockId;
use wordcraft_doc::{Document, Path};

use crate::error::ModelError;

/// The ids of the body's top-level blocks, in document order.
///
/// Deliberately not `Deserialize`: a registry read from outside goes through
/// [`BlockIdentityMap::restore`], which checks it against its document (see `snapshot`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockIdentityMap {
    body: Vec<BlockId>,
}

impl BlockIdentityMap {
    /// Give every body block of `doc` a fresh id. Id generation is injected (UUIDv7 in the
    /// binaries, counters in tests) so this crate needs no random number source.
    pub fn assign(doc: &Document, new_id: impl FnMut() -> BlockId) -> Result<Self, ModelError> {
        Self::from_ids(doc, std::iter::repeat_with(new_id).take(doc.body.len()).collect())
    }

    /// A registry restored from a snapshot; it must match the document it was saved with.
    pub fn restore(doc: &Document, body: Vec<BlockId>) -> Result<Self, ModelError> {
        Self::from_ids(doc, body)
    }

    /// `body` as the registry of `doc`: one distinct id per body block. Duplicates are found
    /// with a set, so a hostile snapshot with many blocks cannot make this quadratic.
    fn from_ids(doc: &Document, body: Vec<BlockId>) -> Result<Self, ModelError> {
        let mut seen = HashSet::with_capacity(body.len());
        if let Some(dup) = body.iter().find(|id| !seen.insert(**id)) {
            return Err(ModelError::DuplicateBlock(*dup));
        }
        let map = Self { body };
        map.check(doc)?;
        Ok(map)
    }

    /// Fails if the registry and the document disagree on the number of body blocks.
    pub fn check(&self, doc: &Document) -> Result<(), ModelError> {
        if self.body.len() == doc.body.len() { Ok(()) } else { Err(ModelError::RegistryMismatch { ids: self.body.len(), blocks: doc.body.len() }) }
    }

    pub fn len(&self) -> usize {
        self.body.len()
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// Where the block currently is in the body.
    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        self.body.iter().position(|b| *b == id)
    }

    /// The WordCraft path of the block.
    pub fn path_of(&self, id: BlockId) -> Result<Path, ModelError> {
        self.index_of(id).map(Path::top).ok_or(ModelError::UnknownBlock(id))
    }

    /// The block at a body index.
    pub fn id_at(&self, index: usize) -> Option<BlockId> {
        self.body.get(index).copied()
    }

    /// Register a new block at `index` (blocks from `index` on move down one).
    pub fn insert(&mut self, index: usize, id: BlockId) -> Result<(), ModelError> {
        if self.body.contains(&id) {
            return Err(ModelError::DuplicateBlock(id));
        }
        if index > self.body.len() {
            return Err(ModelError::RegistryMismatch { ids: self.body.len(), blocks: index });
        }
        self.body.insert(index, id);
        Ok(())
    }

    /// Unregister a block; returns where it was.
    pub fn remove(&mut self, id: BlockId) -> Result<usize, ModelError> {
        let index = self.index_of(id).ok_or(ModelError::UnknownBlock(id))?;
        self.body.remove(index);
        Ok(index)
    }

    /// The ids in body order, for snapshots.
    pub fn ids(&self) -> &[BlockId] {
        &self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ids() -> impl FnMut() -> BlockId {
        let mut n = 0u128;
        move || {
            n += 1;
            BlockId(Uuid::from_u128(n))
        }
    }

    #[test]
    fn registry_follows_inserts_and_removals() {
        let doc = Document::default();
        let mut next = ids();
        let mut map = BlockIdentityMap::assign(&doc, &mut next).unwrap();
        map.check(&doc).unwrap();
        let (a, b, c) = (next(), next(), next());
        map.insert(map.len(), a).unwrap();
        map.insert(map.len(), c).unwrap();
        map.insert(map.len() - 1, b).unwrap();
        assert_eq!(map.ids().iter().rev().take(3).rev().copied().collect::<Vec<_>>(), [a, b, c]);
        let before = map.index_of(c).unwrap();
        map.remove(b).unwrap();
        assert_eq!(map.index_of(c), Some(before - 1));
        assert!(matches!(map.insert(0, a), Err(ModelError::DuplicateBlock(_))));
        assert!(matches!(map.remove(b), Err(ModelError::UnknownBlock(_))));
        assert!(map.insert(map.len() + 5, next()).is_err());
    }

    #[test]
    fn restore_refuses_a_registry_for_another_document() {
        let doc = Document::default();
        let mut next = ids();
        let extra: Vec<BlockId> = (0..doc.body.len() + 1).map(|_| next()).collect();
        assert!(matches!(BlockIdentityMap::restore(&doc, extra), Err(ModelError::RegistryMismatch { .. })));
    }

    #[test]
    fn restore_refuses_repeated_ids() {
        let mut doc = Document::default();
        doc.split_paragraph(&wordcraft_doc::Pos::body(0, 0)).unwrap();
        let a = BlockId(Uuid::from_u128(7));
        assert!(matches!(BlockIdentityMap::restore(&doc, vec![a, a]), Err(ModelError::DuplicateBlock(id)) if id == a));
        let mut next = ids();
        let map = BlockIdentityMap::assign(&doc, &mut next).unwrap();
        assert_eq!(BlockIdentityMap::restore(&doc, map.ids().to_vec()).unwrap(), map);
    }
}
