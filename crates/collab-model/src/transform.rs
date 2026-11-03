//! Rebasing an operation over concurrent commits (design §5.3).
//!
//! A client submits an operation against `base_revision`; the server may have committed other
//! operations since. [`transform`] rewrites the submission so it means the same thing after them.
//!
//! **Safety rule:** a pair of operations without an implemented and tested transformation is a
//! [`ModelError::Conflict`]; the submission is refused instead of being applied at a position
//! that may be wrong.
//!
//! Rules (`op` is the submission, `c` the operation committed before it):
//!
//! | `op` \ `c` | different blocks | `InsertText` (same block) | `DeleteText` (same block) | split / merge touching the block |
//! | --- | --- | --- | --- | --- |
//! | any | unchanged | | | `Conflict` (C1.4) |
//! | `InsertText` at `p` | | `p` moves past `c`'s text if `c` is at or before `p` | `p` maps through the deletion; inside it, to where it collapsed | |
//! | `DeleteText` `s..e` | | the range moves past `c`'s text if `c` is at or before `s`; `c` strictly inside is `Conflict` | the range loses its overlap with `c`'s (may become empty) | |
//!
//! **Tie-break:** two insertions at the same offset keep server order: the one committed first
//! stays first. Only the server transforms in the MVP (clients apply commits as they come), so
//! this is the same on every replica; an optimistic client (§15) rebasing its pending operations
//! over commits must use the same rule, which it can, since a commit is always "first".
//!
//! A deletion that would also remove text another user inserted inside it is a `Conflict` rather
//! than silently deleting their words or leaving half of the range: the user retries on the
//! current text.

use wordcraft_collab_protocol::{BlockId, DocumentOperation};

use crate::error::ModelError;

/// `op` rewritten to apply after `committed`, which the server accepted first.
pub fn transform(op: &DocumentOperation, committed: &DocumentOperation) -> Result<DocumentOperation, ModelError> {
    use DocumentOperation::{DeleteText, InsertText};
    let theirs = committed.touched_blocks();
    let mine = op.touched_blocks();
    if mine.iter().all(|b| !theirs.contains(b)) && !creates_any(committed, &mine) {
        return Ok(op.clone());
    }
    match (op, committed) {
        (InsertText { block_id, offset, text }, InsertText { block_id: theirs, offset: at, text: inserted }) if block_id == theirs => {
            let offset = if at <= offset { shift(*block_id, *offset, inserted)? } else { *offset };
            Ok(InsertText { block_id: *block_id, offset, text: text.clone() })
        }
        (InsertText { block_id, offset, text }, DeleteText { block_id: theirs, start, end }) if block_id == theirs => {
            let offset = through_deletion(*offset, (*start, *end))?;
            Ok(InsertText { block_id: *block_id, offset, text: text.clone() })
        }
        (DeleteText { block_id, start, end }, InsertText { block_id: theirs, offset: at, text: inserted }) if block_id == theirs => {
            check_range(*block_id, *start, *end)?;
            if at <= start {
                Ok(DeleteText { block_id: *block_id, start: shift(*block_id, *start, inserted)?, end: shift(*block_id, *end, inserted)? })
            } else if at >= end {
                Ok(op.clone())
            } else {
                Err(ModelError::Conflict)
            }
        }
        (DeleteText { block_id, start, end }, DeleteText { block_id: theirs, start: s, end: e }) if block_id == theirs => {
            check_range(*block_id, *start, *end)?;
            let deleted = (*s, *e);
            Ok(DeleteText { block_id: *block_id, start: through_deletion(*start, deleted)?, end: through_deletion(*end, deleted)? })
        }
        _ => Err(ModelError::Conflict),
    }
}

/// `op` rewritten to apply after every operation of `committed`, in order.
pub fn transform_all<'a>(
    op: &DocumentOperation,
    committed: impl IntoIterator<Item = &'a DocumentOperation>,
) -> Result<DocumentOperation, ModelError> {
    committed.into_iter().try_fold(op.clone(), |op, c| transform(&op, c))
}

/// Whether `committed` created one of `blocks` (a split's new paragraph).
fn creates_any(committed: &DocumentOperation, blocks: &[BlockId]) -> bool {
    matches!(committed, DocumentOperation::SplitParagraph { new_block_id, .. } if blocks.contains(new_block_id))
}

/// `offset` moved past `text` inserted before it.
fn shift(block: BlockId, offset: u32, text: &str) -> Result<u32, ModelError> {
    u32::try_from(text.len()).ok().and_then(|n| offset.checked_add(n)).ok_or(ModelError::InvalidOffset { block, offset })
}

/// Where `offset` is after bytes `start..end` were deleted: unchanged before them, pulled back
/// after them, and at `start` (where the deletion collapsed) inside them.
fn through_deletion(offset: u32, (start, end): (u32, u32)) -> Result<u32, ModelError> {
    if start > end {
        // A committed operation was validated; this is a corrupted history, never guessed at.
        return Err(ModelError::Conflict);
    }
    Ok(if offset <= start {
        offset
    } else if offset >= end {
        offset - (end - start)
    } else {
        start
    })
}

fn check_range(block: BlockId, start: u32, end: u32) -> Result<(), ModelError> {
    if start <= end { Ok(()) } else { Err(ModelError::InvalidOffset { block, offset: start }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply;
    use crate::identity::BlockIdentityMap;
    use proptest::prelude::*;
    use uuid::Uuid;
    use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};

    fn block(n: u128) -> BlockId {
        BlockId(Uuid::from_u128(n))
    }

    fn insert(b: u128, offset: u32, text: &str) -> DocumentOperation {
        DocumentOperation::InsertText { block_id: block(b), offset, text: text.into() }
    }

    fn delete(b: u128, start: u32, end: u32) -> DocumentOperation {
        DocumentOperation::DeleteText { block_id: block(b), start, end }
    }

    /// One paragraph with `text`, registered as block 1.
    fn one_paragraph(text: &str) -> (Document, BlockIdentityMap) {
        let mut doc = Document::default();
        doc.insert_text(&Pos::body(0, 0), text, &CharProps::default()).unwrap();
        let ids = BlockIdentityMap::restore(&doc, vec![block(1)]).unwrap();
        (doc, ids)
    }

    fn text(doc: &Document) -> String {
        doc.para(StoryRef::Body, &Path::top(0)).unwrap().text.clone()
    }

    /// `first` applied, then `second` transformed over it, from `base`.
    fn serial(base: &str, first: &DocumentOperation, second: &DocumentOperation) -> Result<String, ModelError> {
        let (mut doc, mut ids) = one_paragraph(base);
        apply(&mut doc, &mut ids, first)?;
        apply(&mut doc, &mut ids, &transform(second, first)?)?;
        Ok(text(&doc))
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
    fn design_example_converges_in_either_order() {
        let a = insert(1, 6, "beautiful ");
        let b = insert(1, 11, "!");
        assert_eq!(transform(&b, &a).unwrap(), insert(1, 21, "!"));
        assert_eq!(transform(&a, &b).unwrap(), a);
        assert_eq!(serial("Hello world", &a, &b).unwrap(), "Hello beautiful world!");
        assert_eq!(serial("Hello world", &b, &a).unwrap(), "Hello beautiful world!");
    }

    #[test]
    fn same_offset_insertions_keep_server_order() {
        let a = insert(1, 5, "A");
        let b = insert(1, 5, "B");
        assert_eq!(serial("Hello", &a, &b).unwrap(), "HelloAB");
        assert_eq!(serial("Hello", &b, &a).unwrap(), "HelloBA");
    }

    #[test]
    fn insertions_map_through_deletions() {
        // "Hello cruel world": delete "cruel " (6..12).
        let d = delete(1, 6, 12);
        let base = "Hello cruel world";
        assert_eq!(serial(base, &d, &insert(1, 0, ">")).unwrap(), ">Hello world");
        assert_eq!(serial(base, &d, &insert(1, 6, "big ")).unwrap(), "Hello big world");
        assert_eq!(serial(base, &d, &insert(1, 9, "XX")).unwrap(), "Hello XXworld");
        assert_eq!(serial(base, &d, &insert(1, 12, "new ")).unwrap(), "Hello new world");
        assert_eq!(serial(base, &d, &insert(1, 17, "!")).unwrap(), "Hello world!");
    }

    #[test]
    fn deletions_move_past_insertions_or_conflict_around_them() {
        let base = "Hello cruel world";
        let d = delete(1, 6, 12);
        assert_eq!(serial(base, &insert(1, 0, ">> "), &d).unwrap(), ">> Hello world");
        assert_eq!(serial(base, &insert(1, 6, "very "), &d).unwrap(), "Hello very world");
        assert_eq!(serial(base, &insert(1, 12, "big "), &d).unwrap(), "Hello big world");
        assert_eq!(serial(base, &insert(1, 17, "!"), &d).unwrap(), "Hello world!");
        // Someone typed inside the range being deleted: refused rather than deleting their text.
        assert!(matches!(transform(&d, &insert(1, 9, "XX")), Err(ModelError::Conflict)));
    }

    #[test]
    fn overlapping_deletions_delete_the_union() {
        let base = "0123456789";
        let cases =
            [((2, 5), (4, 8), "0189"), ((2, 8), (3, 5), "0189"), ((2, 4), (6, 8), "014589"), ((2, 5), (2, 5), "0156789"), ((2, 5), (5, 7), "01789")];
        for ((a0, a1), (b0, b1), want) in cases {
            let (a, b) = (delete(1, a0, a1), delete(1, b0, b1));
            assert_eq!(serial(base, &a, &b).unwrap(), want, "{a:?} then {b:?}");
            assert_eq!(serial(base, &b, &a).unwrap(), want, "{b:?} then {a:?}");
        }
        // Fully covered: the second deletion becomes a no-op.
        assert_eq!(transform(&delete(1, 3, 4), &delete(1, 2, 5)).unwrap(), delete(1, 2, 2));
    }

    #[test]
    fn multibyte_text_shifts_by_bytes() {
        let base = "ñandú";
        let a = insert(1, 2, "🦀");
        let b = insert(1, 7, "!");
        assert_eq!(serial(base, &a, &b).unwrap(), "ñ🦀andú!");
        assert_eq!(serial(base, &delete(1, 0, 2), &insert(1, 5, "·")).unwrap(), "and·ú");
    }

    #[test]
    fn structural_operations_on_the_same_block_are_conflicts() {
        let b = insert(1, 11, "!");
        let merge = DocumentOperation::MergeParagraphs { first: block(9), second: block(1) };
        assert!(matches!(transform(&b, &merge), Err(ModelError::Conflict)));
        let split = DocumentOperation::SplitParagraph { block_id: block(9), offset: 0, new_block_id: block(1) };
        assert!(matches!(transform(&b, &split), Err(ModelError::Conflict)));
        let split_mine = DocumentOperation::SplitParagraph { block_id: block(1), offset: 0, new_block_id: block(5) };
        assert!(matches!(transform(&split_mine, &b), Err(ModelError::Conflict)));
    }

    #[test]
    fn hostile_offsets_are_errors_not_overflows() {
        assert!(matches!(transform(&insert(1, u32::MAX, "x"), &insert(1, 0, "y")), Err(ModelError::InvalidOffset { .. })));
        assert!(matches!(transform(&delete(1, 5, 2), &insert(1, 0, "y")), Err(ModelError::InvalidOffset { .. })));
        assert!(matches!(transform(&delete(1, 5, 2), &delete(1, 0, 1)), Err(ModelError::InvalidOffset { .. })));
        assert!(matches!(transform(&insert(1, 3, "x"), &delete(1, 4, 1)), Err(ModelError::Conflict)));
    }

    /// Text from a multibyte alphabet and an operation on it at char boundaries.
    fn arb_base() -> impl Strategy<Value = String> {
        "[a-cñ🦀]{0,12}"
    }

    fn boundaries(s: &str) -> Vec<u32> {
        s.char_indices().map(|(i, _)| i as u32).chain([s.len() as u32]).collect()
    }

    fn arb_op(base: &str) -> impl Strategy<Value = DocumentOperation> + use<> {
        let cuts = boundaries(base);
        let n = cuts.len();
        (any::<bool>(), 0..n, 0..n, "[xyé🎉]{1,3}").prop_map(move |(ins, i, j, t)| {
            let (i, j) = (cuts[i.min(j)], cuts[i.max(j)]);
            if ins { insert(1, i, &t) } else { delete(1, i, j) }
        })
    }

    fn arb_pair() -> impl Strategy<Value = (String, DocumentOperation, DocumentOperation)> {
        arb_base().prop_flat_map(|base| {
            let (a, b) = (arb_op(&base), arb_op(&base));
            (Just(base), a, b)
        })
    }

    proptest! {
        /// Concurrent operations that both transform converge in either server order (TP1), except
        /// same-offset insertions, whose order is the server's by design.
        #[test]
        fn concurrent_pairs_converge((base, a, b) in arb_pair()) {
            let tie = matches!((&a, &b), (DocumentOperation::InsertText { offset: x, .. }, DocumentOperation::InsertText { offset: y, .. }) if x == y);
            let ab = serial(&base, &a, &b);
            let ba = serial(&base, &b, &a);
            match (&ab, &ba) {
                (Ok(ab), Ok(ba)) if !tie => prop_assert_eq!(ab, ba),
                (Ok(_), Ok(_)) => {}
                // A conflict is allowed (the safety rule); anything else is a bug in a rule.
                _ => {
                    for r in [&ab, &ba] {
                        prop_assert!(matches!(r, Ok(_) | Err(ModelError::Conflict)), "{:?}", r);
                    }
                }
            }
            // Nobody's insertion is lost: each inserted text is in the result unless the other
            // operation deleted around it, which happens only when the insertion was inside it.
            if let Ok(ab) = &ab {
                for op in [&a, &b] {
                    if let DocumentOperation::InsertText { text, .. } = op {
                        prop_assert!(ab.contains(text.as_str()), "{} lost {:?}", ab, text);
                    }
                }
            }
        }
    }
}
