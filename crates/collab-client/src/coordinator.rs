//! Collaboration state and the session bridge (design §6).
//!
//! The coordinator will sit in front of `Session::run` for shared documents (milestone C4):
//! supported commands are captured as a `CommittedEdit` (operations + selection anchor) and
//! submitted; unsupported commands (tables, sections, formatting…) and local undo/redo are
//! disabled while the document is shared; remote commits are applied with a dedicated
//! `apply_remote_edit` that never re-runs user commands, never emits an outgoing event and never
//! calls `Session::set_document` (which would reset the undo history).

use wordcraft_collab_protocol::{DocumentId, Revision};

/// What the status bar shows: connected, sending, offline, read-only or conflict (§10).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CollaborationState {
    /// A local document; nothing is shared.
    #[default]
    Local,
    Connecting,
    /// Joined and up to date with the server at `revision`.
    Synchronized {
        document_id: DocumentId,
        revision: Revision,
    },
    /// Connection lost; edits are paused until resync.
    Reconnecting {
        document_id: DocumentId,
        revision: Revision,
    },
    /// The server refused an operation; the user must be told before editing on.
    Conflict {
        document_id: DocumentId,
        revision: Revision,
    },
    /// Joined as a viewer.
    ReadOnly {
        document_id: DocumentId,
        revision: Revision,
    },
}

impl CollaborationState {
    /// Whether local edits may be submitted right now.
    pub fn can_edit(&self) -> bool {
        matches!(self, Self::Local | Self::Synchronized { .. })
    }

    /// The shared document, if any.
    pub fn document_id(&self) -> Option<DocumentId> {
        match self {
            Self::Local | Self::Connecting => None,
            Self::Synchronized { document_id, .. }
            | Self::Reconnecting { document_id, .. }
            | Self::Conflict { document_id, .. }
            | Self::ReadOnly { document_id, .. } => Some(*document_id),
        }
    }
}
