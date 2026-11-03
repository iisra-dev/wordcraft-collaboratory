//! Operation model errors.

use wordcraft_collab_protocol::{BlockId, RejectReason};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("unknown block {0:?}")]
    UnknownBlock(BlockId),
    #[error("block {0:?} already exists")]
    DuplicateBlock(BlockId),
    #[error("offset {offset} is not a valid position in block {block:?}")]
    InvalidOffset { block: BlockId, offset: u32 },
    #[error("block {second:?} does not follow block {first:?}")]
    NotAdjacent { first: BlockId, second: BlockId },
    #[error("text cannot be inserted: {0}")]
    InvalidText(&'static str),
    #[error("the identity registry names {ids} blocks but the document has {blocks}")]
    RegistryMismatch { ids: usize, blocks: usize },
    #[error("no tested transformation for these concurrent operations")]
    Conflict,
    #[error("not supported on shared documents yet: {0}")]
    Unsupported(&'static str),
    #[error("snapshot format {got} is not supported (expected {expected})")]
    SnapshotFormat { got: u32, expected: u32 },
    #[error("malformed snapshot: {0}")]
    Snapshot(#[from] serde_json::Error),
    #[error(transparent)]
    Doc(#[from] wordcraft_doc::DocError),
}

impl ModelError {
    /// The rejection the server reports for this error.
    pub fn reason(&self) -> RejectReason {
        match self {
            Self::Conflict => RejectReason::Conflict,
            Self::Unsupported(_) => RejectReason::UnsupportedOperation,
            Self::UnknownBlock(_)
            | Self::DuplicateBlock(_)
            | Self::InvalidOffset { .. }
            | Self::InvalidText(_)
            | Self::NotAdjacent { .. }
            | Self::RegistryMismatch { .. }
            | Self::SnapshotFormat { .. }
            | Self::Snapshot(_)
            | Self::Doc(_) => RejectReason::InvalidOperation,
        }
    }
}
