//! Document actors (design §8, milestone C2.2).
//!
//! One logical actor per open document, fed by a Tokio channel, serialises that document's
//! submissions; different documents run concurrently. For each submission:
//!
//! 1. authenticate and authorise (editor or owner);
//! 2. validate the operation and its base revision;
//! 3. transform it over the commits since its base (`wordcraft_collab_model::transform`), or
//!    reject it with `Conflict`;
//! 4. apply it to a *working copy* (`wordcraft_collab_model::apply`);
//! 5. persist the operation and the new revision (one SQLite transaction);
//! 6. only then replace the authoritative state and publish `committed` to every subscriber.
//!
//! A resubmitted `operation_id` returns its original commit without applying it again. Slow
//! subscribers get bounded queues (backpressure) and are dropped, not waited for.

mod actor;

pub use actor::{COMMAND_QUEUE, DocumentHandle, Joined, MAX_HISTORY, Rejection, SNAPSHOT_EVERY, SUBSCRIBER_QUEUE, Submission, open, start};

use wordcraft_collab_model::{BlockId, SharedDocument};
use wordcraft_collab_protocol::DocumentId;
use wordcraft_doc::Document;

use crate::persistence::{Database, DocumentRecord, Timestamp, UserId, repository};
use crate::storage::DocumentStorage;

/// Share `document` (new or imported) as `id`, owned by `owner`: its revision-0 snapshot is
/// written to disk first, then the document, its owner and that snapshot are recorded in one
/// transaction, so a document never exists without a state to [`open`] it from. Blocking.
pub fn create(
    db: &Database,
    storage: &DocumentStorage,
    id: DocumentId,
    title: &str,
    owner: UserId,
    document: Document,
    new_block_id: impl FnMut() -> BlockId,
) -> Result<DocumentRecord, Rejection> {
    // Writing the revision-0 file of an existing document would replace its state: refuse
    // before touching the disk. (Ids are fresh UUIDs; this guards against reuse, not races.)
    if repository::document(&*db.lock()?, id)?.is_some() || storage.has_snapshot(id, 0) {
        return Err(crate::persistence::PersistenceError::AlreadyExists("document").into());
    }
    let shared = SharedDocument::new(document, new_block_id).map_err(|e| Rejection::from_model(&e))?;
    let bytes = shared.to_bytes().map_err(|e| Rejection::from_model(&e))?;
    let stored = storage.write_snapshot(id, 0, &bytes).map_err(Rejection::from_storage)?;
    Ok(repository::create_document_with_snapshot(&mut *db.lock()?, id, title, owner, Some(&stored), &Timestamp::now())?)
}
