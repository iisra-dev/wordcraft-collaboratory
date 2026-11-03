//! Validating and applying operations to a document (design §5.2, §6 "Cómo aplicar cambios
//! remotos").
//!
//! The same function runs on the server (on a working copy, before persisting) and on every
//! client (after the commit arrives), so all of them reach the same state. It must:
//! - check the block exists and offsets are in range, on char boundaries and not inside embedded
//!   objects — operations come from the network and are hostile;
//! - apply the change through `wordcraft_doc` edit primitives (`insert_text`, `delete_range`,
//!   `split_paragraph`…), never by replaying UI commands;
//! - keep the [`BlockIdentityMap`] in step (split registers `new_block_id`, merge removes
//!   `second`);
//! - leave the document untouched when it returns an error.
//!
//! Milestone 01 (docs/collab/roadmap.md, C1.2) implements it; until then every operation is
//! refused rather than half-applied.

use wordcraft_collab_protocol::DocumentOperation;
use wordcraft_doc::Document;

use crate::error::ModelError;
use crate::identity::BlockIdentityMap;

/// Apply a committed (or about-to-be-committed) operation.
pub fn apply(doc: &mut Document, ids: &mut BlockIdentityMap, op: &DocumentOperation) -> Result<(), ModelError> {
    ids.check(doc)?;
    for block in op.touched_blocks() {
        ids.path_of(block)?;
    }
    Err(ModelError::Unsupported("applying operations (milestone C1.2)"))
}
