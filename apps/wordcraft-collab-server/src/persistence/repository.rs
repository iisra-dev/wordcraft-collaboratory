//! The SQLite repository (milestone C2.1): users, documents, members and the operation log.
//!
//! Functions take a connection from [`super::Database::lock`] and are blocking (call them from
//! the document actor's thread or `spawn_blocking`). Anything with more than one write runs in
//! its own transaction, so a failure leaves nothing half-written.
//!
//! The operation log is the heart of durability (design §7, §9): [`commit_operation`] writes the
//! operation at the next revision and moves `documents.current_revision` in one transaction,
//! refuses if the document is not at the revision the caller (the document actor) expects, and
//! returns the *original* commit for an `operation_id` it already holds, without writing again.
//! Revisions are therefore dense and strictly increasing: 1, 2, 3… per document.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, params};
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId, Revision};

use super::PersistenceError;
use crate::storage::StoredSnapshot;

type Result<T> = std::result::Result<T, PersistenceError>;

/// Longest document title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

/// Longest display name, in characters.
const MAX_NAME_CHARS: usize = 100;

/// A user account (OIDC subject or local).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UserId(pub uuid::Uuid);

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A member's role in one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    Editor,
    Viewer,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "owner" => Ok(Self::Owner),
            "editor" => Ok(Self::Editor),
            "viewer" => Ok(Self::Viewer),
            other => Err(PersistenceError::Corrupt(format!("unknown role {other:?}"))),
        }
    }

    /// May submit operations.
    pub fn can_edit(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }

    /// May change members, rename, delete.
    pub fn can_manage(self) -> bool {
        self == Self::Owner
    }
}

/// An RFC 3339 UTC timestamp, as stored.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(String);

impl Timestamp {
    pub fn now() -> Self {
        Self::from(std::time::SystemTime::now())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<std::time::SystemTime> for Timestamp {
    fn from(t: std::time::SystemTime) -> Self {
        let utc: chrono::DateTime<chrono::Utc> = t.into();
        Self(utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub oidc_subject: Option<String>,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRecord {
    pub id: DocumentId,
    pub title: String,
    pub owner: UserId,
    pub current_revision: Revision,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One row of the operation log.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredOperation {
    pub revision: Revision,
    pub operation_id: OperationId,
    pub author: UserId,
    pub client_id: Option<ClientId>,
    pub operation: DocumentOperation,
}

/// An operation to log, as the actor received it.
#[derive(Clone, Copy, Debug)]
pub struct NewOperation<'a> {
    pub operation_id: OperationId,
    pub author: UserId,
    pub client_id: &'a ClientId,
    /// After transformation: what is applied and published.
    pub operation: &'a DocumentOperation,
}

/// What [`commit_operation`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Commit {
    /// Written as this revision.
    New(Revision),
    /// Already in the log (a resubmission after a reconnect): the original, untouched.
    Duplicate(StoredOperation),
}

// --- users -----------------------------------------------------------------------------------

/// Add a user. An OIDC subject belongs to one user only.
pub fn create_user(conn: &Connection, user: &User, now: &Timestamp) -> Result<()> {
    let name = clean_text("display name", &user.display_name, MAX_NAME_CHARS)?;
    let inserted = conn.execute(
        "INSERT INTO users (id, oidc_subject, display_name, created_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
        params![user.id.to_string(), user.oidc_subject, name, now.as_str()],
    )?;
    if inserted == 0 {
        return Err(PersistenceError::AlreadyExists("user"));
    }
    audit(conn, None, AuditAction::UserCreated, None, Some(user.id), if user.oidc_subject.is_some() { "oidc" } else { "local" }, now)
}

pub fn user(conn: &Connection, id: UserId) -> Result<Option<User>> {
    conn.query_row("SELECT id, oidc_subject, display_name FROM users WHERE id = ?1", [id.to_string()], user_row).optional()?.transpose()
}

pub fn user_by_subject(conn: &Connection, subject: &str) -> Result<Option<User>> {
    conn.query_row("SELECT id, oidc_subject, display_name FROM users WHERE oidc_subject = ?1", [subject], user_row).optional()?.transpose()
}

fn user_row(r: &Row<'_>) -> rusqlite::Result<Result<User>> {
    let (id, oidc_subject, display_name): (String, Option<String>, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
    Ok(parse_uuid(&id).map(|id| User { id: UserId(id), oidc_subject, display_name }))
}

/// Record a sign-in token for `user` by its SHA-256 (see [`crate::auth`]).
pub fn add_token(conn: &Connection, user: UserId, token_sha256: &str, now: &Timestamp) -> Result<()> {
    if self::user(conn, user)?.is_none() {
        return Err(PersistenceError::NotFound("user"));
    }
    conn.execute(
        "INSERT INTO user_tokens (token_sha256, user_id, created_at) VALUES (?1, ?2, ?3)",
        params![token_sha256, user.to_string(), now.as_str()],
    )?;
    Ok(())
}

/// The user a token's SHA-256 belongs to.
pub fn user_by_token(conn: &Connection, token_sha256: &str) -> Result<Option<UserId>> {
    let id: Option<String> = conn.query_row("SELECT user_id FROM user_tokens WHERE token_sha256 = ?1", [token_sha256], |r| r.get(0)).optional()?;
    id.map(|id| parse_uuid(&id).map(UserId)).transpose()
}

// --- documents and members -------------------------------------------------------------------

/// Create a document at revision 0, owned by `owner` (who becomes its first member).
pub fn create_document(conn: &mut Connection, id: DocumentId, title: &str, owner: UserId, now: &Timestamp) -> Result<DocumentRecord> {
    create_document_with_snapshot(conn, id, title, owner, None, now)
}

/// [`create_document`] recording, in the same transaction, the snapshot of its revision 0 (the
/// document as created or imported), so a document never exists without a state to start from.
pub fn create_document_with_snapshot(
    conn: &mut Connection,
    id: DocumentId,
    title: &str,
    owner: UserId,
    initial: Option<&StoredSnapshot>,
    now: &Timestamp,
) -> Result<DocumentRecord> {
    let title = clean_text("title", title, MAX_TITLE_CHARS)?;
    let tx = conn.transaction()?;
    if user(&tx, owner)?.is_none() {
        return Err(PersistenceError::NotFound("user"));
    }
    let inserted = tx.execute(
        "INSERT INTO documents (id, title, owner_id, current_revision, created_at, updated_at) VALUES (?1, ?2, ?3, 0, ?4, ?4) ON CONFLICT DO NOTHING",
        params![id.0.to_string(), title, owner.to_string(), now.as_str()],
    )?;
    if inserted == 0 {
        return Err(PersistenceError::AlreadyExists("document"));
    }
    tx.execute("INSERT INTO document_members (document_id, user_id, role) VALUES (?1, ?2, 'owner')", params![id.0.to_string(), owner.to_string()])?;
    if let Some(snapshot) = initial {
        record_snapshot(&tx, id, 0, snapshot, now)?;
    }
    audit(&tx, Some(owner), AuditAction::DocumentCreated, Some(id), None, "", now)?;
    tx.commit()?;
    Ok(DocumentRecord { id, title, owner, current_revision: 0, created_at: now.clone(), updated_at: now.clone() })
}

const DOCUMENT_COLUMNS: &str = "d.id, d.title, d.owner_id, d.current_revision, d.created_at, d.updated_at";

pub fn document(conn: &Connection, id: DocumentId) -> Result<Option<DocumentRecord>> {
    conn.query_row(&format!("SELECT {DOCUMENT_COLUMNS} FROM documents d WHERE d.id = ?1"), [id.0.to_string()], document_row).optional()?.transpose()
}

/// The documents `user` is a member of, with their role, most recently changed first.
pub fn documents_for(conn: &Connection, user: UserId) -> Result<Vec<(DocumentRecord, Role)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {DOCUMENT_COLUMNS}, m.role FROM document_members m JOIN documents d ON d.id = m.document_id
         WHERE m.user_id = ?1 ORDER BY d.updated_at DESC, d.id"
    ))?;
    let rows = stmt.query_map([user.to_string()], |r| {
        let role: String = r.get(6)?;
        Ok(document_row(r)?.and_then(|d| Ok((d, Role::parse(&role)?))))
    })?;
    rows.map(|r| r?).collect()
}

fn document_row(r: &Row<'_>) -> rusqlite::Result<Result<DocumentRecord>> {
    let (id, title, owner, revision, created, updated): (String, String, String, i64, String, String) =
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
    Ok((|| {
        Ok(DocumentRecord {
            id: DocumentId(parse_uuid(&id)?),
            title,
            owner: UserId(parse_uuid(&owner)?),
            current_revision: revision_from_db(revision)?,
            created_at: Timestamp(created),
            updated_at: Timestamp(updated),
        })
    })())
}

/// `user`'s role in `document`, if a member.
pub fn role_of(conn: &Connection, document: DocumentId, user: UserId) -> Result<Option<Role>> {
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM document_members WHERE document_id = ?1 AND user_id = ?2",
            params![document.0.to_string(), user.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    role.map(|r| Role::parse(&r)).transpose()
}

/// Everyone with access to `document` and their role: the owner first, then by name.
pub fn members(conn: &Connection, document: DocumentId) -> Result<Vec<(User, Role)>> {
    let mut stmt = conn.prepare(
        "SELECT u.id, u.oidc_subject, u.display_name, m.role FROM document_members m JOIN users u ON u.id = m.user_id
         WHERE m.document_id = ?1 ORDER BY m.role <> 'owner', u.display_name COLLATE NOCASE, u.id",
    )?;
    let rows = stmt.query_map([document.0.to_string()], |r| {
        let user = user_row(r)?;
        let role: String = r.get(3)?;
        Ok(user.and_then(|u| Ok((u, Role::parse(&role)?))))
    })?;
    rows.map(|r| r?).collect()
}

/// Most users a search returns.
pub const MAX_USER_MATCHES: usize = 20;

/// Users to invite (C5.5): the one whose id is `query`, or those whose display name contains it
/// (ignoring ASCII case), by name; at most `limit` (capped at [`MAX_USER_MATCHES`]).
pub fn find_users(conn: &Connection, query: &str, limit: usize) -> Result<Vec<User>> {
    let limit = i64::try_from(limit.min(MAX_USER_MATCHES)).unwrap_or(0);
    if let Ok(id) = uuid::Uuid::parse_str(query.trim()) {
        return Ok(self::user(conn, UserId(id))?.into_iter().collect());
    }
    // LIKE with the wildcards in the query taken literally.
    let escaped: String = query.trim().chars().flat_map(|c| if matches!(c, '%' | '_' | '\\') { vec!['\\', c] } else { vec![c] }).collect();
    let mut stmt = conn.prepare(
        "SELECT id, oidc_subject, display_name FROM users WHERE display_name LIKE ?1 ESCAPE '\\'
         ORDER BY display_name COLLATE NOCASE, id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![format!("%{escaped}%"), limit], user_row)?;
    rows.map(|r| r?).collect()
}

/// Add a member or change their role. Ownership is not transferred this way: the owner's role
/// cannot change and nobody else becomes an owner.
pub fn set_member(conn: &mut Connection, document: DocumentId, user: UserId, role: Role) -> Result<()> {
    if role == Role::Owner {
        return Err(PersistenceError::Invalid { field: "role", reason: "a document has exactly one owner" });
    }
    let tx = conn.transaction()?;
    let record = self::document(&tx, document)?.ok_or(PersistenceError::NotFound("document"))?;
    if record.owner == user {
        return Err(PersistenceError::Invalid { field: "role", reason: "the owner's role cannot change" });
    }
    if self::user(&tx, user)?.is_none() {
        return Err(PersistenceError::NotFound("user"));
    }
    tx.execute(
        "INSERT INTO document_members (document_id, user_id, role) VALUES (?1, ?2, ?3)
         ON CONFLICT (document_id, user_id) DO UPDATE SET role = excluded.role",
        params![document.0.to_string(), user.to_string(), role.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Remove a member (not the owner). Returns whether they were one.
pub fn remove_member(conn: &Connection, document: DocumentId, user: UserId) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM document_members WHERE document_id = ?1 AND user_id = ?2 AND role <> 'owner'",
        params![document.0.to_string(), user.to_string()],
    )?;
    Ok(removed > 0)
}

// --- operation log ---------------------------------------------------------------------------

/// Persist `operation` as the revision after `expected` — the revision the caller's state is at.
///
/// In one transaction: an `operation_id` already logged returns [`Commit::Duplicate`] with the
/// original row and writes nothing; a document at another revision is a
/// [`PersistenceError::RevisionMismatch`] (two writers, which the single actor per document
/// rules out, so this is a bug guard); otherwise the row is written and `current_revision` and
/// `updated_at` move on.
pub fn commit_operation(conn: &mut Connection, document: DocumentId, expected: Revision, new: NewOperation<'_>, now: &Timestamp) -> Result<Commit> {
    let NewOperation { operation_id, author, client_id, operation } = new;
    let payload = serde_json::to_vec(operation).map_err(|e| PersistenceError::Corrupt(e.to_string()))?;
    let doc_key = document.0.to_string();
    let tx = conn.transaction()?;
    if let Some(original) = find_operation(&tx, document, operation_id)? {
        return Ok(Commit::Duplicate(original));
    }
    let found: i64 = tx
        .query_row("SELECT current_revision FROM documents WHERE id = ?1", [&doc_key], |r| r.get(0))
        .optional()?
        .ok_or(PersistenceError::NotFound("document"))?;
    let found = revision_from_db(found)?;
    if found != expected {
        return Err(PersistenceError::RevisionMismatch { expected, found });
    }
    let revision = expected.checked_add(1).ok_or(PersistenceError::Invalid { field: "revision", reason: "overflow" })?;
    let db_revision = revision_to_db(revision)?;
    tx.execute(
        "INSERT INTO document_operations (document_id, revision, operation_id, author_id, client_id, payload, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![doc_key, db_revision, operation_id.0.to_string(), author.to_string(), client_id.as_str(), payload, now.as_str()],
    )?;
    tx.execute("UPDATE documents SET current_revision = ?2, updated_at = ?3 WHERE id = ?1", params![doc_key, db_revision, now.as_str()])?;
    tx.commit()?;
    Ok(Commit::New(revision))
}

const OPERATION_COLUMNS: &str = "revision, operation_id, author_id, client_id, payload";

/// The logged operation with this id, if any.
pub fn find_operation(conn: &Connection, document: DocumentId, operation_id: OperationId) -> Result<Option<StoredOperation>> {
    conn.query_row(
        &format!("SELECT {OPERATION_COLUMNS} FROM document_operations WHERE document_id = ?1 AND operation_id = ?2"),
        params![document.0.to_string(), operation_id.0.to_string()],
        operation_row,
    )
    .optional()?
    .transpose()
}

/// Up to `limit` operations committed after `after`, in revision order (for `resync` and for
/// replaying the log over a snapshot).
pub fn operations_since(conn: &Connection, document: DocumentId, after: Revision, limit: usize) -> Result<Vec<StoredOperation>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {OPERATION_COLUMNS} FROM document_operations WHERE document_id = ?1 AND revision > ?2 ORDER BY revision LIMIT ?3"
    ))?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![document.0.to_string(), revision_to_db(after)?, limit], operation_row)?;
    rows.map(|r| r?).collect()
}

fn operation_row(r: &Row<'_>) -> rusqlite::Result<Result<StoredOperation>> {
    let (revision, operation_id, author, client_id, payload): (i64, String, String, String, Vec<u8>) =
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
    Ok((|| {
        Ok(StoredOperation {
            revision: revision_from_db(revision)?,
            operation_id: OperationId(parse_uuid(&operation_id)?),
            author: UserId(parse_uuid(&author)?),
            // Rows from before migration 0002 have no client id.
            client_id: if client_id.is_empty() {
                None
            } else {
                Some(ClientId::new(client_id).map_err(|e| PersistenceError::Corrupt(e.to_string()))?)
            },
            operation: serde_json::from_slice(&payload).map_err(|e| PersistenceError::Corrupt(format!("operation payload: {e}")))?,
        })
    })())
}

// --- snapshots -------------------------------------------------------------------------------

/// A snapshot row: which revision, where, and its checksum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub revision: Revision,
    pub stored: StoredSnapshot,
}

/// Record a snapshot written by [`crate::storage::DocumentStorage::write_snapshot`]. Recording
/// the same revision again replaces the row (the file was rewritten with the same content).
pub fn record_snapshot(conn: &Connection, document: DocumentId, revision: Revision, stored: &StoredSnapshot, now: &Timestamp) -> Result<()> {
    conn.execute(
        "INSERT INTO document_snapshots (document_id, revision, storage_key, sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (document_id, revision) DO UPDATE SET storage_key = excluded.storage_key, sha256 = excluded.sha256, created_at = excluded.created_at",
        params![document.0.to_string(), revision_to_db(revision)?, stored.storage_key, stored.sha256, now.as_str()],
    )?;
    Ok(())
}

/// The document's snapshots, newest first (recovery tries them in this order).
pub fn snapshots(conn: &Connection, document: DocumentId) -> Result<Vec<SnapshotRecord>> {
    let mut stmt = conn.prepare("SELECT revision, storage_key, sha256 FROM document_snapshots WHERE document_id = ?1 ORDER BY revision DESC")?;
    let rows = stmt.query_map([document.0.to_string()], |r| {
        let (revision, storage_key, sha256): (i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        Ok(revision_from_db(revision).map(|revision| SnapshotRecord { revision, stored: StoredSnapshot { storage_key, sha256 } }))
    })?;
    rows.map(|r| r?).collect()
}

// --- audit -----------------------------------------------------------------------------------

/// Something the audit trail records (C5.4): actions and ids, never content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAction {
    UserCreated,
    DocumentCreated,
    MemberSet,
    MemberRemoved,
    DocumentExported,
}

impl AuditAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserCreated => "user_created",
            Self::DocumentCreated => "document_created",
            Self::MemberSet => "member_set",
            Self::MemberRemoved => "member_removed",
            Self::DocumentExported => "document_exported",
        }
    }
}

/// One audit row as read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    pub at: String,
    pub actor: Option<UserId>,
    pub action: String,
    pub subject: Option<UserId>,
    pub detail: String,
}

/// Record `action` by `actor` (none for the system), on `document` and/or `subject` (a user).
/// `detail` is a short, non-sensitive note (a role name), capped at 100 characters.
pub fn audit(
    conn: &Connection,
    actor: Option<UserId>,
    action: AuditAction,
    document: Option<DocumentId>,
    subject: Option<UserId>,
    detail: &str,
    now: &Timestamp,
) -> Result<()> {
    let detail: String = detail.chars().filter(|c| !c.is_control()).take(100).collect();
    conn.execute(
        "INSERT INTO audit_log (at, actor_id, action, document_id, subject_id, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            now.as_str(),
            actor.map(|u| u.to_string()),
            action.as_str(),
            document.map(|d| d.0.to_string()),
            subject.map(|u| u.to_string()),
            detail
        ],
    )?;
    Ok(())
}

/// A document's audit trail, oldest first, at most `limit` rows after row `after`.
pub fn audit_for(conn: &Connection, document: DocumentId, after: i64, limit: usize) -> Result<Vec<(i64, AuditEntry)>> {
    let mut stmt =
        conn.prepare("SELECT id, at, actor_id, action, subject_id, detail FROM audit_log WHERE document_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3")?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![document.0.to_string(), after, limit], |r| {
        let (id, at, actor, action, subject, detail): (i64, String, Option<String>, String, Option<String>, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
        Ok((|| {
            let actor = actor.map(|a| parse_uuid(&a).map(UserId)).transpose()?;
            let subject = subject.map(|s| parse_uuid(&s).map(UserId)).transpose()?;
            Ok((id, AuditEntry { at, actor, action, subject, detail }))
        })())
    })?;
    rows.map(|r| r?).collect()
}

// --- helpers ---------------------------------------------------------------------------------

fn parse_uuid(s: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|e| PersistenceError::Corrupt(format!("id {s:?}: {e}")))
}

fn revision_from_db(r: i64) -> Result<Revision> {
    Revision::try_from(r).map_err(|_| PersistenceError::Corrupt(format!("negative revision {r}")))
}

fn revision_to_db(r: Revision) -> Result<i64> {
    i64::try_from(r).map_err(|_| PersistenceError::Invalid { field: "revision", reason: "too large" })
}

/// User-supplied text: trimmed, not empty, at most `max` characters, no control characters.
fn clean_text(field: &'static str, text: &str, max: usize) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(PersistenceError::Invalid { field, reason: "empty" });
    }
    if text.chars().count() > max {
        return Err(PersistenceError::Invalid { field, reason: "too long" });
    }
    if text.chars().any(char::is_control) {
        return Err(PersistenceError::Invalid { field, reason: "control characters" });
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::Database;
    use uuid::Uuid;
    use wordcraft_collab_protocol::BlockId;

    fn ts(s: &str) -> Timestamp {
        Timestamp(s.into())
    }

    fn user_n(n: u128, name: &str) -> User {
        User { id: UserId(Uuid::from_u128(n)), oidc_subject: Some(format!("sub-{n}")), display_name: name.into() }
    }

    fn doc_id(n: u128) -> DocumentId {
        DocumentId(Uuid::from_u128(0xd0c0 + n))
    }

    fn op_id(n: u128) -> OperationId {
        OperationId(Uuid::from_u128(0x0900 + n))
    }

    fn insert(text: &str) -> DocumentOperation {
        DocumentOperation::InsertText { block_id: BlockId(Uuid::from_u128(1)), offset: 0, text: text.into() }
    }

    /// A database with Ana (owner of document 1) and Bea.
    fn setup() -> (Database, User, User) {
        let db = Database::open_in_memory().unwrap();
        let (ana, bea) = (user_n(1, "Ana"), user_n(2, "Bea"));
        {
            let mut conn = db.lock().unwrap();
            create_user(&conn, &ana, &ts("2026-11-03T10:00:00.000Z")).unwrap();
            create_user(&conn, &bea, &ts("2026-11-03T10:00:00.000Z")).unwrap();
            create_document(&mut conn, doc_id(1), "  Plan  ", ana.id, &ts("2026-11-03T10:00:01.000Z")).unwrap();
        }
        (db, ana, bea)
    }

    #[test]
    fn users_are_found_by_id_and_subject_and_never_duplicated() {
        let (db, ana, _) = setup();
        let conn = db.lock().unwrap();
        assert_eq!(user(&conn, ana.id).unwrap(), Some(ana.clone()));
        assert_eq!(user_by_subject(&conn, "sub-1").unwrap(), Some(ana.clone()));
        assert_eq!(user_by_subject(&conn, "nobody").unwrap(), None);
        let now = Timestamp::now();
        assert!(matches!(create_user(&conn, &ana, &now), Err(PersistenceError::AlreadyExists(_))));
        let same_subject = User { id: UserId(Uuid::from_u128(9)), ..ana.clone() };
        assert!(matches!(create_user(&conn, &same_subject, &now), Err(PersistenceError::AlreadyExists(_))));
        for bad in ["", "   ", "a\u{0}b", &"x".repeat(MAX_NAME_CHARS + 1)] {
            let u = User { id: UserId(Uuid::from_u128(10)), oidc_subject: None, display_name: bad.into() };
            assert!(matches!(create_user(&conn, &u, &now), Err(PersistenceError::Invalid { .. })), "{bad:?}");
        }
    }

    #[test]
    fn documents_have_one_owner_and_members_with_roles() {
        let (db, ana, bea) = setup();
        let mut conn = db.lock().unwrap();
        let record = document(&conn, doc_id(1)).unwrap().unwrap();
        assert_eq!((record.title.as_str(), record.owner, record.current_revision), ("Plan", ana.id, 0));
        assert_eq!(role_of(&conn, doc_id(1), ana.id).unwrap(), Some(Role::Owner));
        assert_eq!(role_of(&conn, doc_id(1), bea.id).unwrap(), None);

        set_member(&mut conn, doc_id(1), bea.id, Role::Viewer).unwrap();
        assert!(!role_of(&conn, doc_id(1), bea.id).unwrap().unwrap().can_edit());
        set_member(&mut conn, doc_id(1), bea.id, Role::Editor).unwrap();
        assert!(role_of(&conn, doc_id(1), bea.id).unwrap().unwrap().can_edit());
        assert_eq!(documents_for(&conn, bea.id).unwrap().into_iter().map(|(d, r)| (d.id, r)).collect::<Vec<_>>(), [(doc_id(1), Role::Editor)]);

        // Ownership doesn't move through set_member, the owner can't be removed, strangers can't join.
        assert!(set_member(&mut conn, doc_id(1), bea.id, Role::Owner).is_err());
        assert!(set_member(&mut conn, doc_id(1), ana.id, Role::Viewer).is_err());
        assert!(matches!(set_member(&mut conn, doc_id(1), UserId(Uuid::from_u128(77)), Role::Editor), Err(PersistenceError::NotFound("user"))));
        assert!(matches!(set_member(&mut conn, doc_id(9), bea.id, Role::Editor), Err(PersistenceError::NotFound("document"))));
        assert!(!remove_member(&conn, doc_id(1), ana.id).unwrap());
        assert!(remove_member(&conn, doc_id(1), bea.id).unwrap());
        assert_eq!(role_of(&conn, doc_id(1), bea.id).unwrap(), None);

        let now = Timestamp::now();
        assert!(matches!(create_document(&mut conn, doc_id(1), "Again", ana.id, &now), Err(PersistenceError::AlreadyExists(_))));
        assert!(matches!(create_document(&mut conn, doc_id(2), "Orphan", UserId(Uuid::from_u128(77)), &now), Err(PersistenceError::NotFound(_))));
        assert!(matches!(create_document(&mut conn, doc_id(3), "\n", ana.id, &now), Err(PersistenceError::Invalid { .. })));
        assert!(document(&conn, doc_id(2)).unwrap().is_none() && document(&conn, doc_id(3)).unwrap().is_none());
    }

    #[test]
    fn operations_commit_at_dense_increasing_revisions() {
        let (db, ana, _) = setup();
        let mut conn = db.lock().unwrap();
        let client = ClientId::new("desktop-a").unwrap();
        for n in 0..5u64 {
            let now = ts(&format!("2026-11-03T11:00:0{n}.000Z"));
            let c = commit_operation(
                &mut conn,
                doc_id(1),
                n,
                NewOperation { operation_id: op_id(n.into()), author: ana.id, client_id: &client, operation: &insert(&n.to_string()) },
                &now,
            )
            .unwrap();
            assert_eq!(c, Commit::New(n + 1));
        }
        let record = document(&conn, doc_id(1)).unwrap().unwrap();
        assert_eq!((record.current_revision, record.updated_at.as_str()), (5, "2026-11-03T11:00:04.000Z"));
        let since = operations_since(&conn, doc_id(1), 2, 100).unwrap();
        assert_eq!(since.iter().map(|o| o.revision).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(since[0].operation, insert("2"));
        assert_eq!(since[0].client_id.as_ref(), Some(&client));
        assert_eq!(operations_since(&conn, doc_id(1), 0, 2).unwrap().len(), 2);
        assert!(operations_since(&conn, doc_id(1), 5, 100).unwrap().is_empty());
    }

    #[test]
    fn a_resubmitted_operation_returns_its_original_commit() {
        let (db, ana, bea) = setup();
        let mut conn = db.lock().unwrap();
        let (a, b) = (ClientId::new("desktop-a").unwrap(), ClientId::new("desktop-b").unwrap());
        let now = Timestamp::now();
        assert_eq!(
            commit_operation(
                &mut conn,
                doc_id(1),
                0,
                NewOperation { operation_id: op_id(1), author: ana.id, client_id: &a, operation: &insert("x") },
                &now
            )
            .unwrap(),
            Commit::New(1)
        );
        assert_eq!(
            commit_operation(
                &mut conn,
                doc_id(1),
                1,
                NewOperation { operation_id: op_id(2), author: ana.id, client_id: &a, operation: &insert("y") },
                &now
            )
            .unwrap(),
            Commit::New(2)
        );
        // Resent after a reconnect, even with a stale base or different content: the original.
        let again = commit_operation(
            &mut conn,
            doc_id(1),
            0,
            NewOperation { operation_id: op_id(1), author: bea.id, client_id: &b, operation: &insert("other") },
            &now,
        )
        .unwrap();
        let Commit::Duplicate(original) = again else { panic!("not a duplicate: {again:?}") };
        assert_eq!((original.revision, original.author, original.operation.clone()), (1, ana.id, insert("x")));
        assert_eq!(original.client_id, Some(a));
        assert_eq!(document(&conn, doc_id(1)).unwrap().unwrap().current_revision, 2);
        assert_eq!(operations_since(&conn, doc_id(1), 0, 100).unwrap().len(), 2);
    }

    #[test]
    fn a_wrong_expected_revision_writes_nothing() {
        let (db, ana, _) = setup();
        let mut conn = db.lock().unwrap();
        let client = ClientId::new("desktop-a").unwrap();
        let now = Timestamp::now();
        commit_operation(
            &mut conn,
            doc_id(1),
            0,
            NewOperation { operation_id: op_id(1), author: ana.id, client_id: &client, operation: &insert("x") },
            &now,
        )
        .unwrap();
        for stale in [0, 2, u64::MAX] {
            let r = commit_operation(
                &mut conn,
                doc_id(1),
                stale,
                NewOperation { operation_id: op_id(2), author: ana.id, client_id: &client, operation: &insert("y") },
                &now,
            );
            assert!(matches!(r, Err(PersistenceError::RevisionMismatch { found: 1, .. })), "{stale}: {r:?}");
        }
        assert!(find_operation(&conn, doc_id(1), op_id(2)).unwrap().is_none());
        assert_eq!(document(&conn, doc_id(1)).unwrap().unwrap().current_revision, 1);
        let missing = commit_operation(
            &mut conn,
            doc_id(9),
            0,
            NewOperation { operation_id: op_id(3), author: ana.id, client_id: &client, operation: &insert("z") },
            &now,
        );
        assert!(matches!(missing, Err(PersistenceError::NotFound("document"))));
    }

    #[test]
    fn corrupt_rows_are_errors_not_panics() {
        let (db, ana, _) = setup();
        let mut conn = db.lock().unwrap();
        let client = ClientId::new("desktop-a").unwrap();
        commit_operation(
            &mut conn,
            doc_id(1),
            0,
            NewOperation { operation_id: op_id(1), author: ana.id, client_id: &client, operation: &insert("x") },
            &Timestamp::now(),
        )
        .unwrap();
        conn.execute("UPDATE document_operations SET payload = x'7b7d'", []).unwrap();
        assert!(matches!(operations_since(&conn, doc_id(1), 0, 10), Err(PersistenceError::Corrupt(_))));
        // The schema's CHECK refuses this; a damaged file need not have kept it.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE document_members SET role = 'admin';").unwrap();
        assert!(matches!(role_of(&conn, doc_id(1), ana.id), Err(PersistenceError::Corrupt(_))));
        conn.execute("UPDATE documents SET current_revision = -1", []).unwrap();
        assert!(matches!(document(&conn, doc_id(1)), Err(PersistenceError::Corrupt(_))));
    }

    #[test]
    fn the_log_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("wordcraft-collab-repo-{}", Uuid::from_u128(std::process::id().into())));
        let path = dir.join("collab.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let client = ClientId::new("desktop-a").unwrap();
        {
            let db = Database::open(&path).unwrap();
            let mut conn = db.lock().unwrap();
            create_user(&conn, &user_n(1, "Ana"), &Timestamp::now()).unwrap();
            create_document(&mut conn, doc_id(1), "Plan", UserId(Uuid::from_u128(1)), &Timestamp::now()).unwrap();
            commit_operation(
                &mut conn,
                doc_id(1),
                0,
                NewOperation { operation_id: op_id(1), author: UserId(Uuid::from_u128(1)), client_id: &client, operation: &insert("kept") },
                &Timestamp::now(),
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        let conn = db.lock().unwrap();
        assert_eq!(document(&conn, doc_id(1)).unwrap().unwrap().current_revision, 1);
        assert_eq!(operations_since(&conn, doc_id(1), 0, 10).unwrap()[0].operation, insert("kept"));
        drop(conn);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let t = Timestamp::from(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_762_164_000_123));
        assert_eq!(t.as_str(), "2025-11-03T10:00:00.123Z");
    }
}
