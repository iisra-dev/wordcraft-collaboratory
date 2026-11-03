//! SQLite persistence (design §9): WAL mode, one server process with write access.
//!
//! Every committed operation is a row in `document_operations`, written *before* the commit is
//! acknowledged; `UNIQUE (document_id, operation_id)` makes resubmissions idempotent. Recovery is
//! the latest valid snapshot plus the operations after it.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

pub mod repository;

pub use repository::{Commit, DocumentRecord, MAX_TITLE_CHARS, NewOperation, Role, SnapshotRecord, StoredOperation, Timestamp, User, UserId};

/// Schema migrations, applied in order; `PRAGMA user_version` records how many have run.
/// Never edit a released migration: add a new file.
const MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/0001_init.sql"),
    include_str!("../../migrations/0002_operation_client.sql"),
    include_str!("../../migrations/0003_user_tokens.sql"),
    include_str!("../../migrations/0004_audit_log.sql"),
];

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("database schema version {found} is newer than this server ({known})")]
    SchemaTooNew { found: i64, known: usize },
    #[error("database connection lock poisoned")]
    Poisoned,
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{0} already exists")]
    AlreadyExists(&'static str),
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: &'static str },
    #[error("document is at revision {found}, expected {expected}")]
    RevisionMismatch { expected: u64, found: u64 },
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (creating it and its directory if needed) and migrate.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    /// A private in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, PersistenceError> {
        // journal_mode answers with the mode it set ("memory" for in-memory databases).
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL; PRAGMA busy_timeout = 5000;")?;
        migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// The connection. Blocking: call from `spawn_blocking` (or the document actor's thread).
    pub fn lock(&self) -> Result<MutexGuard<'_, Connection>, PersistenceError> {
        self.conn.lock().map_err(|_| PersistenceError::Poisoned)
    }

    /// The database answers a trivial query.
    pub fn ping(&self) -> Result<(), PersistenceError> {
        self.lock()?.query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }

    /// Migrations applied so far.
    pub fn schema_version(&self) -> Result<i64, PersistenceError> {
        Ok(self.lock()?.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }
}

fn migrate(conn: &mut Connection) -> Result<(), PersistenceError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let done = usize::try_from(found).unwrap_or(usize::MAX);
    if done > MIGRATIONS.len() {
        return Err(PersistenceError::SchemaTooNew { found, known: MIGRATIONS.len() });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(done) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i64::try_from(index.saturating_add(1)).unwrap_or(i64::MAX))?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_the_schema_once() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        db.ping().unwrap();
        let mut conn = db.lock().unwrap();
        migrate(&mut conn).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            tables,
            ["audit_log", "document_members", "document_operations", "document_snapshots", "documents", "sqlite_sequence", "user_tokens", "users"]
        );
    }

    #[test]
    fn duplicate_operation_ids_are_refused() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO users VALUES ('u1', NULL, 'Ana', '2026-11-03T00:00:00Z');
             INSERT INTO documents (id, title, owner_id, created_at, updated_at) VALUES ('d1', 'Plan', 'u1', '2026-11-03T00:00:00Z', '2026-11-03T00:00:00Z');
             INSERT INTO document_operations (document_id, revision, operation_id, author_id, payload, created_at) VALUES ('d1', 1, 'op1', 'u1', x'00', '2026-11-03T00:00:00Z');",
        )
        .unwrap();
        let again = conn.execute("INSERT INTO document_operations (document_id, revision, operation_id, author_id, payload, created_at) VALUES ('d1', 2, 'op1', 'u1', x'00', '2026-11-03T00:00:01Z')", []);
        assert!(again.is_err());
    }

    #[test]
    fn a_newer_schema_is_not_touched() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(migrate(&mut conn), Err(PersistenceError::SchemaTooNew { found: 99, .. })));
    }
}
