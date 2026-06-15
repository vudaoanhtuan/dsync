//! Stage 2 — compare sender/receiver scans into create/transfer/delete sets. See
//! specs/sync-engine.md, Stage 2.

use std::path::PathBuf;

use crate::error::Result;
use crate::ignore::IgnoreSet;
use crate::sync::scan::Indexed;
use crate::transport::Transport;

/// Tolerance (ms) for the size+mtime fast-path comparison, absorbing cross-filesystem mtime
/// granularity (FAT/NFS).
const MTIME_TOLERANCE_MS: i64 = 2000;

#[derive(Debug, Default)]
pub struct SyncPlan {
    /// Receiver paths whose type changed (file↔dir); removed before anything else.
    pub retype: Vec<PathBuf>,
    /// Dirs missing on the receiver, shallow→deep.
    pub create_dirs: Vec<PathBuf>,
    /// New or changed files.
    pub transfer: Vec<TransferItem>,
    /// Extraneous files on the receiver (only if opts.delete and not ignored).
    pub delete: Vec<PathBuf>,
    /// Sum of transfer-item sender sizes (logical bytes).
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub rel: PathBuf,
    pub sender_len: u64,
    pub sender_mtime: i64,
    pub sender_mode: u32,
    pub basis_exists: bool,
}

/// Build the plan. `--checksum` triggers per-file hashing through the transports.
pub async fn build(
    sender: &Indexed,
    receiver: &Indexed,
    sender_t: &dyn Transport,
    receiver_t: &dyn Transport,
    ignore: &IgnoreSet,
    delete: bool,
    checksum: bool,
) -> Result<SyncPlan> {
    let mut plan = SyncPlan::default();

    // Directories: create those present on sender, missing (or wrong type) on receiver.
    let mut create: Vec<PathBuf> = Vec::new();
    for rel in sender.dirs.keys() {
        let on_recv_dir = receiver.dirs.contains_key(rel);
        let on_recv_file = receiver.files.contains_key(rel);
        if on_recv_file {
            plan.retype.push(rel.clone()); // file where a dir should be
        }
        if !on_recv_dir {
            create.push(rel.clone());
        }
    }
    // Shallow→deep so parents exist before children.
    create.sort_by_key(|p| p.components().count());
    plan.create_dirs = create;

    // Files.
    for (rel, se) in &sender.files {
        // A dir exists on the receiver where the sender has a file → remove it first.
        if receiver.dirs.contains_key(rel) {
            plan.retype.push(rel.clone());
        }
        let basis_exists = receiver.files.contains_key(rel) && !receiver.dirs.contains_key(rel);
        let changed = match receiver.files.get(rel) {
            None => true,
            Some(re) => {
                if checksum {
                    let sh = sender_t.hash(rel).await?;
                    let rh = receiver_t.hash(rel).await?;
                    sh != rh
                } else {
                    let size_eq = se.len == re.len;
                    let mtime_close = (se.mtime - re.mtime).abs() <= MTIME_TOLERANCE_MS;
                    !(size_eq && mtime_close)
                }
            }
        };
        if changed {
            plan.transfer.push(TransferItem {
                rel: rel.clone(),
                sender_len: se.len,
                sender_mtime: se.mtime,
                sender_mode: se.mode,
                basis_exists,
            });
            plan.total_bytes += se.len;
        }
    }

    // Deletions: receiver files not on sender, never ignored.
    if delete {
        for rel in receiver.files.keys() {
            if sender.files.contains_key(rel) {
                continue;
            }
            // A sender dir now occupies this path → handled via retype, not delete.
            if sender.dirs.contains_key(rel) {
                continue;
            }
            if ignore.is_ignored(rel, false) {
                continue;
            }
            plan.delete.push(rel.clone());
        }
    }

    plan.retype.sort();
    plan.retype.dedup();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::scan::index;
    use crate::transport::local::LocalTransport;
    use crate::transport::{EntryKind, FileEntry};

    fn fe(name: &str, len: u64, mtime: i64, kind: EntryKind) -> FileEntry {
        FileEntry {
            rel_path: name.into(),
            len,
            mtime,
            kind,
            mode: 0o644,
        }
    }

    async fn run_plan(
        sender: Vec<FileEntry>,
        receiver: Vec<FileEntry>,
        delete: bool,
        ignore_patterns: &str,
    ) -> SyncPlan {
        let s = index(sender);
        let r = index(receiver);
        let st = LocalTransport::new(".");
        let rt = LocalTransport::new(".");
        let set = IgnoreSet::from_patterns(std::path::Path::new("."), ignore_patterns).unwrap();
        build(&s, &r, &st, &rt, &set, delete, false).await.unwrap()
    }

    #[tokio::test]
    async fn equal_size_mtime_unchanged() {
        let plan = run_plan(
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            true,
            "",
        )
        .await;
        assert!(plan.transfer.is_empty());
    }

    #[tokio::test]
    async fn differing_size_transfers() {
        let plan = run_plan(
            vec![fe("a.txt", 11, 1000, EntryKind::File)],
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            true,
            "",
        )
        .await;
        assert_eq!(plan.transfer.len(), 1);
        assert!(plan.transfer[0].basis_exists);
    }

    #[tokio::test]
    async fn mtime_within_tolerance_unchanged() {
        let plan = run_plan(
            vec![fe("a.txt", 10, 1500, EntryKind::File)],
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            true,
            "",
        )
        .await;
        assert!(plan.transfer.is_empty());
    }

    #[tokio::test]
    async fn sender_only_is_new() {
        let plan = run_plan(
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            vec![],
            true,
            "",
        )
        .await;
        assert_eq!(plan.transfer.len(), 1);
        assert!(!plan.transfer[0].basis_exists);
    }

    #[tokio::test]
    async fn receiver_only_is_deleted_unless_ignored() {
        let plan = run_plan(
            vec![],
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            true,
            "",
        )
        .await;
        assert_eq!(plan.delete, vec![PathBuf::from("a.txt")]);

        let plan = run_plan(
            vec![],
            vec![fe("a.log", 10, 1000, EntryKind::File)],
            true,
            "*.log\n",
        )
        .await;
        assert!(plan.delete.is_empty());
    }

    #[tokio::test]
    async fn no_delete_keeps_extraneous() {
        let plan = run_plan(
            vec![],
            vec![fe("a.txt", 10, 1000, EntryKind::File)],
            false,
            "",
        )
        .await;
        assert!(plan.delete.is_empty());
    }
}
