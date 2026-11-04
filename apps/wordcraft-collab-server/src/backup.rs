//! Encrypted backups and their restoration (milestone C6.2, design §9 "Backup externo").
//!
//! A backup is one file, `wordcraft-<UTC time>.wcbackup.age`: a zip encrypted with
//! [age](https://age-encryption.org) to one or more X25519 public keys (`BACKUP_RECIPIENTS`).
//! The server only ever holds the public keys; whoever restores needs a matching private key,
//! kept elsewhere. Decrypted (`age -d -i key.txt file | bsdtar -x`, or [`restore`]) it holds:
//! - `manifest.json`: format, time, schema version, documents, and the SHA-256 of every file;
//! - `wordcraft.db`: a consistent in-memory copy of the database (SQLite's online backup API, while the server
//!   runs: SQLite in WAL lets it read without stopping writers);
//! - `documents/<uuid>/<revision>.snapshot`: every snapshot that copy refers to, each checked
//!   against the checksum the database recorded.
//!
//! Snapshot files never change once written (their key is document + revision), so the copy is
//! consistent: every file the database copy names is read and verified. The archive is built in
//! memory, capped at [`MAX_BACKUP_BYTES`] — ample for a homelab — so no unencrypted copy is ever
//! written to disk.
//!
//! [`restore`] decrypts into an empty directory, checks every checksum, then opens every
//! document the way the server does (newest usable snapshot + the operation log after it), so a
//! backup that restores is one the server can run on. [`verify`] does the same in a temporary
//! directory and removes it.

use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{Connection, MAIN_DB, OpenFlags};
use serde::{Deserialize, Serialize};
use wordcraft_collab_protocol::DocumentId;

use crate::persistence::{Database, Timestamp};
use crate::storage::{self, DocumentStorage, StoredSnapshot};
use crate::{Config, documents};

/// The archive layout this code writes and reads.
pub const FORMAT: &str = "wordcraft-collab-backup/1";

/// Largest backup made or restored (the archive lives in memory while it is encrypted).
pub const MAX_BACKUP_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Backups kept by default; older ones in the backup directory are removed.
pub const DEFAULT_KEEP: usize = 14;

const PREFIX: &str = "wordcraft-";
const SUFFIX: &str = ".wcbackup.age";
const DATABASE: &str = "wordcraft.db";
const MANIFEST: &str = "manifest.json";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backup settings: {0}")]
    Config(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("encryption: {0}")]
    Encrypt(#[from] age::EncryptError),
    #[error("decryption: {0}")]
    Decrypt(#[from] age::DecryptError),
    #[error("this backup is damaged or incomplete: {0}")]
    Damaged(String),
    #[error("the restored data does not open: {0}")]
    Unusable(String),
}

/// Where backups go, who can read them, and how many to keep (from the environment).
pub struct BackupConfig {
    /// `BACKUP_DIR`, default `backups` (the container uses `/backups`, a volume of its own, so
    /// syncing it off the machine never picks up the live database).
    pub dir: PathBuf,
    /// `BACKUP_RECIPIENTS`: age public keys (`age1…`), separated by commas or whitespace.
    pub recipients: Vec<age::x25519::Recipient>,
    /// `BACKUP_KEEP`, default [`DEFAULT_KEEP`]; 0 keeps every backup.
    pub keep: usize,
}

impl BackupConfig {
    pub fn from_env() -> Result<Self, BackupError> {
        Self::from_vars(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, BackupError> {
        let dir = dir_from(&var);
        let recipients = parse_recipients(&var("BACKUP_RECIPIENTS").unwrap_or_default())?;
        let keep = match var("BACKUP_KEEP") {
            Some(n) => n.trim().parse().map_err(|_| BackupError::Config(format!("BACKUP_KEEP must be a number, not {n:?}")))?,
            None => DEFAULT_KEEP,
        };
        Ok(Self { dir, recipients, keep })
    }
}

/// `BACKUP_DIR`, default `backups`: where backups are written and `latest` is looked for.
pub fn dir_from_env() -> PathBuf {
    dir_from(&|name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

fn dir_from(var: &impl Fn(&str) -> Option<String>) -> PathBuf {
    PathBuf::from(var("BACKUP_DIR").unwrap_or_else(|| "backups".into()))
}

/// age X25519 public keys, separated by commas or whitespace; at least one.
pub fn parse_recipients(text: &str) -> Result<Vec<age::x25519::Recipient>, BackupError> {
    let keys: Vec<_> = text.split(|c: char| c == ',' || c.is_whitespace()).filter(|k| !k.is_empty()).collect();
    if keys.is_empty() {
        return Err(BackupError::Config("set BACKUP_RECIPIENTS to at least one age public key (age1…); make one with `age-keygen`".into()));
    }
    keys.into_iter()
        .map(|k| age::x25519::Recipient::from_str(k).map_err(|e| BackupError::Config(format!("{k:?} is not an age public key: {e}"))))
        .collect()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    created_at: String,
    schema_version: i64,
    documents: usize,
    /// Every file in the archive but the manifest, with its SHA-256.
    files: Vec<ManifestFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    sha256: String,
}

/// What a backup or a restore covered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub path: PathBuf,
    pub documents: usize,
    pub snapshots: usize,
    pub bytes: u64,
}

/// Make one backup of the server configured by `config` into `backup.dir`, then remove the
/// oldest beyond `backup.keep`. Safe while the server runs.
pub fn create(config: &Config, backup: &BackupConfig) -> Result<Summary, BackupError> {
    std::fs::create_dir_all(&backup.dir)?;
    let created_at = Timestamp::now();
    let stamp: String = created_at.as_str().chars().filter(char::is_ascii_alphanumeric).collect();
    let name = format!("{PREFIX}{stamp}{SUFFIX}");

    // Never place plaintext in BACKUP_DIR: a sync service could copy it before deletion.
    let mut copy = copy_database(config)?;
    let schema_version: i64 = copy.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let documents: i64 = copy.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
    let documents = usize::try_from(documents).map_err(|_| BackupError::Damaged("invalid document count".into()))?;
    let snapshots = {
        let mut stmt = copy.prepare("SELECT storage_key, sha256 FROM document_snapshots ORDER BY storage_key")?;
        stmt.query_map([], |r| Ok(StoredSnapshot { storage_key: r.get(0)?, sha256: r.get(1)? }))?.collect::<Result<Vec<_>, _>>()?
    };
    let database = copy.serialize(MAIN_DB)?;
    let (plain, snapshots) = archive(config, &database, schema_version, documents, &snapshots, created_at.as_str())?;
    let bytes = encrypt_to_file(&plain, &backup.recipients, &backup.dir, &name)?;
    prune(&backup.dir, backup.keep)?;
    Ok(Summary { path: backup.dir.join(name), documents, snapshots, bytes })
}

/// A read transaction pins one WAL revision, including the size check before allocating the
/// destination. Writers continue while we copy; no plaintext database file is ever created.
fn copy_database(config: &Config) -> Result<Connection, BackupError> {
    let mut live = Connection::open_with_flags(&config.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    live.busy_timeout(Duration::from_secs(30))?;
    let transaction = live.transaction()?;
    let pages: i64 = transaction.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = transaction.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    if pages.checked_mul(page_size).is_none_or(|size| !(0..=MAX_BACKUP_BYTES as i64).contains(&size)) {
        return Err(BackupError::Config("the database is too large for one backup".into()));
    }
    let mut copy = Connection::open_in_memory()?;
    {
        let copier = rusqlite::backup::Backup::new(&transaction, &mut copy)?;
        let started = std::time::Instant::now();
        loop {
            if copier.step(-1)? == rusqlite::backup::StepResult::Done {
                break;
            }
            if started.elapsed() >= Duration::from_secs(30) {
                return Err(BackupError::Config("the database stayed busy for 30 seconds; try the backup again".into()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(copy)
}

/// The zip, in memory: the manifest, the database copy and every snapshot it names.
fn archive(
    config: &Config,
    database: &[u8],
    schema_version: i64,
    documents: usize,
    snapshots: &[StoredSnapshot],
    created_at: &str,
) -> Result<(Vec<u8>, usize), BackupError> {
    let storage = DocumentStorage::open(&config.document_storage)?;
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let mut files = vec![ManifestFile { path: DATABASE.into(), sha256: storage::sha256_hex(database) }];
    let mut total = database.len() as u64;
    if total > MAX_BACKUP_BYTES {
        return Err(BackupError::Config("the database is too large for one backup".into()));
    }
    zip.start_file(DATABASE, options)?;
    zip.write_all(database)?;
    for snapshot in snapshots {
        // Checked against the database's checksum on the way in: a damaged file is not backed
        // up as if it were good.
        let bytes = storage.read_snapshot(snapshot).map_err(|e| BackupError::Damaged(format!("snapshot {}: {e}", snapshot.storage_key)))?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_BACKUP_BYTES {
            return Err(BackupError::Config(format!("the data is over {} GiB, too large for one backup", MAX_BACKUP_BYTES >> 30)));
        }
        let path = format!("documents/{}", snapshot.storage_key);
        zip.start_file(path.as_str(), options)?;
        zip.write_all(&bytes)?;
        files.push(ManifestFile { path, sha256: snapshot.sha256.clone() });
    }
    let manifest = Manifest { format: FORMAT.into(), created_at: created_at.into(), schema_version, documents, files };
    let manifest = serde_json::to_vec_pretty(&manifest).map_err(|e| BackupError::Damaged(e.to_string()))?;
    if manifest.len() as u64 > MAX_MANIFEST_BYTES || total.saturating_add(manifest.len() as u64) > MAX_BACKUP_BYTES {
        return Err(BackupError::Config("the manifest and data are too large for one backup".into()));
    }
    zip.start_file(MANIFEST, options)?;
    zip.write_all(&manifest)?;
    let plain = zip.finish()?.into_inner();
    if plain.len() as u64 > MAX_BACKUP_BYTES {
        return Err(BackupError::Config("the archive is too large for one backup".into()));
    }
    Ok((plain, snapshots.len()))
}

/// Encrypt `plain` to `recipients` as `dir/name`, written under a temporary name, synced and
/// renamed, so a backup file is either complete or absent.
fn encrypt_to_file(plain: &[u8], recipients: &[age::x25519::Recipient], dir: &Path, name: &str) -> Result<u64, BackupError> {
    let encryptor = age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))?;
    // Atomic, private creation: concurrent jobs cannot truncate each other's temporary file
    // or follow a symlink. A timestamp collision must not replace an existing good backup.
    let partial = tempfile::Builder::new().prefix(".wordcraft-").suffix(".partial").tempfile_in(dir)?;
    let mut writer = encryptor.wrap_output(std::io::BufWriter::new(partial))?;
    writer.write_all(plain)?;
    let file = writer.finish()?.into_inner().map_err(|e| e.into_error())?;
    file.as_file().sync_all()?;
    let bytes = file.as_file().metadata()?.len();
    file.persist_noclobber(dir.join(name)).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(bytes)
}

/// Remove the oldest backups in `dir` beyond `keep` (0 keeps them all). Only files this code
/// names are ever touched.
pub fn prune(dir: &Path, keep: usize) -> Result<Vec<PathBuf>, BackupError> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    let mut backups = list(dir)?;
    let excess = backups.len().saturating_sub(keep);
    let removed: Vec<PathBuf> = backups.drain(..excess).collect();
    for path in &removed {
        std::fs::remove_file(path)?;
    }
    Ok(removed)
}

/// The backups in `dir`, oldest first (their names sort by time).
pub fn list(dir: &Path) -> Result<Vec<PathBuf>, BackupError> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX)))
        .collect();
    found.sort();
    Ok(found)
}

/// Read the age identities (private keys) in `identity_file`, as `age-keygen` writes them;
/// `-` reads them from standard input (`… --identity - < key.txt`), so the key needs no file the
/// server's user can read.
pub fn identities(identity_file: &Path) -> Result<Vec<Box<dyn age::Identity>>, BackupError> {
    let file = if identity_file == Path::new("-") {
        age::IdentityFile::from_buffer(std::io::BufReader::new(std::io::stdin().lock()))?
    } else {
        age::IdentityFile::from_file(identity_file.to_string_lossy().into_owned())?
    };
    Ok(file.into_identities()?)
}

/// Decrypt `backup` with `identities` into `target` (missing or empty), check every file, and
/// open every document as the server would. The server must not run on `target`.
pub fn restore(backup: &Path, identities: &[Box<dyn age::Identity>], target: &Path) -> Result<Summary, BackupError> {
    if target.exists() && std::fs::read_dir(target)?.next().is_some() {
        return Err(BackupError::Config(format!("{} is not empty: restore into a new directory, then switch the server to it", target.display())));
    }
    let plain = decrypt(backup, identities)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(plain.as_slice()))?;
    let manifest = read_manifest(&mut zip)?;
    let documents_dir = target.join("documents");
    std::fs::create_dir_all(&documents_dir)?;
    let mut snapshots = 0;
    for file in &manifest.files {
        let bytes = read_entry(&mut zip, &file.path)?;
        if storage::sha256_hex(&bytes) != file.sha256 {
            return Err(BackupError::Damaged(format!("{} does not match its checksum", file.path)));
        }
        let destination = if file.path == DATABASE {
            target.join(DATABASE)
        } else {
            let key = file.path.strip_prefix("documents/").filter(|k| storage::is_valid_key(k));
            let key = key.ok_or_else(|| BackupError::Damaged(format!("unexpected file {:?}", file.path)))?;
            snapshots += 1;
            documents_dir.join(key)
        };
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&destination, &bytes)?;
    }
    if !target.join(DATABASE).is_file() {
        return Err(BackupError::Damaged("no database in the backup".into()));
    }
    let documents = open_everything(target)?;
    if documents != manifest.documents {
        return Err(BackupError::Damaged(format!("the manifest lists {} documents, the database {documents}", manifest.documents)));
    }
    Ok(Summary { path: target.to_path_buf(), documents, snapshots, bytes: plain.len() as u64 })
}

/// [`restore`] into a temporary directory of the system's (the backup folder may be read-only
/// or synced elsewhere), then remove it: proof that the backup restores, touching nothing.
pub fn verify(backup: &Path, identities: &[Box<dyn age::Identity>]) -> Result<Summary, BackupError> {
    // Atomically created with private permissions, never a predictable shared /tmp path.
    let scratch = tempfile::Builder::new().prefix("wordcraft-verify-").tempdir()?;
    let result = restore(backup, identities, scratch.path());
    result.map(|s| Summary { path: backup.to_path_buf(), ..s })
}

fn decrypt(backup: &Path, identities: &[Box<dyn age::Identity>]) -> Result<Vec<u8>, BackupError> {
    let file = std::io::BufReader::new(std::fs::File::open(backup)?);
    let decryptor = age::Decryptor::new(file)?;
    let mut reader = decryptor.decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity))?;
    let mut plain = Vec::new();
    (&mut reader).take(MAX_BACKUP_BYTES + 1).read_to_end(&mut plain)?;
    if plain.len() as u64 > MAX_BACKUP_BYTES {
        return Err(BackupError::Damaged("larger than any backup this server makes".into()));
    }
    Ok(plain)
}

fn read_manifest(zip: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Result<Manifest, BackupError> {
    let bytes = read_entry_bounded(zip, MANIFEST, MAX_MANIFEST_BYTES)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| BackupError::Damaged(format!("manifest: {e}")))?;
    if manifest.format != FORMAT {
        return Err(BackupError::Damaged(format!("format {:?}; this server reads {FORMAT:?}", manifest.format)));
    }
    validate_entries(zip, &manifest, MAX_BACKUP_BYTES)?;
    Ok(manifest)
}

/// Check the whole expansion budget before writing anything. Per-entry limits alone let a
/// small compressed archive expand to an unbounded number of individually acceptable files.
fn validate_entries(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, manifest: &Manifest, max: u64) -> Result<(), BackupError> {
    let mut names = HashSet::new();
    let mut total = zip.by_name(MANIFEST)?.size();
    for file in &manifest.files {
        if (file.path != DATABASE && !file.path.strip_prefix("documents/").is_some_and(storage::is_valid_key)) || !names.insert(file.path.as_str()) {
            return Err(BackupError::Damaged(format!("unexpected or repeated file {:?}", file.path)));
        }
        let entry = zip.by_name(&file.path).map_err(|_| BackupError::Damaged(format!("{} is missing", file.path)))?;
        total = total.saturating_add(entry.size());
        if total > max {
            return Err(BackupError::Damaged("the expanded archive is too large".into()));
        }
    }
    if !names.contains(DATABASE) || zip.len() != names.len().saturating_add(1) {
        return Err(BackupError::Damaged("the archive must contain exactly the manifest, database and listed snapshots".into()));
    }
    Ok(())
}

fn read_entry(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, path: &str) -> Result<Vec<u8>, BackupError> {
    read_entry_bounded(zip, path, MAX_BACKUP_BYTES)
}

fn read_entry_bounded(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, path: &str, max: u64) -> Result<Vec<u8>, BackupError> {
    let entry = zip.by_name(path).map_err(|_| BackupError::Damaged(format!("{path} is missing")))?;
    let size = entry.size();
    if size > max {
        return Err(BackupError::Damaged(format!("{path} is too large")));
    }
    let mut bytes = Vec::new();
    // Bounded: a crafted archive cannot expand past the largest backup.
    entry.take(size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(BackupError::Damaged(format!("{path} does not match its declared size")));
    }
    Ok(bytes)
}

/// Open the restored database (running any migration a newer server adds) and every document
/// in it, as the server does on first use. Returns how many documents opened.
fn open_everything(target: &Path) -> Result<usize, BackupError> {
    let db = std::sync::Arc::new(Database::open(&target.join(DATABASE)).map_err(|e| BackupError::Unusable(e.to_string()))?);
    let storage = std::sync::Arc::new(DocumentStorage::open(&target.join("documents"))?);
    let ids: Vec<(DocumentId, u64)> = {
        let conn = db.lock().map_err(|e| BackupError::Unusable(e.to_string()))?;
        let mut stmt = conn.prepare("SELECT id, current_revision FROM documents ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut ids = Vec::new();
        for row in rows {
            let (id, revision) = row?;
            let id = uuid::Uuid::parse_str(&id).map_err(|e| BackupError::Unusable(format!("document id {id:?}: {e}")))?;
            let revision = u64::try_from(revision).map_err(|_| BackupError::Unusable(format!("document {id} has a negative revision")))?;
            ids.push((DocumentId(id), revision));
        }
        ids
    };
    // On a thread and runtime of its own: callers may already be inside a Tokio runtime (where
    // `block_on` would panic) or have none.
    std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<(), BackupError> {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(open_all(&db, &storage, &ids))
            })
            .join()
            .map_err(|_| BackupError::Unusable("opening the restored documents stopped unexpectedly".into()))?
    })?;
    Ok(ids.len())
}

async fn open_all(db: &std::sync::Arc<Database>, storage: &std::sync::Arc<DocumentStorage>, ids: &[(DocumentId, u64)]) -> Result<(), BackupError> {
    {
        for &(id, revision) in ids {
            let handle = documents::open(db.clone(), storage.clone(), id)
                .await
                .map_err(|r| BackupError::Unusable(format!("document {}: {}", id.0, r.message)))?;
            let (at, _) = handle.current().await.map_err(|r| BackupError::Unusable(format!("document {}: {}", id.0, r.message)))?;
            if at != revision {
                return Err(BackupError::Unusable(format!("document {} opened at revision {at}, the database says {revision}", id.0)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_expansion_is_bounded_in_total_not_only_per_file() {
        let snapshot = "documents/00000000-0000-0000-0000-000000000001/00000000000000000000.snapshot";
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in [(MANIFEST, b"{}".as_slice()), (DATABASE, b"123456"), (snapshot, b"123456")] {
            writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(bytes).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let mut manifest = Manifest {
            format: FORMAT.into(),
            created_at: "test".into(),
            schema_version: 4,
            documents: 1,
            files: [DATABASE, snapshot].into_iter().map(|path| ManifestFile { path: path.into(), sha256: String::new() }).collect(),
        };
        assert!(validate_entries(&mut zip, &manifest, 14).is_ok());
        assert!(matches!(validate_entries(&mut zip, &manifest, 13), Err(BackupError::Damaged(_))));
        // A duplicate path must not overwrite an earlier restored entry.
        manifest.files.push(ManifestFile { path: DATABASE.into(), sha256: String::new() });
        assert!(matches!(validate_entries(&mut zip, &manifest, 100), Err(BackupError::Damaged(_))));
        manifest.files.truncate(1);
        assert!(matches!(validate_entries(&mut zip, &manifest, 100), Err(BackupError::Damaged(_))), "unlisted archive entry");
        assert!(matches!(read_entry_bounded(&mut zip, DATABASE, 5), Err(BackupError::Damaged(_))));
    }

    #[test]
    fn the_consistent_database_copy_has_no_plaintext_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: dir.path().join(DATABASE),
            document_storage: dir.path().join("documents"),
            oidc: None,
        };
        let live = Database::open(&config.database_path).unwrap();
        live.lock().unwrap().execute("INSERT INTO users (id, display_name, created_at) VALUES ('user', 'Private name', 'now')", []).unwrap();
        let copy = copy_database(&config).unwrap();
        let path: String = copy.query_row("PRAGMA database_list", [], |row| row.get(2)).unwrap();
        assert!(path.is_empty(), "a backup copy must live only in memory: {path}");
        let name: String = copy.query_row("SELECT display_name FROM users", [], |row| row.get(0)).unwrap();
        assert_eq!(name, "Private name", "the copy includes committed WAL data");
    }

    #[test]
    fn encryption_does_not_replace_existing_backups_or_leave_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();
        let recipients = [identity.to_public()];
        let name = "wordcraft-test.wcbackup.age";
        encrypt_to_file(b"first backup", &recipients, dir.path(), name).unwrap();
        let original = std::fs::read(dir.path().join(name)).unwrap();
        assert!(encrypt_to_file(b"second backup", &recipients, dir.path(), name).is_err());
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.path().join(name)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // An encryption failure cleans up too.
        assert!(encrypt_to_file(b"no recipients", &[], dir.path(), "bad.age").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
