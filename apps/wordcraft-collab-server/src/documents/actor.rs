//! The document actor (design §8, milestone C2.2).
//!
//! One actor per open document owns its authoritative state — the [`SharedDocument`], its
//! revision and the recent commits — and handles one [`Command`] at a time from a bounded Tokio
//! channel. SQLite is synchronous, so the actor runs on a blocking thread (`spawn_blocking`) and
//! reads its channel with `blocking_recv`; documents run concurrently on separate threads.
//!
//! A submission goes: authorise (editor or owner) → already logged? return the original commit
//! → base revision known? → transform over the commits since → apply to a working copy →
//! persist → replace the state → publish `committed` to every subscriber. Any failure leaves the
//! state as it was. Subscribers have bounded queues; one that falls behind is dropped (its
//! receiver closes), never waited for.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use wordcraft_collab_model::SharedDocument;
use wordcraft_collab_model::transform::transform_all;
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId, RejectReason, Revision, ServerMessage};

use crate::persistence::repository::{self, Commit, NewOperation, StoredOperation};
use crate::persistence::{Database, PersistenceError, Role, Timestamp, UserId};
use crate::storage::DocumentStorage;

/// Messages a subscriber may have queued before it counts as too slow and is dropped.
pub const SUBSCRIBER_QUEUE: usize = 256;

/// Commands waiting for the actor before senders wait (backpressure on submitters).
pub const COMMAND_QUEUE: usize = 1024;

/// Commits kept in memory for transforming late submissions and answering `resync`. A client
/// further behind gets `StaleRevision` on submit and a snapshot on resync.
pub const MAX_HISTORY: usize = 10_000;

/// A snapshot is written after this many commits since the last one (design §9), and when the
/// actor stops with commits not in a snapshot yet.
pub const SNAPSHOT_EVERY: u64 = 100;

/// Why a request was refused; becomes a `rejected` message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub reason: RejectReason,
    pub message: String,
}

impl Rejection {
    fn new(reason: RejectReason, message: impl Into<String>) -> Self {
        Self { reason, message: message.into() }
    }

    pub(crate) fn from_model(e: &wordcraft_collab_model::ModelError) -> Self {
        Self::new(e.reason(), e.to_string())
    }

    pub(crate) fn from_storage(e: crate::storage::StorageError) -> Self {
        Self::unavailable(e)
    }

    fn unavailable(e: impl std::fmt::Display) -> Self {
        tracing::error!("document actor: {e}");
        Self::new(RejectReason::Unavailable, "the server could not complete the request; nothing changed")
    }
}

impl From<PersistenceError> for Rejection {
    fn from(e: PersistenceError) -> Self {
        match e {
            // The request itself is wrong; the server is fine.
            PersistenceError::Invalid { .. } | PersistenceError::AlreadyExists(_) | PersistenceError::NotFound(_) => {
                Self::new(RejectReason::InvalidOperation, e.to_string())
            }
            _ => Self::unavailable(e),
        }
    }
}

/// A submission from an authenticated user.
#[derive(Clone, Debug)]
pub struct Submission {
    pub user: UserId,
    pub client_id: ClientId,
    pub operation_id: OperationId,
    pub base_revision: Revision,
    pub operation: DocumentOperation,
}

/// What `join` gives a participant: the state now, and every commit after it.
pub struct Joined {
    /// A `snapshot` message.
    pub snapshot: ServerMessage,
    pub revision: Revision,
    pub updates: mpsc::Receiver<ServerMessage>,
}

/// The sending side of an actor. Cheap to clone; the actor stops when every handle is gone.
#[derive(Clone)]
pub struct DocumentHandle {
    document_id: DocumentId,
    tx: mpsc::Sender<Command>,
}

enum Command {
    Join { user: UserId, reply: oneshot::Sender<Result<Joined, Rejection>> },
    Submit { submission: Submission, reply: oneshot::Sender<Result<ServerMessage, Rejection>> },
    Resync { user: UserId, since: Revision, reply: oneshot::Sender<Result<Vec<ServerMessage>, Rejection>> },
    Current { reply: oneshot::Sender<(Revision, SharedDocument)> },
}

impl DocumentHandle {
    pub fn document_id(&self) -> DocumentId {
        self.document_id
    }

    /// Enter the document (any member, viewers included).
    pub async fn join(&self, user: UserId) -> Result<Joined, Rejection> {
        self.ask(|reply| Command::Join { user, reply }).await?
    }

    /// Submit an operation; answers with its `committed` message (the original one for an
    /// operation id already committed). Every subscriber, the author included, also receives it.
    pub async fn submit(&self, submission: Submission) -> Result<ServerMessage, Rejection> {
        self.ask(|reply| Command::Submit { submission, reply }).await?
    }

    /// Everything committed after `since` as `committed` messages, or a single `snapshot` if
    /// those commits are no longer in memory.
    pub async fn resync(&self, user: UserId, since: Revision) -> Result<Vec<ServerMessage>, Rejection> {
        self.ask(|reply| Command::Resync { user, since, reply }).await?
    }

    /// The authoritative state (for export and tests).
    pub async fn current(&self) -> Result<(Revision, SharedDocument), Rejection> {
        self.ask(|reply| Command::Current { reply }).await
    }

    async fn ask<T>(&self, command: impl FnOnce(oneshot::Sender<T>) -> Command) -> Result<T, Rejection> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(command(reply)).await.map_err(|_| Rejection::unavailable("document actor stopped"))?;
        answer.await.map_err(|_| Rejection::unavailable("document actor stopped"))
    }
}

/// One commit as the actor keeps it in memory.
#[derive(Clone, Debug)]
struct Entry {
    revision: Revision,
    operation_id: OperationId,
    client_id: ClientId,
    operation: DocumentOperation,
}

impl Entry {
    fn message(&self, document_id: DocumentId) -> ServerMessage {
        ServerMessage::Committed {
            document_id,
            revision: self.revision,
            operation_id: self.operation_id,
            client_id: self.client_id.clone(),
            operation: self.operation.clone(),
        }
    }

    fn from_stored(op: StoredOperation) -> Result<Self, Rejection> {
        // Rows from before migration 0002 have no client id.
        let client_id = match op.client_id {
            Some(c) => c,
            None => ClientId::new("unknown").map_err(Rejection::unavailable)?,
        };
        Ok(Self { revision: op.revision, operation_id: op.operation_id, client_id, operation: op.operation })
    }
}

struct Actor {
    db: Arc<Database>,
    storage: Arc<DocumentStorage>,
    /// Revision of the newest snapshot on disk.
    last_snapshot: Revision,
    document_id: DocumentId,
    revision: Revision,
    shared: SharedDocument,
    /// Commits `history_start + 1 ..= revision`, in order.
    history: Vec<Entry>,
    history_start: Revision,
    subscribers: Vec<mpsc::Sender<ServerMessage>>,
}

/// Start the actor of `document_id` from `base`, its state at `base_revision` (a snapshot, or
/// the document as created at revision 0). Every logged commit after it is replayed, so the
/// actor starts at the document's current revision: this is recovery after a restart.
pub async fn start(
    db: Arc<Database>,
    storage: Arc<DocumentStorage>,
    document_id: DocumentId,
    base: SharedDocument,
    base_revision: Revision,
) -> Result<DocumentHandle, Rejection> {
    let actor =
        tokio::task::spawn_blocking(move || Actor::load(db, storage, document_id, base, base_revision)).await.map_err(Rejection::unavailable)??;
    Ok(spawn(actor))
}

/// Open `document_id` as it was last committed: the newest snapshot that reads back intact (key,
/// checksum, format, registry) and fits the log, plus every commit after it. A damaged snapshot
/// is skipped for an older one, down to the one written at creation (C2.4).
pub async fn open(db: Arc<Database>, storage: Arc<DocumentStorage>, document_id: DocumentId) -> Result<DocumentHandle, Rejection> {
    let actor = tokio::task::spawn_blocking(move || Actor::recover(db, storage, document_id)).await.map_err(Rejection::unavailable)??;
    Ok(spawn(actor))
}

fn spawn(actor: Actor) -> DocumentHandle {
    let document_id = actor.document_id;
    let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
    tokio::task::spawn_blocking(move || actor.run(rx));
    DocumentHandle { document_id, tx }
}

impl Actor {
    fn recover(db: Arc<Database>, storage: Arc<DocumentStorage>, document_id: DocumentId) -> Result<Self, Rejection> {
        let snapshots = repository::snapshots(&*db.lock()?, document_id)?;
        for record in snapshots {
            let attempt = storage
                .read_snapshot(&record.stored)
                .map_err(Rejection::unavailable)
                .and_then(|bytes| SharedDocument::from_bytes(&bytes).map_err(Rejection::unavailable))
                .and_then(|shared| Self::load(db.clone(), storage.clone(), document_id, shared, record.revision));
            match attempt {
                Ok(actor) => return Ok(actor),
                Err(e) => {
                    tracing::warn!("document {document_id:?}: snapshot at revision {} unusable ({}), trying an older one", record.revision, e.message)
                }
            }
        }
        Err(Rejection::unavailable(format!("document {document_id:?} has no usable snapshot")))
    }

    fn load(
        db: Arc<Database>,
        storage: Arc<DocumentStorage>,
        document_id: DocumentId,
        mut shared: SharedDocument,
        base: Revision,
    ) -> Result<Self, Rejection> {
        let (record, log) = {
            let conn = db.lock()?;
            let record =
                repository::document(&conn, document_id)?.ok_or_else(|| Rejection::new(RejectReason::InvalidOperation, "no such document"))?;
            (record, repository::operations_since(&conn, document_id, base, usize::MAX)?)
        };
        let mut revision = base;
        let mut history = Vec::with_capacity(log.len().min(MAX_HISTORY));
        for stored in log {
            if Some(stored.revision) != revision.checked_add(1) {
                return Err(Rejection::unavailable(format!("operation log of {document_id:?} skips from {revision} to {}", stored.revision)));
            }
            shared.apply(&stored.operation).map_err(|e| Rejection::unavailable(format!("replaying revision {}: {e}", stored.revision)))?;
            revision = stored.revision;
            history.push(Entry::from_stored(stored)?);
        }
        if revision != record.current_revision {
            return Err(Rejection::unavailable(format!("{document_id:?} replayed to {revision}, database says {}", record.current_revision)));
        }
        let mut actor =
            Self { db, storage, last_snapshot: base, document_id, revision, shared, history, history_start: base, subscribers: Vec::new() };
        actor.trim_history();
        Ok(actor)
    }

    fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        while let Some(command) = rx.blocking_recv() {
            match command {
                Command::Join { user, reply } => {
                    let _ = reply.send(self.join(user));
                }
                Command::Submit { submission, reply } => match self.submit(submission) {
                    Ok((message, new)) => {
                        if new {
                            self.publish(&message);
                        }
                        let _ = reply.send(Ok(message));
                        if self.revision.saturating_sub(self.last_snapshot) >= SNAPSHOT_EVERY {
                            self.snapshot_to_disk();
                        }
                    }
                    Err(rejection) => {
                        let _ = reply.send(Err(rejection));
                    }
                },
                Command::Resync { user, since, reply } => {
                    let _ = reply.send(self.resync(user, since));
                }
                Command::Current { reply } => {
                    let _ = reply.send((self.revision, self.shared.clone()));
                }
            }
        }
        // Every handle is gone (the document closed or the server is stopping).
        if self.revision > self.last_snapshot {
            self.snapshot_to_disk();
        }
    }

    /// Write and record a snapshot of the current state. A failure is logged and retried at the
    /// next opportunity: the operation log alone already holds every commit.
    fn snapshot_to_disk(&mut self) {
        let result = (|| -> Result<(), String> {
            let bytes = self.shared.to_bytes().map_err(|e| e.to_string())?;
            let stored = self.storage.write_snapshot(self.document_id, self.revision, &bytes).map_err(|e| e.to_string())?;
            repository::record_snapshot(&*self.db.lock().map_err(|e| e.to_string())?, self.document_id, self.revision, &stored, &Timestamp::now())
                .map_err(|e| e.to_string())
        })();
        match result {
            Ok(()) => self.last_snapshot = self.revision,
            Err(e) => tracing::error!("document {:?}: snapshot at revision {} failed: {e}", self.document_id, self.revision),
        }
    }

    fn role(&self, user: UserId) -> Result<Role, Rejection> {
        let role = repository::role_of(&*self.db.lock()?, self.document_id, user)?;
        role.ok_or_else(|| Rejection::new(RejectReason::Forbidden, "not a member of this document"))
    }

    fn snapshot(&self) -> Result<ServerMessage, Rejection> {
        let snapshot = self.shared.to_json().map_err(Rejection::unavailable)?;
        Ok(ServerMessage::Snapshot { document_id: self.document_id, revision: self.revision, snapshot })
    }

    fn join(&mut self, user: UserId) -> Result<Joined, Rejection> {
        self.role(user)?;
        let snapshot = self.snapshot()?;
        let (tx, updates) = mpsc::channel(SUBSCRIBER_QUEUE);
        self.subscribers.push(tx);
        Ok(Joined { snapshot, revision: self.revision, updates })
    }

    /// The `committed` message, and whether it is a new commit (to publish).
    fn submit(&mut self, s: Submission) -> Result<(ServerMessage, bool), Rejection> {
        if !self.role(s.user)?.can_edit() {
            return Err(Rejection::new(RejectReason::Forbidden, "viewers cannot edit"));
        }
        let logged = repository::find_operation(&*self.db.lock()?, self.document_id, s.operation_id)?;
        if let Some(original) = logged {
            return Ok((Entry::from_stored(original)?.message(self.document_id), false));
        }
        if s.base_revision > self.revision {
            return Err(Rejection::new(
                RejectReason::StaleRevision,
                format!("base revision {} is ahead of the server ({})", s.base_revision, self.revision),
            ));
        }
        let skip = usize::try_from(s.base_revision.saturating_sub(self.history_start)).unwrap_or(usize::MAX);
        let since = match self.history.get(skip..) {
            Some(since) if s.base_revision >= self.history_start => since,
            _ => return Err(Rejection::new(RejectReason::StaleRevision, "base revision too old: resync first")),
        };
        let operation = transform_all(&s.operation, since.iter().map(|e| &e.operation)).map_err(|e| Rejection::new(e.reason(), e.to_string()))?;
        let mut working = self.shared.clone();
        working.apply(&operation).map_err(|e| Rejection::new(e.reason(), e.to_string()))?;
        let new = NewOperation { operation_id: s.operation_id, author: s.user, client_id: &s.client_id, operation: &operation };
        let commit = repository::commit_operation(&mut *self.db.lock()?, self.document_id, self.revision, new, &Timestamp::now())?;
        let revision = match commit {
            Commit::New(revision) => revision,
            // Checked above under this actor, the only writer: a bug guard, not a path.
            Commit::Duplicate(original) => return Ok((Entry::from_stored(original)?.message(self.document_id), false)),
        };
        // Persisted: only now does the authoritative state change.
        self.shared = working;
        self.revision = revision;
        let entry = Entry { revision, operation_id: s.operation_id, client_id: s.client_id, operation };
        let message = entry.message(self.document_id);
        self.history.push(entry);
        self.trim_history();
        Ok((message, true))
    }

    fn resync(&self, user: UserId, since: Revision) -> Result<Vec<ServerMessage>, Rejection> {
        self.role(user)?;
        if since > self.revision {
            return Err(Rejection::new(RejectReason::StaleRevision, format!("revision {since} is ahead of the server ({})", self.revision)));
        }
        let skip = usize::try_from(since.saturating_sub(self.history_start)).unwrap_or(usize::MAX);
        match self.history.get(skip..) {
            Some(after) if since >= self.history_start => Ok(after.iter().map(|e| e.message(self.document_id)).collect()),
            _ => Ok(vec![self.snapshot()?]),
        }
    }

    /// Send to every subscriber; drop the ones that are gone or too far behind.
    fn publish(&mut self, message: &ServerMessage) {
        let before = self.subscribers.len();
        self.subscribers.retain(|s| s.try_send(message.clone()).is_ok());
        let dropped = before - self.subscribers.len();
        if dropped > 0 {
            tracing::info!("document {:?}: dropped {dropped} subscriber(s) that left or fell behind", self.document_id);
        }
    }

    fn trim_history(&mut self) {
        let excess = self.history.len().saturating_sub(MAX_HISTORY);
        if excess > 0 {
            self.history.drain(..excess);
            self.history_start = self.history_start.saturating_add(excess as u64);
        }
    }
}
