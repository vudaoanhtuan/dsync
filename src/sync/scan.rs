//! Indexing helpers: turn a flat `Vec<FileEntry>` into file/dir maps and count skipped symlinks.
//! The actual transport scan happens in `mod.rs` (it needs the concrete sender type for remote
//! ignore-pattern filtering). See specs/sync-engine.md, Stage 1.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::transport::{EntryKind, FileEntry};

pub struct Indexed {
    pub files: HashMap<PathBuf, FileEntry>,
    pub dirs: HashMap<PathBuf, FileEntry>,
    pub symlinks: usize,
}

/// Split entries into files and dirs, dropping (and counting) symlinks.
pub fn index(entries: Vec<FileEntry>) -> Indexed {
    let mut files = HashMap::new();
    let mut dirs = HashMap::new();
    let mut symlinks = 0usize;
    for e in entries {
        match e.kind {
            EntryKind::File => {
                files.insert(e.rel_path.clone(), e);
            }
            EntryKind::Dir => {
                dirs.insert(e.rel_path.clone(), e);
            }
            EntryKind::Symlink => symlinks += 1,
        }
    }
    Indexed {
        files,
        dirs,
        symlinks,
    }
}
