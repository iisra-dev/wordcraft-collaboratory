//! HTTP API (design §8 "API HTTP").
//!
//! Implemented (C2.5): health, the WebSocket and the document endpoints ([`documents`]), each
//! checking permissions *per document*, not just that the user is signed in. Still to come:
//! `POST /api/v1/documents/{id}/snapshots` (named versions).
//!
//! | Method | Endpoint | Job |
//! |---|---|---|
//! | `GET`  | `/api/v1/documents` | list accessible documents |
//! | `POST` | `/api/v1/documents` | create or import |
//! | `GET`  | `/api/v1/documents/{id}` | metadata |
//! | `GET`  | `/api/v1/documents/{id}/export` | download DOCX |
//! | `POST` | `/api/v1/documents/{id}/snapshots` | create a version |
//! | `GET`  | `/api/v1/documents/{id}/history` | history |
//! | `GET`  | `/api/v1/documents/{id}/members` | who has access (any member) |
//! | `PUT`  | `/api/v1/documents/{id}/members/{user_id}` | change permissions |
//! | `GET`  | `/api/v1/users?q=` | find people to invite (signed in) |
//! | `GET`  | `/api/v1/me` | who the token belongs to |
//! | `GET`  | `/api/v1/ws` | WebSocket ([`crate::ws`]) |

pub mod documents;
pub mod links;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::{get, put};

use crate::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health/live", get(live))
        .route("/api/v1/health/ready", get(ready))
        .route("/api/v1/ws", get(crate::ws::handler))
        .route("/api/v1/documents", get(documents::list).post(documents::create).layer(DefaultBodyLimit::max(documents::MAX_IMPORT_BYTES)))
        .route("/api/v1/documents/{id}", get(documents::metadata))
        .route("/api/v1/documents/{id}/export", get(documents::export))
        .route("/api/v1/documents/{id}/history", get(documents::history))
        .route("/api/v1/documents/{id}/audit", get(documents::audit))
        .route("/api/v1/documents/{id}/members", get(documents::members))
        .route("/api/v1/documents/{id}/members/{user_id}", put(documents::put_member).delete(documents::delete_member))
        .route("/api/v1/users", get(documents::users))
        .route("/api/v1/me", get(documents::me))
        .merge(links::router())
        .with_state(state)
}

/// The process is up.
async fn live() -> &'static str {
    "ok"
}

/// The database answers and the snapshot directory is there.
async fn ready(State(state): State<AppState>) -> (StatusCode, &'static str) {
    let db = state.db.clone();
    let db_ok = matches!(tokio::task::spawn_blocking(move || db.ping()).await, Ok(Ok(())));
    if db_ok && state.storage.is_available() { (StatusCode::OK, "ready") } else { (StatusCode::SERVICE_UNAVAILABLE, "not ready") }
}
