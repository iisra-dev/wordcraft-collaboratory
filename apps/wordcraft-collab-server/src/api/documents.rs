//! Document endpoints (design §8 "API HTTP", milestone C2.5).
//!
//! Every endpoint needs a signed-in user ([`AuthUser`]) and checks their role *in that document*:
//! members read, the owner manages members. A document that does not exist and one the user is
//! not a member of get the same 404, so ids cannot be probed. Blocking work (SQLite, DOCX) runs
//! in `spawn_blocking`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wordcraft_collab_model::BlockId;
use wordcraft_collab_protocol::{DocumentId, DocumentOperation, RejectReason};
use wordcraft_doc::Document;

use crate::AppState;
use crate::documents::{self, Rejection};
use crate::persistence::{DocumentRecord, PersistenceError, Role, UserId, repository};

/// Largest DOCX accepted for import, in bytes.
pub const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;

/// Most history rows one request returns.
pub const MAX_HISTORY_PAGE: usize = 500;

const DOCX_TYPE: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

/// An error answer: a status and `{"error": "…"}`.
#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into())
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "no such document, or not a member")
    }

    pub(crate) fn internal(e: impl std::fmt::Display) -> Self {
        tracing::error!("API: {e}");
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "the server could not complete the request")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<PersistenceError> for ApiError {
    fn from(e: PersistenceError) -> Self {
        match e {
            PersistenceError::Invalid { .. } => Self::new(StatusCode::BAD_REQUEST, e.to_string()),
            PersistenceError::AlreadyExists(_) => Self::new(StatusCode::CONFLICT, e.to_string()),
            PersistenceError::NotFound(_) => Self::new(StatusCode::NOT_FOUND, e.to_string()),
            _ => Self::internal(e),
        }
    }
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        match r.reason {
            RejectReason::Forbidden => Self::not_found(),
            RejectReason::RateLimited => Self::new(StatusCode::TOO_MANY_REQUESTS, r.message),
            RejectReason::InvalidOperation | RejectReason::UnsupportedOperation | RejectReason::TooLarge => {
                Self::new(StatusCode::BAD_REQUEST, r.message)
            }
            _ => Self::new(StatusCode::SERVICE_UNAVAILABLE, r.message),
        }
    }
}

/// Run blocking work off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::internal)?
}

/// The signed-in user (`Authorization: Bearer <token>`), or 401.
pub struct AuthUser(pub UserId);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let authorization = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let (db, oidc) = (state.db.clone(), state.oidc.clone());
        let user = blocking(move || Ok(crate::auth::authenticate(&db, oidc.as_deref(), authorization.as_deref())?)).await?;
        user.map(AuthUser).ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "sign in first"))
    }
}

pub(crate) fn parse_document(id: &str) -> Result<DocumentId, ApiError> {
    Uuid::parse_str(id).map(DocumentId).map_err(|_| ApiError::not_found())
}

/// The document and the user's role in it, or the uniform 404.
async fn membership(state: &AppState, user: UserId, id: DocumentId) -> Result<(DocumentRecord, Role), ApiError> {
    let db = state.db.clone();
    blocking(move || {
        let conn = db.lock()?;
        let role = repository::role_of(&conn, id, user)?.ok_or_else(ApiError::not_found)?;
        let record = repository::document(&conn, id)?.ok_or_else(ApiError::not_found)?;
        Ok((record, role))
    })
    .await
}

#[derive(Serialize)]
pub struct DocumentInfo {
    pub id: Uuid,
    pub title: String,
    pub owner: Uuid,
    pub role: &'static str,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Owner => "owner",
        Role::Editor => "editor",
        Role::Viewer => "viewer",
    }
}

impl DocumentInfo {
    fn new(d: DocumentRecord, role: Role) -> Self {
        Self {
            id: d.id.0,
            title: d.title,
            owner: d.owner.0,
            role: role_name(role),
            revision: d.current_revision,
            created_at: d.created_at.as_str().to_owned(),
            updated_at: d.updated_at.as_str().to_owned(),
        }
    }
}

/// `GET /api/v1/documents`: the user's documents, most recently changed first.
pub async fn list(State(state): State<AppState>, AuthUser(user): AuthUser) -> Result<Json<Vec<DocumentInfo>>, ApiError> {
    let db = state.db.clone();
    let docs = blocking(move || Ok(repository::documents_for(&*db.lock()?, user)?)).await?;
    Ok(Json(docs.into_iter().map(|(d, r)| DocumentInfo::new(d, r)).collect()))
}

#[derive(Deserialize)]
pub struct CreateQuery {
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    title: String,
}

/// `POST /api/v1/documents`: a blank document (`application/json`, `{"title": …}`) or an
/// imported one (a DOCX body, title in `?title=`). Answers 201 with the new document.
pub async fn create(
    State(state): State<AppState>,
    super::links::AccountUser(user): super::links::AccountUser,
    Query(query): Query<CreateQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<DocumentInfo>), ApiError> {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let essence = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let (title, docx) = match essence.as_str() {
        "application/json" => {
            let CreateBody { title } = serde_json::from_slice(&body).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
            (title, None)
        }
        DOCX_TYPE => {
            if body.len() > MAX_IMPORT_BYTES {
                return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "document too large"));
            }
            (query.title.unwrap_or_else(|| "Imported document".into()), Some(body))
        }
        _ => return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("send application/json or {DOCX_TYPE}"))),
    };
    let (db, storage) = (state.db.clone(), state.storage.clone());
    let record = blocking(move || {
        let document = match docx {
            None => Document::default(),
            // Hostile input: the reader is guarded (zip bombs, depth, sizes) and errors, never panics.
            Some(bytes) => wordcraft_docx::read(&bytes).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("not a readable DOCX: {e}")))?,
        };
        let id = DocumentId(crate::auth::new_uuid().map_err(ApiError::internal)?);
        // A failed random source yields the nil id, which the registry refuses as a duplicate
        // on the second block: creation fails cleanly instead of panicking.
        let new_block = || BlockId(crate::auth::new_uuid().unwrap_or(Uuid::nil()));
        Ok(documents::create(&db, &storage, id, &title, user, document, new_block)?)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(DocumentInfo::new(record, Role::Owner))))
}

/// `GET /api/v1/documents/{id}`.
pub async fn metadata(State(state): State<AppState>, AuthUser(user): AuthUser, Path(id): Path<String>) -> Result<Json<DocumentInfo>, ApiError> {
    let (record, role) = membership(&state, user, parse_document(&id)?).await?;
    Ok(Json(DocumentInfo::new(record, role)))
}

/// `GET /api/v1/documents/{id}/export`: the current shared state as DOCX.
pub async fn export(State(state): State<AppState>, AuthUser(user): AuthUser, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_document(&id)?;
    let (record, _) = membership(&state, user, id).await?;
    let (_, shared) = state.documents.get(id).await?.read(user).await?;
    let db = state.db.clone();
    let bytes = blocking(move || {
        let bytes = wordcraft_docx::write(shared.document()).map_err(ApiError::internal)?;
        repository::audit(
            &*db.lock()?,
            Some(user),
            repository::AuditAction::DocumentExported,
            Some(id),
            None,
            "docx",
            &crate::persistence::Timestamp::now(),
        )?;
        Ok(bytes)
    })
    .await?;
    let disposition = format!("attachment; filename=\"{}.docx\"", safe_file_name(&record.title));
    Ok(([(header::CONTENT_TYPE, DOCX_TYPE.to_owned()), (header::CONTENT_DISPOSITION, disposition)], bytes).into_response())
}

/// A title as a file name: letters, digits, spaces, `-` and `_` (others become `_`), at most
/// 80 characters, never empty.
fn safe_file_name(title: &str) -> String {
    let name: String = title.chars().take(80).map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') { c } else { '_' }).collect();
    let name = name.trim();
    if name.is_empty() { "document".into() } else { name.into() }
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

/// One commit in the history: who, when in the sequence, what kind — not the text (§12, audit
/// without unnecessary content).
#[derive(Serialize)]
pub struct HistoryEntry {
    pub revision: u64,
    pub operation_id: Uuid,
    pub author: Uuid,
    pub client_id: Option<String>,
    pub kind: &'static str,
}

/// `GET /api/v1/documents/{id}/history?after=&limit=`.
pub async fn history(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Vec<HistoryEntry>>, ApiError> {
    let id = parse_document(&id)?;
    membership(&state, user, id).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, MAX_HISTORY_PAGE);
    let db = state.db.clone();
    let rows = blocking(move || {
        let conn = db.lock()?;
        repository::role_of(&conn, id, user)?.ok_or_else(ApiError::not_found)?;
        Ok(repository::operations_since(&conn, id, q.after.unwrap_or(0), limit)?)
    })
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|op| HistoryEntry {
                revision: op.revision,
                operation_id: op.operation_id.0,
                author: op.author.0,
                client_id: op.client_id.map(|c| c.as_str().to_owned()),
                kind: match op.operation {
                    DocumentOperation::InsertText { .. } => "insert_text",
                    DocumentOperation::DeleteText { .. } => "delete_text",
                    DocumentOperation::SplitParagraph { .. } => "split_paragraph",
                    DocumentOperation::MergeParagraphs { .. } => "merge_paragraphs",
                    DocumentOperation::FormatText { .. } => "format_text",
                    DocumentOperation::Batch { .. } => "batch",
                },
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberBody {
    role: String,
}

/// The owner, or the uniform 404 for non-members and 403 for other members.
async fn owner_of(state: &AppState, user: UserId, id: DocumentId) -> Result<(), ApiError> {
    let (_, role) = membership(state, user, id).await?;
    if role.can_manage() { Ok(()) } else { Err(ApiError::new(StatusCode::FORBIDDEN, "only the owner manages members")) }
}

fn parse_user(id: &str) -> Result<UserId, ApiError> {
    Uuid::parse_str(id).map(UserId).map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "no such user"))
}

/// `PUT /api/v1/documents/{id}/members/{user_id}` with `{"role": "editor" | "viewer"}`.
pub async fn put_member(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, member)): Path<(String, String)>,
    Json(body): Json<MemberBody>,
) -> Result<StatusCode, ApiError> {
    let (id, member) = (parse_document(&id)?, parse_user(&member)?);
    owner_of(&state, user, id).await?;
    let role = match body.role.as_str() {
        "editor" => Role::Editor,
        "viewer" => Role::Viewer,
        _ => return Err(ApiError::new(StatusCode::BAD_REQUEST, "role must be \"editor\" or \"viewer\"")),
    };
    state.documents.get(id).await?.change_member(user, member, Some(role)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/documents/{id}/members/{user_id}` (not the owner).
pub async fn delete_member(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, member)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (id, member) = (parse_document(&id)?, parse_user(&member)?);
    owner_of(&state, user, id).await?;
    let removed = state.documents.get(id).await?.change_member(user, member, None).await?;
    if removed { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::new(StatusCode::NOT_FOUND, "not a member (or the owner)")) }
}

#[derive(Deserialize)]
pub struct AuditQuery {
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub at: String,
    pub actor: Option<Uuid>,
    pub action: String,
    pub subject: Option<Uuid>,
    pub detail: String,
}

/// `GET /api/v1/documents/{id}/audit?after=&limit=`: the document's audit trail (owner only).
pub async fn audit(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditRow>>, ApiError> {
    let id = parse_document(&id)?;
    owner_of(&state, user, id).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, MAX_HISTORY_PAGE);
    let db = state.db.clone();
    let rows = blocking(move || Ok(repository::audit_for(&*db.lock()?, id, q.after.unwrap_or(0), limit)?)).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(row, e)| AuditRow {
                id: row,
                at: e.at,
                actor: e.actor.map(|u| u.0),
                action: e.action,
                subject: e.subject.map(|u| u.0),
                detail: e.detail,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_file_name("Plan 2026"), "Plan 2026");
        assert_eq!(safe_file_name("../../etc/passwd"), "______etc_passwd");
        assert_eq!(safe_file_name("a\"b\r\nc"), "a_b__c");
        assert_eq!(safe_file_name("¿Qué tal? ñandú"), "_Qué tal_ ñandú");
        assert_eq!(safe_file_name("   "), "document");
        assert_eq!(safe_file_name(&"x".repeat(500)).len(), 80);
    }
}

// --- people (C5.5: inviting from the editor) -------------------------------------------------

/// A person as the participants screen shows them: no OIDC subject, no tokens.
#[derive(Serialize)]
pub struct Person {
    pub id: Uuid,
    pub name: String,
}

impl Person {
    fn new(user: crate::persistence::User) -> Self {
        Self { id: user.id.0, name: user.display_name }
    }
}

#[derive(Serialize)]
pub struct MemberInfo {
    pub id: Uuid,
    pub name: String,
    pub role: &'static str,
}

/// `GET /api/v1/me`: who the token belongs to, so a person can tell the owner who to invite.
pub async fn me(State(state): State<AppState>, AuthUser(user): AuthUser) -> Result<Json<Person>, ApiError> {
    let db = state.db.clone();
    let found = blocking(move || Ok(repository::user(&*db.lock()?, user)?)).await?;
    found.map(|u| Json(Person::new(u))).ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "sign in first"))
}

/// `GET /api/v1/documents/{id}/members`: who has access and how; for every member (people see
/// who they share a document with), the owner first.
pub async fn members(State(state): State<AppState>, AuthUser(user): AuthUser, Path(id): Path<String>) -> Result<Json<Vec<MemberInfo>>, ApiError> {
    let id = parse_document(&id)?;
    membership(&state, user, id).await?;
    let db = state.db.clone();
    let list = blocking(move || Ok(repository::members(&*db.lock()?, id)?)).await?;
    Ok(Json(list.into_iter().map(|(u, role)| MemberInfo { id: u.id.0, name: u.display_name, role: role_name(role) }).collect()))
}

#[derive(Deserialize)]
pub struct UserQuery {
    q: Option<String>,
}

/// Shortest name fragment a search accepts (a UUID is always accepted).
pub const MIN_USER_QUERY_CHARS: usize = 2;

/// `GET /api/v1/users?q=`: people on this server whose name contains `q`, or whose id is `q`,
/// for the owner to pick when inviting. Signed-in users only; names and ids only, at most
/// [`repository::MAX_USER_MATCHES`]. A person who signs in with an identity provider appears
/// after their first sign-in.
pub async fn users(
    State(state): State<AppState>,
    super::links::AccountUser(_): super::links::AccountUser,
    Query(query): Query<UserQuery>,
) -> Result<Json<Vec<Person>>, ApiError> {
    let q = query.q.unwrap_or_default().trim().to_owned();
    let is_id = Uuid::parse_str(&q).is_ok();
    let chars = q.chars().count();
    if !is_id && !(MIN_USER_QUERY_CHARS..=100).contains(&chars) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, format!("search with {MIN_USER_QUERY_CHARS} to 100 characters, or a user id")));
    }
    let db = state.db.clone();
    let found = blocking(move || Ok(repository::find_users(&*db.lock()?, &q, repository::MAX_USER_MATCHES)?)).await?;
    Ok(Json(found.into_iter().map(Person::new).collect()))
}
