//! C6.5: invitations, redeemed transactionally by the document actor. No raw tokens in SQLite.

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wordcraft_collab_protocol::DocumentId;

use super::{PersistenceError, Role, Timestamp, User, UserId, repository};
use crate::{auth, storage::sha256_hex};

pub const MAX_LINKS: i64 = 100;
pub const MAX_GUESTS: u32 = 100;
pub const MAX_LIFETIME_SECONDS: u32 = 30 * 24 * 60 * 60;
const TOKEN_PREFIX: &str = "wcs_";

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error(transparent)]
    Random(#[from] auth::AuthError),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("no such invitation, or it is no longer available")]
    NotFound,
    #[error("only the owner manages invitation links")]
    NotOwner,
    #[error("sign in to use this invitation")]
    SignIn,
    #[error("this invitation has no guest places left; ask the owner for access")]
    GuestLimit,
    #[error("this document already has the maximum number of invitation links")]
    LinkLimit,
    #[error("guest sessions are limited to their invited document")]
    GuestScope,
    #[error("the server could not complete the request")]
    Unavailable,
}

impl From<rusqlite::Error> for LinkError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Persistence(e.into())
    }
}

type Result<T> = std::result::Result<T, LinkError>;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkRole {
    Editor,
    Viewer,
}

impl LinkRole {
    fn label(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }
    fn role(self) -> Role {
        match self {
            Self::Editor => Role::Editor,
            Self::Viewer => Role::Viewer,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateLink {
    pub role: LinkRole,
    #[serde(default = "default_lifetime")]
    pub expires_in_seconds: u32,
    #[serde(default)]
    pub allow_guests: bool,
    pub max_guests: Option<u32>,
}

fn default_lifetime() -> u32 {
    7 * 24 * 60 * 60
}

#[derive(Serialize)]
pub struct LinkInfo {
    pub id: Uuid,
    pub document_id: Uuid,
    pub role: LinkRole,
    pub created_at: String,
    pub expires_at: String,
    pub allow_guests: bool,
    pub max_guests: u32,
    pub guests_used: u32,
    pub revoked: bool,
}

#[derive(Serialize)]
pub struct CreatedLink {
    #[serde(flatten)]
    pub info: LinkInfo,
    /// Shown once, never returned by listing or kept in the database.
    pub token: String,
    pub path: String,
}

pub enum Applicant {
    Account(UserId),
    Guest { name: String },
}

pub struct JoinedLink {
    pub user: UserId,
    pub role: Role,
    pub token: Option<String>,
}

pub fn is_guest(conn: &Connection, user: UserId) -> std::result::Result<bool, PersistenceError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM guest_accounts WHERE user_id = ?1)", [user.to_string()], |r| r.get(0))?)
}

pub(crate) fn allows_document(conn: &Connection, user: UserId, document: DocumentId) -> std::result::Result<bool, PersistenceError> {
    Ok(conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM guest_accounts WHERE user_id = ?1 AND document_id <> ?2)",
        params![user.to_string(), document.0.to_string()],
        |r| r.get(0),
    )?)
}

fn owner(conn: &Connection, document: DocumentId, user: UserId) -> Result<()> {
    match repository::role_of(conn, document, user)? {
        Some(Role::Owner) => Ok(()),
        Some(_) => Err(LinkError::NotOwner),
        None => Err(LinkError::NotFound),
    }
}

pub fn create(conn: &mut Connection, document: DocumentId, user: UserId, options: CreateLink, now: &Timestamp) -> Result<CreatedLink> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    owner(&tx, document, user)?;
    if !(1..=MAX_LIFETIME_SECONDS).contains(&options.expires_in_seconds) {
        return Err(LinkError::Invalid("invitation lifetime must be between one second and 30 days"));
    }
    let max_guests = options.max_guests.unwrap_or(if options.allow_guests { 10 } else { 0 });
    if (options.allow_guests && !(1..=MAX_GUESTS).contains(&max_guests)) || (!options.allow_guests && max_guests != 0) {
        return Err(LinkError::Invalid("guest invitations need 1 to 100 places; account-only invitations need zero"));
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM share_links WHERE document_id = ?1", [document.0.to_string()], |r| r.get(0))?;
    if count >= MAX_LINKS {
        return Err(LinkError::LinkLimit);
    }
    let created = chrono::DateTime::parse_from_rfc3339(now.as_str()).map_err(|_| LinkError::Invalid("invalid invitation time"))?;
    let expires = created
        .checked_add_signed(chrono::Duration::seconds(i64::from(options.expires_in_seconds)))
        .ok_or(LinkError::Invalid("invitation expiry is out of range"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let token = format!("{TOKEN_PREFIX}{}", auth::generate_token()?.strip_prefix(auth::TOKEN_PREFIX).unwrap_or_default());
    let id = auth::new_uuid()?;
    tx.execute("INSERT INTO share_links (id, document_id, token_sha256, role, created_at, expires_at, allow_guests, max_guests) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id.to_string(), document.0.to_string(), sha256_hex(token.as_bytes()), options.role.label(), now.as_str(), expires, options.allow_guests, max_guests])?;
    audit(&tx, user, document, "share_link_created", options.role.label(), now)?;
    tx.commit()?;
    Ok(CreatedLink {
        path: format!("/s/{token}"),
        token,
        info: LinkInfo {
            id,
            document_id: document.0,
            role: options.role,
            created_at: now.as_str().into(),
            expires_at: expires,
            allow_guests: options.allow_guests,
            max_guests,
            guests_used: 0,
            revoked: false,
        },
    })
}

fn info(row: &Row<'_>) -> rusqlite::Result<std::result::Result<LinkInfo, PersistenceError>> {
    let id: String = row.get(0)?;
    let document: String = row.get(1)?;
    let role: String = row.get(2)?;
    let parse = || -> std::result::Result<LinkInfo, PersistenceError> {
        let id = Uuid::parse_str(&id).map_err(|_| PersistenceError::Corrupt("invalid invitation id".into()))?;
        let document_id = Uuid::parse_str(&document).map_err(|_| PersistenceError::Corrupt("invalid invitation document".into()))?;
        let role = match role.as_str() {
            "editor" => LinkRole::Editor,
            "viewer" => LinkRole::Viewer,
            _ => return Err(PersistenceError::Corrupt("invalid invitation role".into())),
        };
        Ok(LinkInfo {
            id,
            document_id,
            role,
            created_at: row.get(3)?,
            expires_at: row.get(4)?,
            allow_guests: row.get(5)?,
            max_guests: row.get(6)?,
            guests_used: row.get(7)?,
            revoked: row.get(8)?,
        })
    };
    Ok(parse())
}

const COLUMNS: &str = "id, document_id, role, created_at, expires_at, allow_guests, max_guests, guests_used, revoked_at IS NOT NULL";

pub fn list(conn: &Connection, document: DocumentId, user: UserId) -> Result<Vec<LinkInfo>> {
    owner(conn, document, user)?;
    let mut stmt =
        conn.prepare(&format!("SELECT {COLUMNS} FROM share_links WHERE document_id = ?1 ORDER BY created_at DESC, id LIMIT {MAX_LINKS}"))?;
    let rows = stmt.query_map([document.0.to_string()], info)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row??);
    }
    Ok(result)
}

pub fn revoke(conn: &mut Connection, document: DocumentId, user: UserId, link: Uuid, now: &Timestamp) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    owner(&tx, document, user)?;
    let found: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM share_links WHERE id = ?1 AND document_id = ?2)",
        params![link.to_string(), document.0.to_string()],
        |r| r.get(0),
    )?;
    if !found {
        return Err(LinkError::NotFound);
    }
    let changed =
        tx.execute("UPDATE share_links SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![now.as_str(), link.to_string()])?;
    if changed != 0 {
        audit(&tx, user, document, "share_link_revoked", "", now)?;
    }
    tx.commit()?;
    Ok(())
}

/// A token's shape is checked before hashing; invalid tokens never appear in error messages.
pub fn token_hash(token: &str) -> Result<String> {
    if !token.strip_prefix(TOKEN_PREFIX).is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(LinkError::NotFound);
    }
    Ok(sha256_hex(token.as_bytes()))
}

fn active(conn: &Connection, hash: &str, now: &Timestamp) -> Result<LinkInfo> {
    let found = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM share_links WHERE token_sha256 = ?1 AND revoked_at IS NULL AND expires_at > ?2"),
            params![hash, now.as_str()],
            info,
        )
        .optional()?;
    found.ok_or(LinkError::NotFound)?.map_err(LinkError::from)
}

/// Resolve for routing only. Redeeming rechecks validity in its write transaction.
pub fn document_for(conn: &Connection, hash: &str, now: &Timestamp) -> Result<DocumentId> {
    Ok(DocumentId(active(conn, hash, now)?.document_id))
}

/// Called only in the actor turn that also notifies existing subscribers of a promotion.
pub fn redeem(conn: &mut Connection, document: DocumentId, hash: &str, applicant: Applicant, now: &Timestamp) -> Result<JoinedLink> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let link = active(&tx, hash, now)?;
    if link.document_id != document.0 {
        return Err(LinkError::NotFound);
    }
    let (user, token) = match applicant {
        Applicant::Account(user) => {
            check_guest_scope(&tx, user, document)?;
            (user, None)
        }
        Applicant::Guest { name } => new_guest(&tx, &link, &name, now)?,
    };
    let existing = repository::role_of(&tx, document, user)?;
    let role = match existing {
        Some(Role::Owner) => Role::Owner,
        Some(Role::Editor) => Role::Editor,
        _ => link.role.role(),
    };
    let label = match role {
        Role::Owner => "owner",
        Role::Editor => "editor",
        Role::Viewer => "viewer",
    };
    tx.execute("INSERT INTO document_members (document_id, user_id, role) VALUES (?1, ?2, ?3) ON CONFLICT(document_id, user_id) DO UPDATE SET role = excluded.role",
        params![document.0.to_string(), user.to_string(), label])?;
    if existing != Some(role) {
        audit(&tx, user, document, "share_link_joined", label, now)?;
    }
    tx.commit()?;
    Ok(JoinedLink { user, role, token })
}

fn check_guest_scope(conn: &Connection, user: UserId, document: DocumentId) -> Result<()> {
    if !allows_document(conn, user, document)? {
        return Err(LinkError::GuestScope);
    }
    Ok(())
}

fn new_guest(conn: &Connection, link: &LinkInfo, name: &str, now: &Timestamp) -> Result<(UserId, Option<String>)> {
    if !link.allow_guests {
        return Err(LinkError::SignIn);
    }
    if link.guests_used >= link.max_guests {
        return Err(LinkError::GuestLimit);
    }
    let user = UserId(auth::new_uuid()?);
    let token = auth::generate_token()?;
    repository::create_user(conn, &User { id: user, oidc_subject: None, display_name: name.into() }, now)?;
    repository::add_token(conn, user, &sha256_hex(token.as_bytes()), now)?;
    conn.execute(
        "INSERT INTO guest_accounts (user_id, document_id, link_id) VALUES (?1, ?2, ?3)",
        params![user.to_string(), link.document_id.to_string(), link.id.to_string()],
    )?;
    conn.execute("UPDATE share_links SET guests_used = guests_used + 1 WHERE id = ?1", [link.id.to_string()])?;
    Ok((user, Some(token)))
}

/// Fixed action/detail strings only; never a token, display name or document contents.
fn audit(conn: &Connection, user: UserId, document: DocumentId, action: &str, detail: &str, now: &Timestamp) -> Result<()> {
    conn.execute(
        "INSERT INTO audit_log (at, actor_id, action, document_id, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now.as_str(), user.to_string(), action, document.0.to_string(), detail],
    )?;
    Ok(())
}
