//! Structural anchors (design §5.4, §10).
//!
//! Carets, selections and remote participants' cursors are kept as (block id, byte offset)
//! anchors rather than WordCraft `Pos` values, so a remote edit that moves blocks does not leave
//! them pointing somewhere else. After each remote commit the client transforms its anchors and
//! converts them back to `Pos` for the engine (milestone C4).

use wordcraft_collab_protocol::DocumentOperation;
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

/// Where `anchor` is after `op` was applied. `own` is true for the caret of the client that made
/// `op`: its caret follows its own typing (past an insertion at the caret, onto the paragraph
/// an Enter created), while another participant's insertion at the caret leaves it in front.
pub fn transform_anchor(anchor: Anchor, op: &DocumentOperation, own: bool) -> Anchor {
    let Anchor { block_id, offset } = anchor;
    match op {
        DocumentOperation::InsertText { block_id: b, offset: at, text } if *b == block_id => {
            let moves = if own { offset >= *at } else { offset > *at };
            let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
            Anchor { block_id, offset: if moves { offset.saturating_add(len) } else { offset } }
        }
        DocumentOperation::DeleteText { block_id: b, start, end } if *b == block_id && start <= end => {
            let offset = if offset <= *start {
                offset
            } else if offset >= *end {
                offset - (end - start)
            } else {
                *start
            };
            Anchor { block_id, offset }
        }
        DocumentOperation::SplitParagraph { block_id: b, offset: at, new_block_id } if *b == block_id => {
            let moves = if own { offset >= *at } else { offset > *at };
            if moves { Anchor { block_id: *new_block_id, offset: offset - at } } else { anchor }
        }
        DocumentOperation::MergeParagraphs { first, second, first_len } if *second == block_id => {
            Anchor { block_id: *first, offset: first_len.saturating_add(offset) }
        }
        _ => anchor,
    }
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

    #[test]
    fn anchors_move_with_operations() {
        let (a, b, n) = (BlockId(Uuid::from_u128(1)), BlockId(Uuid::from_u128(2)), BlockId(Uuid::from_u128(3)));
        let at = |block_id, offset| Anchor { block_id, offset };
        let insert = DocumentOperation::InsertText { block_id: a, offset: 4, text: "ñx".into() };
        // Another participant typing at my caret leaves it in front; my own typing moves it on.
        assert_eq!(transform_anchor(at(a, 4), &insert, false), at(a, 4));
        assert_eq!(transform_anchor(at(a, 4), &insert, true), at(a, 7));
        assert_eq!(transform_anchor(at(a, 5), &insert, false), at(a, 8));
        assert_eq!(transform_anchor(at(b, 5), &insert, false), at(b, 5));
        let delete = DocumentOperation::DeleteText { block_id: a, start: 2, end: 6 };
        assert_eq!([1, 2, 4, 6, 9].map(|o| transform_anchor(at(a, o), &delete, false).offset), [1, 2, 2, 2, 5]);
        let split = DocumentOperation::SplitParagraph { block_id: a, offset: 3, new_block_id: n };
        assert_eq!(transform_anchor(at(a, 3), &split, false), at(a, 3));
        assert_eq!(transform_anchor(at(a, 3), &split, true), at(n, 0));
        assert_eq!(transform_anchor(at(a, 5), &split, false), at(n, 2));
        let merge = DocumentOperation::MergeParagraphs { first: a, second: b, first_len: 10 };
        assert_eq!(transform_anchor(at(b, 2), &merge, false), at(a, 12));
        assert_eq!(transform_anchor(at(a, 2), &merge, false), at(a, 2));
    }
}
