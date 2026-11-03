//! Collaboration state and the client core (design §6, milestone C3.2).
//!
//! [`SharedSession`] is the client side of one shared document, *sans I/O*: it is fed decoded
//! [`ServerMessage`]s and connection events and answers with an [`Output`] — the
//! [`ClientMessage`]s to send and the [`Event`]s for the editor. The transport
//! ([`crate::connection`], native only) just carries those messages; the editor bridge (C4)
//! turns events into changes of the `Session`. So the whole protocol logic is tested without a
//! network and builds for wasm.
//!
//! MVP policy (§6), *confirm before visible apply*: a local edit becomes an operation that is
//! queued ([`crate::PendingQueue`]) and sent; only the server's `committed` changes the replica.
//! Commits are applied strictly in revision order: an old one (a duplicate, or one already in a
//! snapshot) is skipped but still settles its pending entry; a gap asks for `resync`.
//!
//! Reconnecting (C3.3) is a fresh `join` — the server requires one per connection before it
//! accepts submissions — followed by every pending operation again; the server answers the ones
//! it had already committed with their original commit.

use wordcraft_collab_model::SharedDocument;
use wordcraft_collab_protocol::{ClientId, ClientMessage, DocumentId, DocumentOperation, OperationId, RejectReason, Revision, ServerMessage};

use crate::pending::{PendingError, PendingOperation, PendingQueue};

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

/// What the editor must do after a message.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The replica was replaced by a snapshot (joining, or recovering): show it whole.
    Snapshot { revision: Revision },
    /// A commit was applied to the replica; `own` if this client submitted it.
    Applied { revision: Revision, operation: DocumentOperation, own: bool },
    /// The server refused one of this client's operations; the replica is unchanged.
    Rejected { operation: Option<DocumentOperation>, reason: RejectReason, message: String },
    /// The status changed (see [`CollaborationState`]).
    State(CollaborationState),
}

/// The answer to an input: messages to send, in order, and events for the editor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub send: Vec<ClientMessage>,
    pub events: Vec<Event>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubmitError {
    #[error("editing is paused ({0:?})")]
    NotEditable(CollaborationState),
    #[error(transparent)]
    Pending(#[from] PendingError),
}

/// The client side of one shared document.
pub struct SharedSession {
    document_id: DocumentId,
    client_id: ClientId,
    replica: Option<SharedDocument>,
    revision: Revision,
    pending: PendingQueue,
    state: CollaborationState,
    /// Set once the server refused an edit as `forbidden`: a viewer stays read-only across
    /// reconnects.
    viewer: bool,
}

impl SharedSession {
    pub fn new(document_id: DocumentId, client_id: ClientId) -> Self {
        Self {
            document_id,
            client_id,
            replica: None,
            revision: 0,
            pending: PendingQueue::default(),
            state: CollaborationState::Connecting,
            viewer: false,
        }
    }

    pub fn state(&self) -> &CollaborationState {
        &self.state
    }

    /// The shared document as last committed (none before the first snapshot).
    pub fn replica(&self) -> Option<&SharedDocument> {
        self.replica.as_ref()
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn pending(&self) -> &PendingQueue {
        &self.pending
    }

    /// A connection is up: say hello, join, and resend whatever is unacknowledged.
    pub fn connected(&mut self) -> Output {
        let mut out = Output {
            send: vec![ClientMessage::Hello { client_id: self.client_id.clone() }, ClientMessage::Join { document_id: self.document_id }],
            events: Vec::new(),
        };
        out.send.extend(self.pending.iter().map(|p| self.submit_message(p)));
        out
    }

    /// The connection dropped: pause editing until the next snapshot.
    pub fn disconnected(&mut self) -> Output {
        let mut out = Output::default();
        if self.replica.is_some() {
            self.set_state(CollaborationState::Reconnecting { document_id: self.document_id, revision: self.revision }, &mut out);
        } else {
            self.set_state(CollaborationState::Connecting, &mut out);
        }
        out
    }

    /// Queue and send a local edit made against the current replica. `operation_id` must be
    /// fresh (the binaries generate UUIDv7s; this crate has no random source).
    pub fn submit(&mut self, operation_id: OperationId, operation: DocumentOperation) -> Result<ClientMessage, SubmitError> {
        if !self.state.can_edit() || matches!(self.state, CollaborationState::Local) {
            return Err(SubmitError::NotEditable(self.state.clone()));
        }
        let pending = PendingOperation { operation_id, base_revision: self.revision, operation };
        let message = self.submit_message(&pending);
        self.pending.push(pending)?;
        Ok(message)
    }

    /// Clear a conflict once the user has seen it, so editing can go on.
    pub fn acknowledge_conflict(&mut self) -> Output {
        let mut out = Output::default();
        if matches!(self.state, CollaborationState::Conflict { .. }) {
            self.set_state(self.settled_state(), &mut out);
        }
        out
    }

    /// Handle one message from the server.
    pub fn receive(&mut self, message: ServerMessage) -> Output {
        let mut out = Output::default();
        match message {
            ServerMessage::Welcome {} | ServerMessage::Presence { .. } => {}
            ServerMessage::Snapshot { document_id, revision, snapshot } if document_id == self.document_id => {
                match SharedDocument::from_json(snapshot) {
                    Ok(replica) => {
                        self.replica = Some(replica);
                        self.revision = revision;
                        out.events.push(Event::Snapshot { revision });
                        self.set_state(self.settled_state(), &mut out);
                    }
                    // A snapshot that does not restore: report and stay disconnected rather than
                    // edit against a guess.
                    Err(e) => out.events.push(Event::Rejected {
                        operation: None,
                        reason: RejectReason::InvalidOperation,
                        message: format!("unreadable snapshot: {e}"),
                    }),
                }
            }
            ServerMessage::Committed { document_id, revision, operation_id, operation, .. } if document_id == self.document_id => {
                let own = self.pending.settle(operation_id).is_some();
                if revision <= self.revision || self.replica.is_none() {
                    // Already in the replica (a duplicate answer, or covered by the snapshot).
                    return out;
                }
                if revision != self.revision + 1 {
                    out.send.push(ClientMessage::Resync { document_id, since_revision: self.revision });
                    return out;
                }
                let applied = self.replica.as_mut().map(|r| r.apply(&operation));
                match applied {
                    Some(Ok(())) => {
                        self.revision = revision;
                        out.events.push(Event::Applied { revision, operation, own });
                        if let CollaborationState::Synchronized { revision: r, .. } | CollaborationState::ReadOnly { revision: r, .. } =
                            &mut self.state
                        {
                            *r = revision;
                        }
                    }
                    // The server applied it, so the replica has diverged: start over from a snapshot.
                    _ => {
                        self.replica = None;
                        out.send.push(ClientMessage::Join { document_id });
                        self.set_state(CollaborationState::Reconnecting { document_id, revision: self.revision }, &mut out);
                    }
                }
            }
            ServerMessage::Rejected { document_id, operation_id, reason, message } if document_id.is_none_or(|d| d == self.document_id) => {
                let operation = operation_id.and_then(|id| self.pending.settle(id)).map(|p| p.operation);
                match (reason, operation_id) {
                    // The server dropped this subscription (fell behind): catch up.
                    (RejectReason::StaleRevision, None) => {
                        out.send.push(ClientMessage::Resync { document_id: self.document_id, since_revision: self.revision })
                    }
                    (RejectReason::Forbidden, Some(_)) => {
                        self.viewer = true;
                        self.set_state(self.settled_state(), &mut out);
                    }
                    (RejectReason::Conflict, _) => {
                        self.set_state(CollaborationState::Conflict { document_id: self.document_id, revision: self.revision }, &mut out)
                    }
                    _ => {}
                }
                out.events.push(Event::Rejected { operation, reason, message });
            }
            // Another document's message on a shared connection.
            ServerMessage::Snapshot { .. } | ServerMessage::Committed { .. } | ServerMessage::Rejected { .. } => {}
        }
        out
    }

    fn submit_message(&self, p: &PendingOperation) -> ClientMessage {
        ClientMessage::Submit {
            document_id: self.document_id,
            operation_id: p.operation_id,
            client_id: self.client_id.clone(),
            base_revision: p.base_revision,
            operation: p.operation.clone(),
        }
    }

    /// Synchronized, or read-only for a viewer.
    fn settled_state(&self) -> CollaborationState {
        let (document_id, revision) = (self.document_id, self.revision);
        if self.viewer { CollaborationState::ReadOnly { document_id, revision } } else { CollaborationState::Synchronized { document_id, revision } }
    }

    fn set_state(&mut self, state: CollaborationState, out: &mut Output) {
        if self.state != state {
            self.state = state.clone();
            out.events.push(Event::State(state));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use wordcraft_collab_model::BlockId;
    use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};

    const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));
    const BLOCK: BlockId = BlockId(Uuid::from_u128(0xb1));

    fn server_state() -> SharedDocument {
        let mut doc = Document::default();
        doc.insert_text(&Pos::body(0, 0), "Hello", &CharProps::default()).unwrap();
        SharedDocument::new(doc, || BLOCK).unwrap()
    }

    fn snapshot(revision: Revision, state: &SharedDocument) -> ServerMessage {
        ServerMessage::Snapshot { document_id: DOC, revision, snapshot: state.to_json().unwrap() }
    }

    fn insert(offset: u32, text: &str) -> DocumentOperation {
        DocumentOperation::InsertText { block_id: BLOCK, offset, text: text.into() }
    }

    fn op_id(n: u128) -> OperationId {
        OperationId(Uuid::from_u128(n))
    }

    fn committed(revision: Revision, n: u128, client: &str, operation: DocumentOperation) -> ServerMessage {
        ServerMessage::Committed { document_id: DOC, revision, operation_id: op_id(n), client_id: ClientId::new(client).unwrap(), operation }
    }

    fn rejected(n: Option<u128>, reason: RejectReason) -> ServerMessage {
        ServerMessage::Rejected { document_id: Some(DOC), operation_id: n.map(op_id), reason, message: "no".into() }
    }

    fn text(s: &SharedSession) -> String {
        s.replica().unwrap().document().para(StoryRef::Body, &Path::top(0)).unwrap().text.clone()
    }

    /// A session joined at `revision`.
    fn joined(revision: Revision) -> SharedSession {
        let mut s = SharedSession::new(DOC, ClientId::new("desktop-a").unwrap());
        let out = s.connected();
        assert_eq!(out.send, [ClientMessage::Hello { client_id: ClientId::new("desktop-a").unwrap() }, ClientMessage::Join { document_id: DOC }]);
        let out = s.receive(snapshot(revision, &server_state()));
        assert_eq!(out.events, [Event::Snapshot { revision }, Event::State(CollaborationState::Synchronized { document_id: DOC, revision })]);
        s
    }

    #[test]
    fn edits_apply_only_when_committed() {
        let mut s = joined(4);
        let message = s.submit(op_id(1), insert(5, "!")).unwrap();
        assert!(matches!(message, ClientMessage::Submit { base_revision: 4, .. }));
        // Confirm before apply: nothing changes until the commit.
        assert_eq!((text(&s), s.pending().len()), ("Hello".into(), 1));
        let out = s.receive(committed(5, 1, "desktop-a", insert(5, "!")));
        assert_eq!(out.events, [Event::Applied { revision: 5, operation: insert(5, "!"), own: true }]);
        assert_eq!((text(&s), s.pending().len(), s.revision()), ("Hello!".into(), 0, 5));
        assert_eq!(s.state(), &CollaborationState::Synchronized { document_id: DOC, revision: 5 });
        // Someone else's commit.
        let out = s.receive(committed(6, 2, "desktop-b", insert(0, ">")));
        assert_eq!(out.events, [Event::Applied { revision: 6, operation: insert(0, ">"), own: false }]);
        assert_eq!(text(&s), ">Hello!");
    }

    #[test]
    fn old_commits_are_skipped_but_settle_and_gaps_resync() {
        let mut s = joined(4);
        s.submit(op_id(1), insert(0, "x")).unwrap();
        // The original commit of a resend, already in the snapshot.
        assert_eq!(s.receive(committed(3, 1, "desktop-a", insert(0, "x"))), Output::default());
        assert!(s.pending().is_empty());
        assert_eq!(text(&s), "Hello");
        // Revision 6 before 5: ask for what was missed, apply nothing.
        let out = s.receive(committed(6, 9, "desktop-b", insert(0, "y")));
        assert_eq!(out.send, [ClientMessage::Resync { document_id: DOC, since_revision: 4 }]);
        assert!(out.events.is_empty());
        assert_eq!(s.revision(), 4);
        // Dropped by the server for falling behind: same.
        assert_eq!(s.receive(rejected(None, RejectReason::StaleRevision)).send, [ClientMessage::Resync { document_id: DOC, since_revision: 4 }]);
    }

    #[test]
    fn a_conflict_pauses_editing_until_acknowledged() {
        let mut s = joined(1);
        s.submit(op_id(1), insert(0, "x")).unwrap();
        let out = s.receive(rejected(Some(1), RejectReason::Conflict));
        assert_eq!(
            out.events,
            [
                Event::State(CollaborationState::Conflict { document_id: DOC, revision: 1 }),
                Event::Rejected { operation: Some(insert(0, "x")), reason: RejectReason::Conflict, message: "no".into() },
            ]
        );
        assert!(s.pending().is_empty());
        assert!(matches!(s.submit(op_id(2), insert(0, "y")), Err(SubmitError::NotEditable(_))));
        s.acknowledge_conflict();
        assert!(s.submit(op_id(2), insert(0, "y")).is_ok());
    }

    #[test]
    fn a_viewer_becomes_and_stays_read_only() {
        let mut s = joined(1);
        s.submit(op_id(1), insert(0, "x")).unwrap();
        s.receive(rejected(Some(1), RejectReason::Forbidden));
        assert_eq!(s.state(), &CollaborationState::ReadOnly { document_id: DOC, revision: 1 });
        assert!(s.submit(op_id(2), insert(0, "y")).is_err());
        // Commits still arrive and apply; the revision follows.
        s.receive(committed(2, 7, "desktop-b", insert(0, ">")));
        assert_eq!(s.state(), &CollaborationState::ReadOnly { document_id: DOC, revision: 2 });
        s.disconnected();
        s.connected();
        s.receive(snapshot(2, &server_state()));
        assert!(matches!(s.state(), CollaborationState::ReadOnly { .. }));
    }

    #[test]
    fn reconnecting_rejoins_and_resends_pending_operations() {
        let mut s = joined(1);
        let first = s.submit(op_id(1), insert(0, "a")).unwrap();
        let second = s.submit(op_id(2), insert(5, "b")).unwrap();
        let out = s.disconnected();
        assert_eq!(out.events, [Event::State(CollaborationState::Reconnecting { document_id: DOC, revision: 1 })]);
        assert!(s.submit(op_id(3), insert(0, "c")).is_err());
        let out = s.connected();
        assert_eq!(out.send[2..], [first, second]);
        // The server had committed the first before the drop: the snapshot has it, its resend
        // answers with the original, the second commits now.
        let mut state = server_state();
        state.apply(&insert(0, "a")).unwrap();
        s.receive(snapshot(2, &state));
        s.receive(committed(2, 1, "desktop-a", insert(0, "a")));
        s.receive(committed(3, 2, "desktop-a", insert(6, "b")));
        assert_eq!((text(&s), s.revision(), s.pending().len()), ("aHellob".into(), 3, 0));
    }

    #[test]
    fn broken_input_never_corrupts_the_replica() {
        let mut s = joined(1);
        // A snapshot that doesn't restore: reported, replica kept.
        let out = s.receive(ServerMessage::Snapshot { document_id: DOC, revision: 9, snapshot: serde_json::json!({"format": 1}) });
        assert!(matches!(out.events.as_slice(), [Event::Rejected { reason: RejectReason::InvalidOperation, .. }]));
        assert_eq!((s.revision(), text(&s)), (1, "Hello".into()));
        // Another document's messages are not ours.
        let other = DocumentId(Uuid::from_u128(5));
        let foreign = ServerMessage::Committed {
            document_id: other,
            revision: 2,
            operation_id: op_id(9),
            client_id: ClientId::new("x").unwrap(),
            operation: insert(0, "z"),
        };
        assert_eq!(s.receive(foreign), Output::default());
        // A commit that doesn't fit the replica means it diverged: start again from a snapshot.
        let out = s.receive(committed(2, 8, "desktop-b", insert(99, "far")));
        assert_eq!(out.send, [ClientMessage::Join { document_id: DOC }]);
        assert!(s.replica().is_none());
        assert_eq!(s.state(), &CollaborationState::Reconnecting { document_id: DOC, revision: 1 });
    }
}
