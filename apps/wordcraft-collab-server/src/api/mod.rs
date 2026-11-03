//! HTTP API (design §8 "API HTTP").
//!
//! Implemented: health and the WebSocket. To come (milestones C2/C5), each checking permissions *per document*,
//! not just that the user is signed in:
//!
//! | Method | Endpoint | Job |
//! |---|---|---|
//! | `GET`  | `/api/v1/documents` | list accessible documents |
//! | `POST` | `/api/v1/documents` | create or import |
//! | `GET`  | `/api/v1/documents/{id}` | metadata |
//! | `GET`  | `/api/v1/documents/{id}/export` | download DOCX |
//! | `POST` | `/api/v1/documents/{id}/snapshots` | create a version |
//! | `GET`  | `/api/v1/documents/{id}/history` | history |
//! | `PUT`  | `/api/v1/documents/{id}/members/{user_id}` | change permissions |
//! | `GET`  | `/api/v1/ws` | WebSocket ([`crate::ws`]) |

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;

use crate::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health/live", get(live))
        .route("/api/v1/health/ready", get(ready))
        .route("/api/v1/ws", get(crate::ws::handler))
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
