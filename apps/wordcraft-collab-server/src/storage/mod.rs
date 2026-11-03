//! Snapshot files on the homelab's file system (design §9, "Estrategia de guardado"; C2.4).
//!
//! The authoritative format is WordCraft's internal document plus the block identity registry
//! (a `collab-model` snapshot) and the operation log; DOCX is only import/export. A snapshot is
//! written atomically — temporary file in the same directory, fsync, rename, fsync of the
//! directory — and its SHA-256 recorded in `document_snapshots`, so recovery can tell a damaged
//! file and fall back to an older one.
//!
//! Layout: `<root>/<document uuid>/<revision, 20 digits>.snapshot`. Keys stored in the database
//! are relative to the root and checked against that shape before use, so a tampered row cannot
//! point outside it.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use wordcraft_collab_protocol::{DocumentId, Revision};

/// Largest snapshot read back, in bytes (allocation cap against a damaged or hostile file).
pub const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("snapshot storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot key {0:?} is not one this server writes")]
    BadKey(String),
    #[error("snapshot {key} is {size} bytes, over the {max}-byte limit")]
    TooLarge { key: String, size: u64, max: u64 },
    #[error("snapshot {key} does not match its checksum")]
    Checksum { key: String },
}

/// Where a snapshot was written, as recorded in the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub storage_key: String,
    pub sha256: String,
}

pub struct DocumentStorage {
    root: PathBuf,
}

impl DocumentStorage {
    /// Use `root`, creating it if needed.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self { root: root.to_path_buf() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory is still there (the volume may have gone away).
    pub fn is_available(&self) -> bool {
        self.root.is_dir()
    }

    /// Write `bytes` as the snapshot of `document` at `revision`, atomically. Rewriting the same
    /// revision replaces the file (same content: snapshots of one revision are identical).
    pub fn write_snapshot(&self, document: DocumentId, revision: Revision, bytes: &[u8]) -> Result<StoredSnapshot, StorageError> {
        let key = key_for(document, revision);
        let path = self.root.join(&key);
        let dir = path.parent().ok_or_else(|| StorageError::BadKey(key.clone()))?;
        std::fs::create_dir_all(dir)?;
        let temp = dir.join(format!(".{revision:020}.snapshot.tmp"));
        {
            let mut file = File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, &path)?;
        sync_dir(dir)?;
        Ok(StoredSnapshot { storage_key: key, sha256: sha256_hex(bytes) })
    }

    /// Whether a snapshot file for `document` at `revision` is on disk.
    pub fn has_snapshot(&self, document: DocumentId, revision: Revision) -> bool {
        self.root.join(key_for(document, revision)).exists()
    }

    /// Read a snapshot back, checking its key, size and checksum.
    pub fn read_snapshot(&self, stored: &StoredSnapshot) -> Result<Vec<u8>, StorageError> {
        let key = &stored.storage_key;
        if !is_valid_key(key) {
            return Err(StorageError::BadKey(key.clone()));
        }
        let file = File::open(self.root.join(key))?;
        let size = file.metadata()?.len();
        if size > MAX_SNAPSHOT_BYTES {
            return Err(StorageError::TooLarge { key: key.clone(), size, max: MAX_SNAPSHOT_BYTES });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        file.take(MAX_SNAPSHOT_BYTES).read_to_end(&mut bytes)?;
        if sha256_hex(&bytes) != stored.sha256 {
            return Err(StorageError::Checksum { key: key.clone() });
        }
        Ok(bytes)
    }
}

fn key_for(document: DocumentId, revision: Revision) -> String {
    format!("{}/{revision:020}.snapshot", document.0.hyphenated())
}

/// `<uuid>/<20 digits>.snapshot` and nothing else.
fn is_valid_key(key: &str) -> bool {
    let Some((dir, file)) = key.split_once('/') else { return false };
    let Some(digits) = file.strip_suffix(".snapshot") else { return false };
    uuid::Uuid::try_parse(dir).is_ok_and(|u| u.hyphenated().to_string() == dir) && digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Make a rename durable: fsync the directory holding it (POSIX; elsewhere the rename is what
/// the platform offers).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_storage(name: &str) -> (PathBuf, DocumentStorage) {
        let dir = std::env::temp_dir().join(format!("wordcraft-collab-storage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = DocumentStorage::open(&dir).unwrap();
        (dir, storage)
    }

    #[test]
    fn snapshots_round_trip_and_detect_damage() {
        let (dir, storage) = temp_storage("roundtrip");
        let doc = DocumentId(Uuid::from_u128(7));
        let stored = storage.write_snapshot(doc, 42, b"{\"format\":1}").unwrap();
        assert_eq!(stored.storage_key, "00000000-0000-0000-0000-000000000007/00000000000000000042.snapshot");
        assert_eq!(storage.read_snapshot(&stored).unwrap(), b"{\"format\":1}");
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(dir.join("00000000-0000-0000-0000-000000000007")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1);

        std::fs::write(dir.join(&stored.storage_key), b"{\"format\":2}").unwrap();
        assert!(matches!(storage.read_snapshot(&stored), Err(StorageError::Checksum { .. })));
        std::fs::remove_file(dir.join(&stored.storage_key)).unwrap();
        assert!(matches!(storage.read_snapshot(&stored), Err(StorageError::Io(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_from_the_database_cannot_escape_the_root() {
        let (dir, storage) = temp_storage("keys");
        for key in [
            "../../etc/passwd",
            "/etc/passwd",
            "00000000-0000-0000-0000-000000000007/../../x.snapshot",
            "00000000-0000-0000-0000-000000000007/42.snapshot",
            "00000000000000000000000000000007/00000000000000000042.snapshot",
            "00000000-0000-0000-0000-000000000007/0000000000000000004a.snapshot",
            "",
        ] {
            let stored = StoredSnapshot { storage_key: key.into(), sha256: String::new() };
            assert!(matches!(storage.read_snapshot(&stored), Err(StorageError::BadKey(_))), "{key}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksums_are_lowercase_hex_sha256() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
