//! Validating and applying operations to a document (design §5.2, §6 "Cambios remotos").
//!
//! The same function runs on the server (on a working copy, before persisting) and on every
//! client (after the commit arrives), so all of them reach the same state. It:
//! - checks the block exists, is a paragraph, and offsets are in range and on char boundaries —
//!   operations come from the network and are hostile;
//! - applies the change through `wordcraft_doc` edit primitives (`insert_text`, `delete_range`),
//!   never by replaying UI commands;
//! - keeps the [`BlockIdentityMap`] in step: a split registers `new_block_id` right after its
//!   paragraph, a merge (of adjacent paragraphs, `first_len` matching) unregisters `second`;
//! - leaves the document untouched when it returns an error: everything is validated before the
//!   first edit.
//!
//! Embedded objects (U+FFFC: pictures, fields, bookmarks, content control markers…) are not
//! shared-editable yet. Text to insert may not contain U+FFFC, and a deletion may not cover one:
//! `delete_range` keeps some markers in place, so the bytes removed would differ from
//! `end - start` and the offsets [`crate::transform`] computes would be wrong.

use wordcraft_collab_protocol::{BlockId, DocumentOperation};
use wordcraft_doc::para::OBJ;
use wordcraft_doc::{Block, Document, Paragraph, Pos};

use crate::error::ModelError;
use crate::identity::BlockIdentityMap;

/// Apply a committed (or about-to-be-committed) operation.
pub fn apply(doc: &mut Document, ids: &mut BlockIdentityMap, op: &DocumentOperation) -> Result<(), ModelError> {
    ids.check(doc)?;
    match op {
        DocumentOperation::InsertText { block_id, offset, text } => {
            let (index, para) = paragraph(doc, ids, *block_id)?;
            let off = boundary(para, *block_id, *offset)?;
            check_text(text)?;
            // Typed text continues the formatting to its left, as the editor's typing does.
            let props = para.props_at(off).clone();
            doc.insert_text(&Pos::body(index, off), text, &props)?;
        }
        DocumentOperation::DeleteText { block_id, start, end } => {
            let (index, para) = paragraph(doc, ids, *block_id)?;
            let a = boundary(para, *block_id, *start)?;
            let b = boundary(para, *block_id, *end)?;
            if a > b {
                return Err(ModelError::InvalidOffset { block: *block_id, offset: *start });
            }
            if para.text.get(a..b).is_some_and(|t| t.contains(OBJ)) {
                return Err(ModelError::Unsupported("deleting embedded objects"));
            }
            doc.delete_range(&Pos::body(index, a), &Pos::body(index, b))?;
        }
        DocumentOperation::SplitParagraph { block_id, offset, new_block_id } => {
            let (index, para) = paragraph(doc, ids, *block_id)?;
            let off = boundary(para, *block_id, *offset)?;
            if ids.index_of(*new_block_id).is_some() {
                return Err(ModelError::DuplicateBlock(*new_block_id));
            }
            atomically(doc, ids, |doc, ids| {
                doc.split_paragraph(&Pos::body(index, off))?;
                ids.insert(index + 1, *new_block_id)
            })?;
        }
        DocumentOperation::MergeParagraphs { first, second, first_len } => {
            let (index, head) = paragraph(doc, ids, *first)?;
            let (next, tail) = paragraph(doc, ids, *second)?;
            if next != index + 1 {
                return Err(ModelError::NotAdjacent { first: *first, second: *second });
            }
            if usize::try_from(*first_len).ok() != Some(head.len()) {
                return Err(ModelError::InvalidOffset { block: *first, offset: *first_len });
            }
            let (blocks, at, joined) = (doc.body.len(), head.len(), head.len() + tail.len());
            atomically(doc, ids, |doc, ids| {
                // The same primitive as Backspace at a paragraph start; the result is checked so
                // a primitive that ever joined differently is refused, not committed.
                doc.delete_range(&Pos::body(index, at), &Pos::body(next, 0))?;
                let ok = doc.body.len() + 1 == blocks && matches!(doc.body.get(index).map(|b| &**b), Some(Block::Para(p)) if p.len() == joined);
                if !ok {
                    return Err(ModelError::Unsupported("merging these paragraphs"));
                }
                ids.remove(*second).map(|_| ())
            })?;
        }
    }
    Ok(())
}

/// Run a structural edit (several primitive steps, document and registry) all or nothing: on
/// error both are put back. Blocks are `Arc`-shared, so the backup copies no paragraph.
fn atomically(
    doc: &mut Document,
    ids: &mut BlockIdentityMap,
    edit: impl FnOnce(&mut Document, &mut BlockIdentityMap) -> Result<(), ModelError>,
) -> Result<(), ModelError> {
    let backup = (doc.clone(), ids.clone());
    let result = edit(doc, ids);
    if result.is_err() {
        (*doc, *ids) = backup;
    }
    result
}

/// The body paragraph named `id`, with its index.
fn paragraph<'d>(doc: &'d Document, ids: &BlockIdentityMap, id: BlockId) -> Result<(usize, &'d Paragraph), ModelError> {
    let index = ids.index_of(id).ok_or(ModelError::UnknownBlock(id))?;
    match doc.body.get(index).map(|b| &**b) {
        Some(Block::Para(p)) => Ok((index, p)),
        Some(Block::Table(_)) => Err(ModelError::Unsupported("editing tables")),
        None => Err(ModelError::UnknownBlock(id)),
    }
}

/// `offset` as a byte offset into `para`, if it is within the text and on a char boundary.
fn boundary(para: &Paragraph, block: BlockId, offset: u32) -> Result<usize, ModelError> {
    usize::try_from(offset).ok().filter(|off| para.text.is_char_boundary(*off)).ok_or(ModelError::InvalidOffset { block, offset })
}

/// Text an insertion may carry: not empty, no embedded object marker and no control characters
/// other than tab and manual line break. Paragraph breaks travel as `SplitParagraph`, never as
/// characters in the text.
fn check_text(text: &str) -> Result<(), ModelError> {
    if text.is_empty() {
        return Err(ModelError::InvalidText("empty insertion"));
    }
    if text.contains(OBJ) {
        return Err(ModelError::InvalidText("embedded object marker"));
    }
    if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n')) {
        return Err(ModelError::InvalidText("control character"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use uuid::Uuid;
    use wordcraft_doc::{CharProps, InlineObject};

    fn counter() -> impl FnMut() -> BlockId {
        let mut n = 0u128;
        move || {
            n += 1;
            BlockId(Uuid::from_u128(n))
        }
    }

    /// Paragraphs with the given texts, and their registry.
    fn shared(texts: &[&str]) -> (Document, BlockIdentityMap) {
        let mut doc = Document::default();
        for (i, t) in texts.iter().enumerate() {
            if i > 0 {
                let end = doc.para(wordcraft_doc::StoryRef::Body, &wordcraft_doc::Path::top(i - 1)).map_or(0, Paragraph::len);
                doc.split_paragraph(&Pos::body(i - 1, end)).unwrap();
            }
            doc.insert_text(&Pos::body(i, 0), t, &CharProps::default()).unwrap();
        }
        let ids = BlockIdentityMap::assign(&doc, counter()).unwrap();
        (doc, ids)
    }

    fn text(doc: &Document, i: usize) -> String {
        doc.para(wordcraft_doc::StoryRef::Body, &wordcraft_doc::Path::top(i)).unwrap().text.clone()
    }

    fn insert(ids: &[BlockId], i: usize, offset: u32, text: &str) -> DocumentOperation {
        DocumentOperation::InsertText { block_id: ids[i], offset, text: text.into() }
    }

    fn delete(ids: &[BlockId], i: usize, start: u32, end: u32) -> DocumentOperation {
        DocumentOperation::DeleteText { block_id: ids[i], start, end }
    }

    #[test]
    fn design_example_inserts_and_deletes() {
        let (mut doc, mut ids) = shared(&["Hello world", "Second"]);
        let b = ids.ids().to_vec();
        apply(&mut doc, &mut ids, &insert(&b, 0, 6, "beautiful ")).unwrap();
        apply(&mut doc, &mut ids, &insert(&b, 0, 21, "!")).unwrap();
        assert_eq!(text(&doc, 0), "Hello beautiful world!");
        apply(&mut doc, &mut ids, &delete(&b, 0, 5, 15)).unwrap();
        assert_eq!(text(&doc, 0), "Hello world!");
        assert_eq!(text(&doc, 1), "Second");
        // A collapsed deletion is a no-op, not an error.
        apply(&mut doc, &mut ids, &delete(&b, 1, 3, 3)).unwrap();
        assert_eq!(text(&doc, 1), "Second");
    }

    #[test]
    fn multibyte_offsets_are_bytes_on_char_boundaries() {
        let (mut doc, mut ids) = shared(&["ñandú 🦀"]);
        let b = ids.ids().to_vec();
        // "ñ" is 2 bytes, "ú" 2, "🦀" 4.
        apply(&mut doc, &mut ids, &insert(&b, 0, 2, "·")).unwrap();
        assert_eq!(text(&doc, 0), "ñ·andú 🦀");
        let before = doc.clone();
        for bad in [1, 3, 11, 12, 13, 99, u32::MAX] {
            assert!(matches!(apply(&mut doc, &mut ids, &insert(&b, 0, bad, "z")), Err(ModelError::InvalidOffset { .. })), "{bad}");
            assert!(matches!(apply(&mut doc, &mut ids, &delete(&b, 0, 0, bad)), Err(ModelError::InvalidOffset { .. })), "{bad}");
        }
        assert_eq!(doc, before);
        let len = text(&doc, 0).len() as u32;
        apply(&mut doc, &mut ids, &delete(&b, 0, len - 4, len)).unwrap();
        assert_eq!(text(&doc, 0), "ñ·andú ");
    }

    #[test]
    fn invalid_operations_leave_the_document_untouched() {
        let (mut doc, mut ids) = shared(&["Hello", "World"]);
        let b = ids.ids().to_vec();
        let before = (doc.clone(), ids.clone());
        let stranger = BlockId(Uuid::from_u128(999));
        let ops = [
            DocumentOperation::InsertText { block_id: stranger, offset: 0, text: "x".into() },
            DocumentOperation::DeleteText { block_id: stranger, start: 0, end: 1 },
            delete(&b, 0, 4, 2),
            insert(&b, 0, 0, ""),
            insert(&b, 0, 0, "a\u{FFFC}b"),
            insert(&b, 0, 0, "a\rb"),
            insert(&b, 0, 0, "page\u{000C}break"),
            // Structural operations: a split onto an existing id, off a char boundary or out of
            // range; merges of unknown, swapped or same paragraphs, or with a wrong length.
            DocumentOperation::SplitParagraph { block_id: b[0], offset: 2, new_block_id: b[1] },
            DocumentOperation::SplitParagraph { block_id: b[0], offset: 2, new_block_id: b[0] },
            DocumentOperation::SplitParagraph { block_id: b[0], offset: 6, new_block_id: stranger },
            DocumentOperation::SplitParagraph { block_id: stranger, offset: 0, new_block_id: BlockId(Uuid::from_u128(998)) },
            DocumentOperation::MergeParagraphs { first: b[1], second: b[0], first_len: 5 },
            DocumentOperation::MergeParagraphs { first: b[0], second: b[0], first_len: 5 },
            DocumentOperation::MergeParagraphs { first: b[0], second: stranger, first_len: 5 },
            DocumentOperation::MergeParagraphs { first: b[0], second: b[1], first_len: 4 },
            DocumentOperation::MergeParagraphs { first: b[0], second: b[1], first_len: u32::MAX },
        ];
        for op in &ops {
            assert!(apply(&mut doc, &mut ids, op).is_err(), "{op:?}");
            assert_eq!((&doc, &ids), (&before.0, &before.1), "{op:?}");
        }
        // Tabs and manual line breaks are text.
        apply(&mut doc, &mut ids, &insert(&b, 0, 5, "\tand\nmore")).unwrap();
        assert_eq!(text(&doc, 0), "Hello\tand\nmore");
    }

    #[test]
    fn splits_and_merges_keep_the_registry_in_step() {
        let (mut doc, mut ids) = shared(&["Hola ñandú", "🦀 end"]);
        let b = ids.ids().to_vec();
        let new = BlockId(Uuid::from_u128(500));
        // Split after "Hola " (5 bytes): the tail gets the new id, right after its paragraph.
        apply(&mut doc, &mut ids, &DocumentOperation::SplitParagraph { block_id: b[0], offset: 5, new_block_id: new }).unwrap();
        assert_eq!(ids.ids(), [b[0], new, b[1]]);
        assert_eq!((text(&doc, 0), text(&doc, 1), text(&doc, 2)), ("Hola ".into(), "ñandú".into(), "🦀 end".into()));
        ids.check(&doc).unwrap();
        // Splits at either end make an empty paragraph.
        let (start, end) = (BlockId(Uuid::from_u128(501)), BlockId(Uuid::from_u128(502)));
        apply(&mut doc, &mut ids, &DocumentOperation::SplitParagraph { block_id: b[1], offset: 0, new_block_id: start }).unwrap();
        let len = text(&doc, 3).len() as u32;
        apply(&mut doc, &mut ids, &DocumentOperation::SplitParagraph { block_id: start, offset: len, new_block_id: end }).unwrap();
        assert_eq!(ids.ids(), [b[0], new, b[1], start, end]);
        assert_eq!((text(&doc, 2), text(&doc, 4)), (String::new(), String::new()));
        // Merging everything back gives the original text, and the registry its original ids.
        for (first, second, at) in [(start, end, 3), (b[1], start, 2), (new, b[1], 1), (b[0], new, 0)] {
            let first_len = text(&doc, at).len() as u32;
            apply(&mut doc, &mut ids, &DocumentOperation::MergeParagraphs { first, second, first_len }).unwrap();
            ids.check(&doc).unwrap();
        }
        assert_eq!(ids.ids(), [b[0]]);
        assert_eq!(text(&doc, 0), "Hola ñandú🦀 end");
    }

    #[test]
    fn a_split_takes_the_section_break_with_its_tail() {
        let (mut doc, mut ids) = shared(&["one two", "three"]);
        let b = ids.ids().to_vec();
        if let Some(Block::Para(p)) = doc.body.get_mut(0).map(std::sync::Arc::make_mut) {
            p.section = Some(Box::default());
        }
        let new = BlockId(Uuid::from_u128(600));
        apply(&mut doc, &mut ids, &DocumentOperation::SplitParagraph { block_id: b[0], offset: 3, new_block_id: new }).unwrap();
        let section = |i: usize| doc.para(wordcraft_doc::StoryRef::Body, &wordcraft_doc::Path::top(i)).unwrap().section.is_some();
        assert!(!section(0) && section(1));
    }

    #[test]
    fn a_registry_out_of_step_with_the_document_is_refused() {
        let (mut doc, mut ids) = shared(&["One", "Two"]);
        let b = ids.ids().to_vec();
        let op = insert(&b, 0, 0, "x");
        doc.split_paragraph(&Pos::body(1, 1)).unwrap();
        assert!(matches!(apply(&mut doc, &mut ids, &op), Err(ModelError::RegistryMismatch { ids: 2, blocks: 3 })));
    }

    #[test]
    fn embedded_objects_are_kept_out_of_shared_edits() {
        let (mut doc, mut ids) = shared(&["ab"]);
        let b = ids.ids().to_vec();
        doc.insert_object(&Pos::body(0, 1), InlineObject::BookmarkStart { name: "m".into() }, &CharProps::default()).unwrap();
        let obj = OBJ.len_utf8() as u32;
        // Around the object is fine…
        apply(&mut doc, &mut ids, &insert(&b, 0, 1, "<")).unwrap();
        apply(&mut doc, &mut ids, &insert(&b, 0, 2 + obj, ">")).unwrap();
        assert_eq!(text(&doc, 0), "a<\u{FFFC}>b");
        apply(&mut doc, &mut ids, &delete(&b, 0, 0, 2)).unwrap();
        // …deleting it is not, nor is offsetting into its bytes.
        let before = doc.clone();
        assert!(matches!(apply(&mut doc, &mut ids, &delete(&b, 0, 0, obj)), Err(ModelError::Unsupported(_))));
        assert!(matches!(apply(&mut doc, &mut ids, &insert(&b, 0, 1, "x")), Err(ModelError::InvalidOffset { .. })));
        assert_eq!(doc, before);
    }

    #[test]
    fn inserted_text_takes_the_formatting_to_its_left() {
        let mut doc = Document::default();
        let bold = CharProps { bold: Some(true), ..CharProps::default() };
        doc.insert_text(&Pos::body(0, 0), "plain", &CharProps::default()).unwrap();
        doc.insert_text(&Pos::body(0, 5), "BOLD", &bold).unwrap();
        let mut ids = BlockIdentityMap::assign(&doc, counter()).unwrap();
        let b = ids.ids().to_vec();
        apply(&mut doc, &mut ids, &insert(&b, 0, 9, "!")).unwrap();
        let para = doc.para(wordcraft_doc::StoryRef::Body, &wordcraft_doc::Path::top(0)).unwrap();
        assert_eq!(para.props_of_char(9).bold, Some(true));
        assert_eq!(para.props_of_char(0).bold, None);
    }

    /// A random edit against the current state of a one-paragraph document: any byte offsets
    /// (valid or not) and text from a multibyte alphabet.
    fn arb_edit() -> impl Strategy<Value = (bool, u32, u32, String)> {
        (any::<bool>(), 0u32..40, 0u32..40, "[a-zñú🦀 ]{0,4}")
    }

    proptest! {
        /// Two replicas fed the same operations end identical, whether each operation succeeds
        /// or fails — and a failure changes nothing.
        #[test]
        fn replicas_fed_the_same_operations_stay_identical(edits in proptest::collection::vec(arb_edit(), 0..40)) {
            let (mut a, mut ids_a) = shared(&["Hola ñandú 🦀"]);
            let b = ids_a.ids().to_vec();
            let (mut replica, mut ids_b) = (a.clone(), ids_a.clone());
            for (ins, x, y, t) in edits {
                let op = if ins { insert(&b, 0, x, &t) } else { delete(&b, 0, x.min(y), x.max(y)) };
                let before = a.clone();
                let ra = apply(&mut a, &mut ids_a, &op);
                let rb = apply(&mut replica, &mut ids_b, &op);
                prop_assert_eq!(ra.is_ok(), rb.is_ok());
                if ra.is_err() {
                    prop_assert_eq!(&a, &before);
                }
                prop_assert_eq!(&a, &replica);
                // Inserting n bytes grows the text by exactly n; deleting shrinks it by end - start.
                let (old, new) = (text(&before, 0).len(), text(&a, 0).len());
                if ra.is_ok() {
                    match &op {
                        DocumentOperation::InsertText { text, .. } => prop_assert_eq!(new, old + text.len()),
                        DocumentOperation::DeleteText { start, end, .. } => prop_assert_eq!(new, old - (end - start) as usize),
                        _ => {}
                    }
                }
            }
        }
    }
}
