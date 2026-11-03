//! Snapshot files on the homelab's file system (design §9, "Estrategia de guardado").
//!
//! The authoritative format is WordCraft's internal document plus the block identity registry
//! and the operation log; DOCX is only import/export. Snapshots (every 100 operations or 5 min)
//! are written atomically (temp file, fsync, rename) with a SHA-256 recorded in
//! `document_snapshots`, so recovery can pick the last valid one (milestone C2.4).

use std::path::{Path, PathBuf};

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
}
