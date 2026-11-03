//! Structural anchors (design §5.4, §10).
//!
//! Carets, selections and remote participants' cursors are kept as (block id, byte offset)
//! anchors rather than WordCraft `Pos` values, so a remote edit that moves blocks does not leave
//! them pointing somewhere else. After each remote commit the client transforms its anchors and
//! converts them back to `Pos` for the engine (milestone C4).

use wordcraft_collab_protocol::messages::Anchor;
use wordcraft_doc::{Pos, StoryRef};

use crate::identity::BlockIdentityMap;

/// A caret in the body as an anchor, if its block is a registered top-level block.
pub fn anchor_from_pos(ids: &BlockIdentityMap, pos: &Pos) -> Option<Anchor> {
    if pos.story != StoryRef::Body || pos.path.0.len() != 1 {
        return None;
    }
    let block_id = ids.id_at(pos.path.last())?;
    Some(Anchor { block_id, offset: u32::try_from(pos.off).ok()? })
}

/// The `Pos` an anchor names now, if its block still exists.
pub fn pos_from_anchor(ids: &BlockIdentityMap, anchor: &Anchor) -> Option<Pos> {
    let index = ids.index_of(anchor.block_id)?;
    Some(Pos::body(index, usize::try_from(anchor.offset).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use wordcraft_collab_protocol::BlockId;
    use wordcraft_doc::Path;

    #[test]
    fn anchors_follow_their_block() {
        let (a, b) = (BlockId(Uuid::from_u128(1)), BlockId(Uuid::from_u128(2)));
        let mut ids = BlockIdentityMap::default();
        ids.insert(0, b).unwrap();
        let anchor = anchor_from_pos(&ids, &Pos::body(0, 3)).unwrap();
        assert_eq!(anchor.block_id, b);
        ids.insert(0, a).unwrap();
        assert_eq!(pos_from_anchor(&ids, &anchor), Some(Pos::body(1, 3)));
        ids.remove(b).unwrap();
        assert_eq!(pos_from_anchor(&ids, &anchor), None);
        assert_eq!(anchor_from_pos(&ids, &Pos::new(StoryRef::Body, Path(vec![0, 0, 0, 0]), 0)), None);
        assert_eq!(anchor_from_pos(&ids, &Pos::new(StoryRef::Part(1), Path::top(0), 0)), None);
    }
}
