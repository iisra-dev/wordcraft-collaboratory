//! Collaborative document operations (design §5.2).
//!
//! The MVP set: text insertion and deletion inside a paragraph, splitting a paragraph and merging
//! two. Offsets are **UTF-8 byte offsets** into the paragraph, like WordCraft's `Pos::off`; never
//! UTF-16 indexes or screen positions. Whether an offset is a valid char boundary (and not inside
//! an embedded object) is checked against the document by `collab-model`, not here.
//!
//! C7 adds basic character formatting ([`DocumentOperation::FormatText`]) and atomic batches
//! ([`DocumentOperation::Batch`]: a paste or a cross-paragraph replacement is one revision, all or
//! nothing). Tables, paragraph formatting and sections come later, each with explicit conflict
//! semantics (§1).

use serde::{Deserialize, Serialize};

use crate::ids::BlockId;

/// One effect on the shared document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentOperation {
    /// Insert `text` at byte `offset` of the paragraph.
    InsertText { block_id: BlockId, offset: u32, text: String },
    /// Delete bytes `start..end` of the paragraph.
    DeleteText { block_id: BlockId, start: u32, end: u32 },
    /// Split the paragraph at byte `offset`; the tail becomes a new paragraph `new_block_id`.
    SplitParagraph { block_id: BlockId, offset: u32, new_block_id: BlockId },
    /// Append paragraph `second` (the one right after `first`) to `first` and remove `second`.
    /// `first_len` is `first`'s byte length before the merge, i.e. where `second`'s text starts
    /// afterwards: concurrent edits in `second` are rebased by it without reading the document,
    /// and it makes the operation invertible (collaborative undo, §15). It must match the
    /// document when applied.
    MergeParagraphs { first: BlockId, second: BlockId, first_len: u32 },
    /// Patch the basic character formatting of bytes `start..end` of the paragraph. Never moves
    /// text, so it never moves another operation.
    FormatText { block_id: BlockId, start: u32, end: u32, format: BasicFormat },
    /// Several operations committed as one revision, all or nothing. Each step applies to the
    /// state the previous step left. Steps may not be batches themselves (checked when applied).
    Batch { steps: Vec<DocumentOperation> },
}

/// Most steps one batch may carry.
pub const MAX_BATCH_STEPS: usize = 4096;

/// A patch of the basic character properties shared documents support (C7.3). An absent field
/// is left as it is; an explicit `false`, `"none"` or `"auto"` turns it off. Values are plain
/// strings and integers so this crate does not depend on the document model; `collab-model`
/// validates them (underline and highlight use WordCraft's camelCase names, colours are
/// `RRGGBB` or `auto`, sizes are half-points like OOXML so no float crosses the wire).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_half_points: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<String>,
}

impl BasicFormat {
    /// Whether the patch changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl DocumentOperation {
    /// The existing paragraphs this operation reads or changes (not the ones it creates).
    pub fn touched_blocks(&self) -> Vec<BlockId> {
        match self {
            Self::InsertText { block_id, .. } | Self::DeleteText { block_id, .. } | Self::SplitParagraph { block_id, .. } => {
                vec![*block_id]
            }
            Self::MergeParagraphs { first, second, .. } => vec![*first, *second],
            Self::FormatText { block_id, .. } => vec![*block_id],
            Self::Batch { steps } => {
                // Paragraphs an earlier step created are not "existing" ones.
                let mut created = Vec::new();
                let mut touched = Vec::new();
                for step in steps.iter().take(MAX_BATCH_STEPS) {
                    for b in step.touched_blocks() {
                        if !created.contains(&b) && !touched.contains(&b) {
                            touched.push(b);
                        }
                    }
                    created.extend(step.created_blocks());
                }
                touched
            }
        }
    }

    /// The paragraphs this operation creates.
    pub fn created_blocks(&self) -> Vec<BlockId> {
        match self {
            Self::SplitParagraph { new_block_id, .. } => vec![*new_block_id],
            Self::Batch { steps } => steps.iter().take(MAX_BATCH_STEPS).flat_map(Self::created_blocks).collect(),
            _ => Vec::new(),
        }
    }
}
