//! WordCraft Collaborative server (docs/collab/design.md §8–§12).
//!
//! The server is authoritative: no shared state changes without passing validation and
//! persistence. One [`documents`] actor per open document serialises its operations; documents
//! run concurrently. Operations are persisted *before* they are acknowledged.
//!
//! Module map (design §4): [`config`] (environment), [`api`] (HTTP), [`ws`] (WebSocket),
//! [`auth`] (OIDC), [`documents`] (actors), [`persistence`] (SQLite), [`storage`] (snapshot
//! files), [`backup`] (encrypted backups and restore), [`healthcheck`] (the container probe).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod api;
pub mod auth;
pub mod backup;
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
    #[error(transparent)]
    Backup(#[from] backup::BackupError),
    #[error("{0}")]
    Usage(String),
}

/// What every request handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<persistence::Database>,
    pub storage: Arc<storage::DocumentStorage>,
    /// The open documents' actors.
    pub documents: Arc<documents::Registry>,
    /// OIDC access-token validation, when `OIDC_ISSUER`/`OIDC_AUDIENCE` are set (C5.1).
    pub oidc: Option<Arc<auth::oidc::Verifier>>,
    /// Open WebSocket connections per user (C5.4).
    pub connections: Arc<ws::Connections>,
}

impl AppState {
    /// Open the database (running migrations) and the snapshot directory.
    pub fn open(config: &Config) -> Result<Self, ServerError> {
        let db = Arc::new(persistence::Database::open(&config.database_path)?);
        let storage = Arc::new(storage::DocumentStorage::open(&config.document_storage)?);
        let documents = Arc::new(documents::Registry::new(db.clone(), storage.clone()));
        let oidc = config.oidc.as_ref().map(|o| Arc::new(auth::oidc::Verifier::new(&o.issuer, &o.audience)));
        Ok(Self { db, storage, documents, oidc, connections: Arc::default() })
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
