//! Rebasing an operation over concurrent commits (design §5.3).
//!
//! A client submits an operation against `base_revision`; the server may have committed other
//! operations since. [`transform`] rewrites the submission so it means the same thing after them.
//!
//! **Safety rule:** a pair of operations without an implemented and tested transformation is a
//! [`ModelError::Conflict`]; the submission is refused instead of being applied at a position
//! that may be wrong.
//!
//! Rules (`op` is the submission, `c` the operation committed before it; both on the same
//! paragraph unless said otherwise; operations on different paragraphs pass unchanged):
//!
//! | `op` \ `c` | `InsertText` at `q` | `DeleteText` `s..e` | `SplitParagraph` at `k` (tail → `n`) | `MergeParagraphs` `f` + `g` (`f` was `L` long) |
//! | --- | --- | --- | --- | --- |
//! | `InsertText` at `p` | moves past `c`'s text if `q ≤ p` | maps through the deletion; inside it, to `s` | stays if `p ≤ k`, else goes to `n` at `p − k` | in `g`: goes to `f` at `L + p` |
//! | `DeleteText` `a..b` | moves past `c`'s text if `q ≤ a`; `q` strictly inside is `Conflict` | loses its overlap (may become empty) | stays if `b ≤ k`, goes to `n` if `a ≥ k`; across `k` is `Conflict` | in `g`: goes to `f` shifted by `L` |
//! | `SplitParagraph` at `j` | `j` moves past `c`'s text if `q ≤ j` | maps through the deletion | stays if `j ≤ k`, else splits `n` at `j − k` | in `g`: splits `f` at `L + j` |
//! | `MergeParagraphs` `f` + `g` | `L` grows if `c` is in `f` | `L` shrinks if `c` is in `f` | `c` split `f`: `Conflict` (no longer adjacent); `c` split `g`: unchanged | same merge: a no-op; chained merges re-target (see [`after_merge`]) |
//!
//! **Ties** keep server order: two insertions at the same offset, or two splits at the same
//! offset, leave the committed one first. An insertion at a split point stays at the end of the
//! first paragraph (and a split moves past an insertion at its offset), the same in both orders.
//! Only the server transforms in the MVP (clients apply commits as they come), so this is the same
//! on every replica; an optimistic client (§15) rebasing its pending operations over commits must
//! use the same rules, which it can, since a commit is always "first".
//!
//! A deletion that would also remove text another user inserted inside it, or that a concurrent
//! split cut in two, is a `Conflict` rather than silently deleting their words or leaving half of
//! the range: the user retries on the current text.
//!
//! Text converges in either server order (TP1, tested). Paragraph *properties* need not: a split
//! of `g` after `f` + `g` were merged gives the new paragraph `f`'s properties, the other order
//! `g`'s. Every replica applies the server's order, so they still end identical.

use wordcraft_collab_protocol::{BlockId, DocumentOperation};

use crate::error::ModelError;

use DocumentOperation::{DeleteText, InsertText, MergeParagraphs, SplitParagraph};

/// `op` rewritten to apply after `committed`, which the server accepted first.
pub fn transform(op: &DocumentOperation, committed: &DocumentOperation) -> Result<DocumentOperation, ModelError> {
    let theirs = committed.touched_blocks();
    let mine = op.touched_blocks();
    if creates_any(committed, &mine) {
        // `op` names a paragraph that did not exist at its base revision: a reused id.
        return Err(ModelError::Conflict);
    }
    if mine.iter().all(|b| !theirs.contains(b)) {
        return Ok(op.clone());
    }
    check(op)?;
    match committed {
        InsertText { block_id, offset, text } => after_insert(op, *block_id, *offset, len(*block_id, text)?),
        DeleteText { block_id, start, end } => {
            if start > end {
                // A committed operation was validated; this is a corrupted history, never guessed at.
                return Err(ModelError::Conflict);
            }
            after_delete(op, *block_id, (*start, *end))
        }
        SplitParagraph { block_id, offset, new_block_id } => after_split(op, *block_id, *offset, *new_block_id),
        MergeParagraphs { first, second, first_len } => after_merge(op, *first, *second, *first_len),
    }
}

/// `op` rewritten to apply after every operation of `committed`, in order.
pub fn transform_all<'a>(
    op: &DocumentOperation,
    committed: impl IntoIterator<Item = &'a DocumentOperation>,
) -> Result<DocumentOperation, ModelError> {
    committed.into_iter().try_fold(op.clone(), |op, c| transform(&op, c))
}

/// After `c` inserted `n` bytes at `at` of `block`.
fn after_insert(op: &DocumentOperation, block: BlockId, at: u32, n: u32) -> Result<DocumentOperation, ModelError> {
    let moved = |off: u32, b: BlockId| if at <= off { add(b, off, n) } else { Ok(off) };
    Ok(match op {
        InsertText { block_id, offset, text } => InsertText { block_id: *block_id, offset: moved(*offset, *block_id)?, text: text.clone() },
        DeleteText { block_id, start, end } => {
            if at <= *start {
                DeleteText { block_id: *block_id, start: add(*block_id, *start, n)?, end: add(*block_id, *end, n)? }
            } else if at >= *end {
                op.clone()
            } else {
                return Err(ModelError::Conflict);
            }
        }
        SplitParagraph { block_id, offset, new_block_id } => {
            SplitParagraph { block_id: *block_id, offset: moved(*offset, *block_id)?, new_block_id: *new_block_id }
        }
        MergeParagraphs { first, second, first_len } if *first == block => {
            if at > *first_len {
                return Err(ModelError::Conflict);
            }
            MergeParagraphs { first: *first, second: *second, first_len: add(*first, *first_len, n)? }
        }
        MergeParagraphs { .. } => op.clone(),
    })
}

/// After `c` deleted bytes `start..end` of `block`.
fn after_delete(op: &DocumentOperation, block: BlockId, deleted: (u32, u32)) -> Result<DocumentOperation, ModelError> {
    let (start, end) = deleted;
    Ok(match op {
        InsertText { block_id, offset, text } => InsertText { block_id: *block_id, offset: through_deletion(*offset, deleted), text: text.clone() },
        DeleteText { block_id, start: a, end: b } => {
            DeleteText { block_id: *block_id, start: through_deletion(*a, deleted), end: through_deletion(*b, deleted) }
        }
        SplitParagraph { block_id, offset, new_block_id } => {
            SplitParagraph { block_id: *block_id, offset: through_deletion(*offset, deleted), new_block_id: *new_block_id }
        }
        MergeParagraphs { first, second, first_len } if *first == block => {
            if end > *first_len {
                return Err(ModelError::Conflict);
            }
            MergeParagraphs { first: *first, second: *second, first_len: first_len - (end - start) }
        }
        MergeParagraphs { .. } => op.clone(),
    })
}

/// After `c` split `block` at `k`, its tail becoming `tail`.
fn after_split(op: &DocumentOperation, block: BlockId, k: u32, tail: BlockId) -> Result<DocumentOperation, ModelError> {
    Ok(match op {
        InsertText { offset, .. } if *offset <= k => op.clone(),
        InsertText { offset, text, .. } => InsertText { block_id: tail, offset: offset - k, text: text.clone() },
        DeleteText { end, .. } if *end <= k => op.clone(),
        DeleteText { start, end, .. } if *start >= k => DeleteText { block_id: tail, start: start - k, end: end - k },
        DeleteText { .. } => return Err(ModelError::Conflict),
        SplitParagraph { offset, .. } if *offset <= k => op.clone(),
        SplitParagraph { offset, new_block_id, .. } => SplitParagraph { block_id: tail, offset: offset - k, new_block_id: *new_block_id },
        // `first` was cut in two: the paragraphs to merge are no longer next to each other.
        MergeParagraphs { first, .. } if *first == block => return Err(ModelError::Conflict),
        MergeParagraphs { .. } => op.clone(),
    })
}

/// After `c` appended `second` to `first`, which was `len` bytes long.
///
/// Merges against merges: the same merge again is a no-op (an empty deletion, so the submission
/// still commits and its author's queue moves on); `x` + `first` stays as it is (`first` now
/// holds `second` too); `second` + `y` becomes `first` + `y`, with `first` longer by `x`'s
/// former length. Any other overlap cannot come from adjacent paragraphs and is a `Conflict`.
fn after_merge(op: &DocumentOperation, first: BlockId, second: BlockId, len: u32) -> Result<DocumentOperation, ModelError> {
    Ok(match op {
        InsertText { block_id, offset, text } if *block_id == second => {
            InsertText { block_id: first, offset: add(first, *offset, len)?, text: text.clone() }
        }
        DeleteText { block_id, start, end } if *block_id == second => {
            DeleteText { block_id: first, start: add(first, *start, len)?, end: add(first, *end, len)? }
        }
        SplitParagraph { block_id, offset, new_block_id } if *block_id == second => {
            SplitParagraph { block_id: first, offset: add(first, *offset, len)?, new_block_id: *new_block_id }
        }
        InsertText { .. } | DeleteText { .. } | SplitParagraph { .. } => op.clone(),
        MergeParagraphs { first: f, second: g, .. } if (*f, *g) == (first, second) => DeleteText { block_id: first, start: len, end: len },
        MergeParagraphs { second: g, .. } if *g == first => op.clone(),
        MergeParagraphs { first: f, second: g, first_len } if *f == second => {
            MergeParagraphs { first, second: *g, first_len: add(first, *first_len, len)? }
        }
        MergeParagraphs { .. } => return Err(ModelError::Conflict),
    })
}

/// Whether `committed` created one of `blocks` (a split's new paragraph).
fn creates_any(committed: &DocumentOperation, blocks: &[BlockId]) -> bool {
    matches!(committed, SplitParagraph { new_block_id, .. } if blocks.contains(new_block_id))
}

/// A submission's own ranges must be ordered before any arithmetic on them.
fn check(op: &DocumentOperation) -> Result<(), ModelError> {
    match op {
        DeleteText { block_id, start, end } if start > end => Err(ModelError::InvalidOffset { block: *block_id, offset: *start }),
        _ => Ok(()),
    }
}

/// The byte length of inserted text, as an offset.
fn len(block: BlockId, text: &str) -> Result<u32, ModelError> {
    u32::try_from(text.len()).map_err(|_| ModelError::InvalidOffset { block, offset: u32::MAX })
}

/// `offset + n`, or an error instead of an overflow.
fn add(block: BlockId, offset: u32, n: u32) -> Result<u32, ModelError> {
    offset.checked_add(n).ok_or(ModelError::InvalidOffset { block, offset })
}

/// Where `offset` is after bytes `start..end` (`start ≤ end`) were deleted: unchanged before
/// them, pulled back after them, and at `start` (where the deletion collapsed) inside them.
fn through_deletion(offset: u32, (start, end): (u32, u32)) -> u32 {
    if offset <= start {
        offset
    } else if offset >= end {
        offset - (end - start)
    } else {
        start
    }
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
        let merge = DocumentOperation::MergeParagraphs { first: block(3), second: block(4), first_len: 0 };
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

    fn split(b: u128, offset: u32, new: u128) -> DocumentOperation {
        DocumentOperation::SplitParagraph { block_id: block(b), offset, new_block_id: block(new) }
    }

    fn merge(first: u128, second: u128, first_len: u32) -> DocumentOperation {
        DocumentOperation::MergeParagraphs { first: block(first), second: block(second), first_len }
    }

    /// Paragraphs with `texts`, registered as blocks 1, 2, 3…
    fn paragraphs(texts: &[&str]) -> (Document, BlockIdentityMap) {
        let mut doc = Document::default();
        let mut ids = BlockIdentityMap::restore(&doc, vec![block(1)]).unwrap();
        for (i, t) in texts.iter().enumerate() {
            if i > 0 {
                let len = doc.para(StoryRef::Body, &Path::top(i - 1)).unwrap().len() as u32;
                apply(&mut doc, &mut ids, &split(i as u128, len, i as u128 + 1)).unwrap();
            }
            if !t.is_empty() {
                apply(&mut doc, &mut ids, &insert(i as u128 + 1, 0, t)).unwrap();
            }
        }
        (doc, ids)
    }

    /// Every paragraph's text, in order, with its block number.
    fn texts(doc: &Document, ids: &BlockIdentityMap) -> Vec<(u128, String)> {
        ids.ids().iter().enumerate().map(|(i, id)| (id.0.as_u128(), doc.para(StoryRef::Body, &Path::top(i)).unwrap().text.clone())).collect()
    }

    /// `first` then `second` transformed over it, from paragraphs `base`.
    fn serial_paragraphs(base: &[&str], first: &DocumentOperation, second: &DocumentOperation) -> Result<Vec<(u128, String)>, ModelError> {
        let (mut doc, mut ids) = paragraphs(base);
        apply(&mut doc, &mut ids, first)?;
        apply(&mut doc, &mut ids, &transform(second, first)?)?;
        Ok(texts(&doc, &ids))
    }

    fn owned(v: &[(u128, &str)]) -> Vec<(u128, String)> {
        v.iter().map(|(b, t)| (*b, t.to_string())).collect()
    }

    #[test]
    fn edits_follow_a_concurrent_split() {
        let base = ["Hello world"];
        let s = split(1, 6, 9);
        // Before, at and after the split point.
        assert_eq!(serial_paragraphs(&base, &s, &insert(1, 0, ">")).unwrap(), owned(&[(1, ">Hello "), (9, "world")]));
        assert_eq!(serial_paragraphs(&base, &s, &insert(1, 6, "big ")).unwrap(), owned(&[(1, "Hello big "), (9, "world")]));
        assert_eq!(serial_paragraphs(&base, &insert(1, 6, "big "), &s).unwrap(), owned(&[(1, "Hello big "), (9, "world")]));
        assert_eq!(serial_paragraphs(&base, &s, &insert(1, 11, "!")).unwrap(), owned(&[(1, "Hello "), (9, "world!")]));
        assert_eq!(serial_paragraphs(&base, &s, &delete(1, 6, 9)).unwrap(), owned(&[(1, "Hello "), (9, "ld")]));
        assert_eq!(serial_paragraphs(&base, &delete(1, 6, 9), &s).unwrap(), owned(&[(1, "Hello "), (9, "ld")]));
        assert_eq!(serial_paragraphs(&base, &s, &split(1, 8, 7)).unwrap(), owned(&[(1, "Hello "), (9, "wo"), (7, "rld")]));
        // A deletion across the split point would now span two paragraphs.
        assert!(matches!(transform(&delete(1, 3, 8), &s), Err(ModelError::Conflict)));
        // Splitting inside a concurrently deleted range splits where it collapsed.
        assert_eq!(serial_paragraphs(&base, &delete(1, 4, 8), &s).unwrap(), owned(&[(1, "Hell"), (9, "rld")]));
    }

    #[test]
    fn edits_follow_a_concurrent_merge() {
        let base = ["Hello ", "world"];
        let m = merge(1, 2, 6);
        assert_eq!(serial_paragraphs(&base, &m, &insert(2, 5, "!")).unwrap(), owned(&[(1, "Hello world!")]));
        assert_eq!(serial_paragraphs(&base, &insert(2, 5, "!"), &m).unwrap(), owned(&[(1, "Hello world!")]));
        assert_eq!(serial_paragraphs(&base, &m, &insert(1, 0, ">")).unwrap(), owned(&[(1, ">Hello world")]));
        assert_eq!(serial_paragraphs(&base, &insert(1, 6, "big "), &m).unwrap(), owned(&[(1, "Hello big world")]));
        assert_eq!(serial_paragraphs(&base, &delete(1, 0, 6), &m).unwrap(), owned(&[(1, "world")]));
        assert_eq!(serial_paragraphs(&base, &m, &delete(2, 0, 1)).unwrap(), owned(&[(1, "Hello orld")]));
        assert_eq!(serial_paragraphs(&base, &m, &split(2, 2, 9)).unwrap(), owned(&[(1, "Hello wo"), (9, "rld")]));
        assert_eq!(serial_paragraphs(&base, &split(2, 2, 9), &m).unwrap(), owned(&[(1, "Hello wo"), (9, "rld")]));
        // Two people pressing Backspace at the start of "world": the second is a no-op.
        assert_eq!(transform(&m, &m).unwrap(), delete(1, 6, 6));
        assert_eq!(serial_paragraphs(&base, &m, &m).unwrap(), owned(&[(1, "Hello world")]));
        // `first` split by someone else: no longer adjacent.
        assert!(matches!(transform(&m, &split(1, 2, 9)), Err(ModelError::Conflict)));
    }

    #[test]
    fn chained_merges_re_target() {
        let base = ["a", "b", "c"];
        let (ab, bc) = (merge(1, 2, 1), merge(2, 3, 1));
        assert_eq!(transform(&bc, &ab).unwrap(), merge(1, 3, 2));
        assert_eq!(transform(&ab, &bc).unwrap(), ab);
        assert_eq!(serial_paragraphs(&base, &ab, &bc).unwrap(), owned(&[(1, "abc")]));
        assert_eq!(serial_paragraphs(&base, &bc, &ab).unwrap(), owned(&[(1, "abc")]));
        // Overlaps adjacent paragraphs can't produce are conflicts, never guesses.
        assert!(matches!(transform(&merge(1, 3, 1), &ab), Err(ModelError::Conflict)));
    }

    #[test]
    fn a_reused_new_block_id_is_a_conflict() {
        assert!(matches!(transform(&insert(9, 0, "x"), &split(1, 0, 9)), Err(ModelError::Conflict)));
        assert!(matches!(transform(&merge(9, 2, 0), &split(1, 0, 9)), Err(ModelError::Conflict)));
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

    /// Two paragraphs from a multibyte alphabet and an operation on them: text edits at char
    /// boundaries, a split (new block 10 + `tag`), or merging them.
    fn arb_two() -> impl Strategy<Value = [String; 2]> {
        [arb_base(), arb_base()]
    }

    fn arb_structural(base: &[String; 2], tag: u128) -> impl Strategy<Value = DocumentOperation> + use<> {
        let first_len = base[0].len() as u32;
        let (a, b) = (arb_op(&base[0]), arb_op(&base[1]));
        let cuts: Vec<Vec<u32>> = base.iter().map(|t| boundaries(t)).collect();
        let splits = (0..2usize, 0..64usize).prop_map(move |(p, i)| {
            let c = &cuts[p];
            split(p as u128 + 1, c[i % c.len()], 10 + tag)
        });
        prop_oneof![
            a,
            b.prop_map(|op| match op {
                DocumentOperation::InsertText { offset, text, .. } => DocumentOperation::InsertText { block_id: block(2), offset, text },
                DocumentOperation::DeleteText { start, end, .. } => DocumentOperation::DeleteText { block_id: block(2), start, end },
                other => other,
            }),
            splits,
            Just(merge(1, 2, first_len)),
        ]
    }

    fn arb_structural_pair() -> impl Strategy<Value = ([String; 2], DocumentOperation, DocumentOperation)> {
        arb_two().prop_flat_map(|base| {
            let (a, b) = (arb_structural(&base, 0), arb_structural(&base, 1));
            (Just(base), a, b)
        })
    }

    /// Same-offset insertions or splits in the same paragraph: ordered by the server.
    fn is_tie(a: &DocumentOperation, b: &DocumentOperation) -> bool {
        let at = |op: &DocumentOperation| match op {
            DocumentOperation::InsertText { block_id, offset, .. } => Some((0, *block_id, *offset)),
            DocumentOperation::SplitParagraph { block_id, offset, .. } => Some((1, *block_id, *offset)),
            _ => None,
        };
        matches!((at(a), at(b)), (Some(x), Some(y)) if x == y)
    }

    proptest! {
        /// TP1 with splits and merges: both server orders give the same paragraphs, in the same
        /// order, with the same ids — or one of them is an explicit conflict.
        #[test]
        fn structural_pairs_converge((base, a, b) in arb_structural_pair()) {
            let base = [base[0].as_str(), base[1].as_str()];
            let ab = serial_paragraphs(&base, &a, &b);
            let ba = serial_paragraphs(&base, &b, &a);
            match (&ab, &ba) {
                (Ok(ab), Ok(ba)) if !is_tie(&a, &b) => prop_assert_eq!(ab, ba),
                (Ok(_), Ok(_)) => {}
                _ => {
                    for r in [&ab, &ba] {
                        prop_assert!(matches!(r, Ok(_) | Err(ModelError::Conflict)), "{:?}", r);
                    }
                }
            }
        }
    }
}
