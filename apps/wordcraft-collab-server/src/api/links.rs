//! C6.5: owner-managed invitation links. Guest creation and access are committed by the actor.

use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, State};
use axum::http::{StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::documents::{ApiError, AuthUser, blocking, parse_document};
use crate::AppState;
use crate::persistence::{
    Role, Timestamp, UserId,
    links::{self, Applicant, LinkError},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/documents/{id}/links", get(list).post(create))
        .route("/api/v1/documents/{id}/links/{link_id}", delete(revoke))
        .route("/api/v1/links/{token}/join", post(join))
        .route("/s/{token}", get(open))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

impl From<LinkError> for ApiError {
    fn from(e: LinkError) -> Self {
        let status = match e {
            LinkError::NotFound => StatusCode::NOT_FOUND,
            LinkError::NotOwner | LinkError::GuestScope => StatusCode::FORBIDDEN,
            LinkError::SignIn => StatusCode::UNAUTHORIZED,
            LinkError::GuestLimit | LinkError::LinkLimit => StatusCode::CONFLICT,
            LinkError::Invalid(_) => StatusCode::BAD_REQUEST,
            LinkError::Persistence(error) => return Self::from(error),
            LinkError::Random(_) | LinkError::Unavailable => return Self::internal(e),
        };
        Self::new(status, e.to_string())
    }
}

/// Full server account, not a document-scoped guest (creating documents, user directory).
pub struct AccountUser(pub UserId);

impl FromRequestParts<AppState> for AccountUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let AuthUser(user) = AuthUser::from_request_parts(parts, state).await?;
        let db = state.db.clone();
        if blocking(move || Ok(links::is_guest(&*db.lock()?, user)?)).await? {
            return Err(LinkError::GuestScope.into());
        }
        Ok(Self(user))
    }
}

/// Missing credentials may mean guest. Present but invalid credentials always mean 401.
pub struct OptionalUser(Option<UserId>);

impl FromRequestParts<AppState> for OptionalUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if parts.headers.contains_key(header::AUTHORIZATION) {
            Ok(Self(Some(AuthUser::from_request_parts(parts, state).await?.0)))
        } else {
            Ok(Self(None))
        }
    }
}

fn private_response(value: impl IntoResponse) -> Response {
    let mut response = value.into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response.headers_mut().insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"));
    response
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(options): Json<links::CreateLink>,
) -> Result<Response, ApiError> {
    let document = parse_document(&id)?;
    let db = state.db.clone();
    let created = blocking(move || Ok(links::create(&mut *db.lock()?, document, user, options, &Timestamp::now())?)).await?;
    Ok(private_response((StatusCode::CREATED, Json(created))))
}

pub async fn list(State(state): State<AppState>, AuthUser(user): AuthUser, Path(id): Path<String>) -> Result<Response, ApiError> {
    let document = parse_document(&id)?;
    let db = state.db.clone();
    let found = blocking(move || Ok(links::list(&*db.lock()?, document, user)?)).await?;
    Ok(private_response(Json(found)))
}

pub async fn revoke(State(state): State<AppState>, AuthUser(user): AuthUser, Path((id, link)): Path<(String, String)>) -> Result<Response, ApiError> {
    let document = parse_document(&id)?;
    let link = Uuid::parse_str(&link).map_err(|_| LinkError::NotFound)?;
    let db = state.db.clone();
    blocking(move || Ok(links::revoke(&mut *db.lock()?, document, user, link, &Timestamp::now())?)).await?;
    Ok(private_response(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinBody {
    name: Option<String>,
}

#[derive(Serialize)]
struct JoinInfo {
    document_id: Uuid,
    user_id: Uuid,
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

pub async fn join(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    Path(token): Path<String>,
    Json(body): Json<JoinBody>,
) -> Result<Response, ApiError> {
    let hash = links::token_hash(&token)?;
    let (db, lookup) = (state.db.clone(), hash.clone());
    let document = blocking(move || Ok(links::document_for(&*db.lock()?, &lookup, &Timestamp::now())?)).await?;
    let applicant = match user {
        Some(user) => Applicant::Account(user),
        None => Applicant::Guest { name: body.name.unwrap_or_default() },
    };
    let joined = state.documents.get(document).await?.accept_link(hash, applicant).await?;
    let role = match joined.role {
        Role::Owner => "owner",
        Role::Editor => "editor",
        Role::Viewer => "viewer",
    };
    Ok(private_response(Json(JoinInfo { document_id: document.0, user_id: joined.user.0, role, token: joined.token })))
}

pub async fn open(State(state): State<AppState>, Path(token): Path<String>) -> Result<Response, ApiError> {
    let hash = links::token_hash(&token)?;
    let db = state.db.clone();
    blocking(move || Ok(links::document_for(&*db.lock()?, &hash, &Timestamp::now())?)).await?;
    Ok(private_response(
        "WordCraft invitation\n\nTo open it, start WordCraft and choose Review > Share > Open Shared, then paste this link into Share link.\nThe owner may require an account on this server, or let you join as a guest with just your name.\nKeep the link private: anyone holding it may obtain the access it grants.\n",
    ))
}
