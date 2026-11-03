//! Collaborative document operations (design §5.2).
//!
//! The MVP set: text insertion and deletion inside a paragraph, splitting a paragraph and merging
//! two. Offsets are **UTF-8 byte offsets** into the paragraph, like WordCraft's `Pos::off`; never
//! UTF-16 indexes or screen positions. Whether an offset is a valid char boundary (and not inside
//! an embedded object) is checked against the document by `collab-model`, not here.
//!
//! Formatting, tables and sections come later, each with explicit conflict semantics (§1).

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
    /// Append paragraph `second` to `first` and remove `second`.
    MergeParagraphs { first: BlockId, second: BlockId },
}

impl DocumentOperation {
    /// The existing paragraphs this operation reads or changes (not the ones it creates).
    pub fn touched_blocks(&self) -> Vec<BlockId> {
        match self {
            Self::InsertText { block_id, .. } | Self::DeleteText { block_id, .. } | Self::SplitParagraph { block_id, .. } => {
                vec![*block_id]
            }
            Self::MergeParagraphs { first, second } => vec![*first, *second],
        }
    }
}
