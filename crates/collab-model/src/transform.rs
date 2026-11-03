//! Rebasing an operation over concurrent commits (design §5.3).
//!
//! A client submits an operation against `base_revision`; the server may have committed other
//! operations since. [`transform`] rewrites the submission so it means the same thing after them.
//!
//! **Safety rule:** a pair of operations without an implemented and tested transformation is a
//! [`ModelError::Conflict`]; the submission is refused instead of being applied at a position
//! that may be wrong. Rules are added one pair at a time, each with unit and property tests
//! (milestone C1.3): insert/insert with a deterministic tie-break, insert/delete,
//! delete/delete overlaps, then split and merge.
//!
//! Implemented so far: operations on disjoint sets of blocks commute and pass through unchanged.

use wordcraft_collab_protocol::DocumentOperation;

use crate::error::ModelError;

/// `op` rewritten to apply after `committed`, which the server accepted first.
pub fn transform(op: &DocumentOperation, committed: &DocumentOperation) -> Result<DocumentOperation, ModelError> {
    let theirs = committed.touched_blocks();
    let disjoint = op.touched_blocks().iter().all(|b| !theirs.contains(b)) && !creates_any(committed, &op.touched_blocks());
    if disjoint { Ok(op.clone()) } else { Err(ModelError::Conflict) }
}

/// `op` rewritten to apply after every operation of `committed`, in order.
pub fn transform_all<'a>(
    op: &DocumentOperation,
    committed: impl IntoIterator<Item = &'a DocumentOperation>,
) -> Result<DocumentOperation, ModelError> {
    committed.into_iter().try_fold(op.clone(), |op, c| transform(&op, c))
}

/// Whether `committed` created one of `blocks` (a split's new paragraph).
fn creates_any(committed: &DocumentOperation, blocks: &[wordcraft_collab_protocol::BlockId]) -> bool {
    matches!(committed, DocumentOperation::SplitParagraph { new_block_id, .. } if blocks.contains(new_block_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use wordcraft_collab_protocol::BlockId;

    fn block(n: u128) -> BlockId {
        BlockId(Uuid::from_u128(n))
    }

    fn insert(b: u128, offset: u32, text: &str) -> DocumentOperation {
        DocumentOperation::InsertText { block_id: block(b), offset, text: text.into() }
    }

    #[test]
    fn edits_in_different_paragraphs_pass_through() {
        let a = insert(1, 0, "A");
        let b = insert(2, 5, "B");
        assert_eq!(transform(&b, &a).unwrap(), b);
        let merge = DocumentOperation::MergeParagraphs { first: block(3), second: block(4) };
        assert_eq!(transform_all(&b, [&a, &merge]).unwrap(), b);
    }

    #[test]
    fn same_paragraph_without_a_rule_is_a_conflict() {
        let a = insert(1, 6, "beautiful ");
        let b = insert(1, 11, "!");
        assert!(matches!(transform(&b, &a), Err(ModelError::Conflict)));
        let merge = DocumentOperation::MergeParagraphs { first: block(9), second: block(1) };
        assert!(matches!(transform(&b, &merge), Err(ModelError::Conflict)));
        let split = DocumentOperation::SplitParagraph { block_id: block(9), offset: 0, new_block_id: block(1) };
        assert!(matches!(transform(&b, &split), Err(ModelError::Conflict)));
    }
}
