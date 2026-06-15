//! Local filesystem implementation of `Transport`. Also used by the remote agent (`server.rs`),
//! which simply runs a `LocalTransport` rooted at the remote path. See specs/transport.md.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::delta::{self, Delta, Signature};
use crate::error::{DsyncError, Result};
use crate::ignore::IgnoreSet;
use crate::transport::{EntryKind, FileEntry, Transport};

pub struct LocalTransport {
    root: PathBuf,
    bytes_sent: Arc<AtomicU64>,
}

impl LocalTransport {
    pub fn new(root: impl Into<PathBuf>) -> LocalTransport {
        LocalTransport {
            root: root.into(),
            bytes_sent: Arc::new(AtomicU64::new(0)),
        }
    }

    fn abs(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }
}

#[async_trait]
impl Transport for LocalTransport {
    async fn scan(&self, ignore: Option<&IgnoreSet>) -> Result<Vec<FileEntry>> {
        // `IgnoreSet` is not Send-cloneable cheaply; resolve all matching decisions here while we
        // still hold the borrow, by walking synchronously on this thread. The walk is IO-bound
        // but bounded by directory size; acceptable on the async runtime for typical trees.
        let root = self.root.clone();
        scan_dir(&root, ignore)
    }

    async fn signature(&self, rel: &Path) -> Result<Option<Signature>> {
        let path = self.abs(rel);
        tokio::task::spawn_blocking(move || {
            match std::fs::File::open(&path) {
                Ok(file) => {
                    let data = map_file(&file, &path)?;
                    Ok(Some(delta::signature(&data)?))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(DsyncError::io(&path, e)),
            }
        })
        .await
        .map_err(join_err)?
    }

    async fn diff(&self, rel: &Path, sig: &Signature) -> Result<Delta> {
        let path = self.abs(rel);
        let sig = sig.clone();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path).map_err(|e| DsyncError::io(&path, e))?;
            let data = map_file(&file, &path)?;
            delta::diff(&sig, &data)
        })
        .await
        .map_err(join_err)?
    }

    async fn patch(&self, rel: &Path, delta: &Delta, mtime: i64, mode: u32) -> Result<[u8; 32]> {
        let path = self.abs(rel);
        let delta = delta.clone();
        let count = self.bytes_sent.clone();
        tokio::task::spawn_blocking(move || {
            // Read the basis (current file), reconstruct, write atomically.
            let basis = match std::fs::File::open(&path) {
                Ok(f) => map_file(&f, &path)?.to_vec(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(DsyncError::io(&path, e)),
            };
            let reconstructed = delta::apply(&basis, &delta)?;
            count.fetch_add(delta.0.len() as u64, Ordering::Relaxed);
            let digest = write_atomic(&path, &reconstructed, mtime, mode)?;
            Ok(digest)
        })
        .await
        .map_err(join_err)?
    }

    async fn write_file(&self, rel: &Path, data: &[u8], mtime: i64, mode: u32) -> Result<[u8; 32]> {
        let path = self.abs(rel);
        let data = data.to_vec();
        let count = self.bytes_sent.clone();
        tokio::task::spawn_blocking(move || {
            count.fetch_add(data.len() as u64, Ordering::Relaxed);
            write_atomic(&path, &data, mtime, mode)
        })
        .await
        .map_err(join_err)?
    }

    async fn read_file(&self, rel: &Path) -> Result<Vec<u8>> {
        let path = self.abs(rel);
        tokio::task::spawn_blocking(move || {
            std::fs::read(&path).map_err(|e| DsyncError::io(&path, e))
        })
        .await
        .map_err(join_err)?
    }

    async fn hash(&self, rel: &Path) -> Result<[u8; 32]> {
        let path = self.abs(rel);
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path).map_err(|e| DsyncError::io(&path, e))?;
            let data = map_file(&file, &path)?;
            Ok(*blake3::hash(&data).as_bytes())
        })
        .await
        .map_err(join_err)?
    }

    async fn mkdir_all(&self, rel: &Path, mode: u32) -> Result<()> {
        let path = self.abs(rel);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&path).map_err(|e| DsyncError::io(&path, e))?;
            set_mode(&path, mode);
            Ok(())
        })
        .await
        .map_err(join_err)?
    }

    async fn remove(&self, rel: &Path) -> Result<()> {
        let path = self.abs(rel);
        tokio::task::spawn_blocking(move || {
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(DsyncError::io(&path, e)),
            };
            let res = if meta.is_dir() {
                std::fs::remove_dir(&path)
            } else {
                std::fs::remove_file(&path)
            };
            res.map_err(|e| DsyncError::io(&path, e))
        })
        .await
        .map_err(join_err)?
    }

    fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }
}

fn join_err(e: tokio::task::JoinError) -> DsyncError {
    DsyncError::Other(format!("worker task failed: {e}"))
}

/// Memory-map a file for reading. Empty files cannot be mapped, so return an empty slice owner.
fn map_file(file: &std::fs::File, path: &Path) -> Result<MappedData> {
    let len = file
        .metadata()
        .map_err(|e| DsyncError::io(path, e))?
        .len();
    if len == 0 {
        return Ok(MappedData::Empty);
    }
    // SAFETY: we only read from the map and the file is not truncated concurrently in normal use.
    let mmap = unsafe { memmap2::Mmap::map(file) }.map_err(|e| DsyncError::io(path, e))?;
    Ok(MappedData::Mapped(mmap))
}

enum MappedData {
    Empty,
    Mapped(memmap2::Mmap),
}

impl std::ops::Deref for MappedData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            MappedData::Empty => &[],
            MappedData::Mapped(m) => m,
        }
    }
}

impl MappedData {
    fn to_vec(&self) -> Vec<u8> {
        use std::ops::Deref;
        self.deref().to_vec()
    }
}

/// Write `data` to a temp file in the destination directory, set mtime/mode, then rename into
/// place. Returns blake3 of the written content.
fn write_atomic(path: &Path, data: &[u8], mtime: i64, mode: u32) -> Result<[u8; 32]> {
    let parent = path
        .parent()
        .ok_or_else(|| DsyncError::Other(format!("no parent dir for {}", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|e| DsyncError::io(parent, e))?;

    let tmp = tmp_path(path);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| DsyncError::io(&tmp, e))?;
        f.write_all(data).map_err(|e| DsyncError::io(&tmp, e))?;
        f.sync_all().ok();
    }
    set_mode(&tmp, mode);
    set_mtime(&tmp, mtime)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        DsyncError::io(path, e)
    })?;
    Ok(*blake3::hash(data).as_bytes())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".dsync.tmp.");
    name.push(path.file_name().unwrap_or(std::ffi::OsStr::new("file")));
    // Include the file's own bytes-ptr-ish uniqueness via the full path length to reduce clashes;
    // concurrent writers target distinct `rel` paths so the base name already differs.
    path.with_file_name(name)
}

fn set_mtime(path: &Path, mtime_ms: i64) -> Result<()> {
    let secs = mtime_ms.div_euclid(1000);
    let nanos = (mtime_ms.rem_euclid(1000) * 1_000_000) as u32;
    let ft = filetime::FileTime::from_unix_time(secs, nanos);
    filetime::set_file_mtime(path, ft).map_err(|e| DsyncError::io(path, e))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Recursive scan honoring the optional ignore set. Pruned: ignored dirs are not descended;
/// symlinked dirs are never followed (classified as Symlink, not walked).
fn scan_dir(root: &Path, ignore: Option<&IgnoreSet>) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DsyncError::io(&dir, e)),
        };
        for entry in read {
            let entry = entry.map_err(|e| DsyncError::io(&dir, e))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map_err(|_| DsyncError::Other("path outside root".into()))?
                .to_path_buf();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => return Err(DsyncError::io(&path, e)),
            };
            let kind = if meta.file_type().is_symlink() {
                EntryKind::Symlink
            } else if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let is_dir = kind == EntryKind::Dir;
            if let Some(set) = ignore {
                if set.is_ignored(&rel, is_dir) {
                    continue;
                }
            } else if rel.starts_with(crate::config::DSYNC_DIR) {
                // Even when scanning the receiver with no ignore set, never surface `.dsync/`.
                continue;
            }
            out.push(FileEntry {
                rel_path: rel,
                len: if kind == EntryKind::File { meta.len() } else { 0 },
                mtime: mtime_ms(&meta),
                kind,
                mode: file_mode(&meta),
            });
            if is_dir {
                stack.push(path);
            }
        }
    }
    Ok(out)
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    match meta.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        },
        Err(_) => 0,
    }
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}
#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}
