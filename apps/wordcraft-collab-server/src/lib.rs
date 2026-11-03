//! WordCraft Collaborative server (docs/collab/design.md §8–§12).
//!
//! The server is authoritative: no shared state changes without passing validation and
//! persistence. One [`documents`] actor per open document serialises its operations; documents
//! run concurrently. Operations are persisted *before* they are acknowledged.
//!
//! Module map (design §4): [`config`] (environment), [`api`] (HTTP), [`ws`] (WebSocket),
//! [`auth`] (OIDC), [`documents`] (actors), [`persistence`] (SQLite), [`storage`] (snapshot
//! files), [`healthcheck`] (the container probe).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod api;
pub mod auth;
pub mod config;
pub mod documents;
pub mod healthcheck;
pub mod persistence;
pub mod storage;
pub mod ws;

use std::sync::Arc;

pub use config::Config;

/// Startup and runtime failures.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Persistence(#[from] persistence::PersistenceError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
}

/// What every request handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<persistence::Database>,
    pub storage: Arc<storage::DocumentStorage>,
    /// The open documents' actors.
    pub documents: Arc<documents::Registry>,
}

impl AppState {
    /// Open the database (running migrations) and the snapshot directory.
    pub fn open(config: &Config) -> Result<Self, ServerError> {
        let db = Arc::new(persistence::Database::open(&config.database_path)?);
        let storage = Arc::new(storage::DocumentStorage::open(&config.document_storage)?);
        let documents = Arc::new(documents::Registry::new(db.clone(), storage.clone()));
        Ok(Self { db, storage, documents })
    }
}

/// Serve on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    axum::serve(listener, api::router(state)).with_graceful_shutdown(shutdown).await?;
    Ok(())
}
