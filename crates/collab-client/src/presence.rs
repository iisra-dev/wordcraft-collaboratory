//! Participants and remote carets (design §10, milestone C5.3).
//!
//! Presence travels as structural anchors at a limited rate; it is never a document operation,
//! never persisted and never triggers a save. Remote carets are transformed with each commit
//! (`wordcraft_collab_model::anchors`) so they never point at an invalid position.

use wordcraft_collab_protocol::messages::Participant;

/// The latest participant list the server sent.
#[derive(Clone, Debug, Default)]
pub struct Presence {
    pub participants: Vec<Participant>,
}
