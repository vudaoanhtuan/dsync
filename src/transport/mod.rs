//! The `Transport` abstraction: all filesystem access (local or remote) goes through this trait,
//! so the sync engine is written once against `dyn Transport`. See specs/transport.md.

pub mod local;
pub mod protocol;
pub mod ssh;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::delta::{Delta, Signature};
use crate::error::Result;
use crate::ignore::IgnoreSet;

pub use local::LocalTransport;
pub use ssh::SshTransport;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    /// Relative to the transport root.
    pub rel_path: PathBuf,
    pub len: u64,
    /// Unix milliseconds (consistent everywhere: protocol, write, plan).
    pub mtime: i64,
    pub kind: EntryKind,
    /// Unix permission bits.
    pub mode: u32,
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Recursively list entries under root. The RECEIVER is scanned with `ignore = None`;
    /// the SENDER (source) is scanned with the source-resolved ignore set. See ignore.md.
    async fn scan(&self, ignore: Option<&IgnoreSet>) -> Result<Vec<FileEntry>>;

    /// Block signature of an existing file (basis side). None if file absent.
    async fn signature(&self, rel: &Path) -> Result<Option<Signature>>;

    /// Produce a delta turning the basis described by `sig` into THIS side's current file.
    async fn diff(&self, rel: &Path, sig: &Signature) -> Result<Delta>;

    /// Apply a delta atomically (temp + rename), set mtime/mode, return blake3 of the result.
    async fn patch(&self, rel: &Path, delta: &Delta, mtime: i64, mode: u32) -> Result<[u8; 32]>;

    /// Whole-file fast path: write `data` atomically, set mtime/mode, return blake3.
    async fn write_file(&self, rel: &Path, data: &[u8], mtime: i64, mode: u32) -> Result<[u8; 32]>;

    /// Read whole-file content (for the fast path).
    async fn read_file(&self, rel: &Path) -> Result<Vec<u8>>;

    /// blake3 of an existing file (for --checksum diffing and verification).
    async fn hash(&self, rel: &Path) -> Result<[u8; 32]>;

    async fn mkdir_all(&self, rel: &Path, mode: u32) -> Result<()>;

    /// Remove a file or empty dir.
    async fn remove(&self, rel: &Path) -> Result<()>;

    /// On-wire bytes sent so far (post-compression for SSH; delta/whole-file length for local).
    /// Used by the engine to compute "bytes saved by delta+zstd".
    fn bytes_sent(&self) -> u64;

    /// Tear down (close SSH channels). No-op for local.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
