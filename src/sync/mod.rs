//! Sync orchestration: scan → plan → transfer → delete → verify, ending with source and the
//! selected remote identical (subject to ignore rules). See specs/sync-engine.md.

pub mod plan;
pub mod scan;
pub mod transfer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Config, Remote};
use crate::error::{DsyncError, Result};
use crate::ignore::IgnoreSet;
use crate::progress::Progress;
use crate::transport::{LocalTransport, SshTransport, Transport};

use self::plan::SyncPlan;
use self::scan::Indexed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncDirection {
    Push,
    Pull,
}

impl SyncDirection {
    fn label(&self) -> &'static str {
        match self {
            SyncDirection::Push => "push",
            SyncDirection::Pull => "pull",
        }
    }
}

pub struct SyncOptions {
    pub direction: SyncDirection,
    pub dry_run: bool,
    /// Already-resolved processing-worker count (0 mapped to num_cpus, then clamped — see
    /// config.md). Bounds local CPU-bound delta/hash concurrency.
    pub threads: usize,
    /// Concurrent SSH transfer channels (≥ 1). Bounds remote round-trip parallelism; kept low so
    /// it doesn't exceed the remote sshd's `MaxSessions`.
    pub transfer_threads: usize,
    pub compress: bool,
    pub compression_level: i32,
    pub checksum: bool,
    pub delete: bool,
    pub quiet: bool,
}

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub direction_label: &'static str,
    pub files_transferred: usize,
    pub files_deleted: usize,
    pub files_unchanged: usize,
    pub symlinks_skipped: usize,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub avg_speed_bps: f64,
    pub dry_run: bool,
}

/// An endpoint is either a local transport or an SSH transport. The enum lets the engine call
/// `SshTransport::scan_with_patterns` for the remote-sender (pull) case while keeping the rest of
/// the engine on `dyn Transport`.
enum Endpoint {
    Local(Arc<LocalTransport>),
    Ssh(Arc<SshTransport>),
}

impl Endpoint {
    fn transport(&self) -> Arc<dyn Transport> {
        match self {
            Endpoint::Local(t) => t.clone(),
            Endpoint::Ssh(t) => t.clone(),
        }
    }

    /// Scan as the SENDER (apply source-resolved ignore rules).
    async fn scan_sender(&self, ignore: &IgnoreSet, patterns: &str) -> Result<Vec<crate::transport::FileEntry>> {
        match self {
            Endpoint::Local(t) => t.scan(Some(ignore)).await,
            Endpoint::Ssh(t) => t.scan_with_patterns(Some(patterns.to_string())).await,
        }
    }

    /// Scan as the RECEIVER (no ignore filtering, so `--delete` sees everything).
    async fn scan_receiver(&self) -> Result<Vec<crate::transport::FileEntry>> {
        self.transport().scan(None).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.transport().shutdown().await
    }
}

/// Build the destination endpoint from the resolved `Remote`.
async fn build_dst(remote: &Remote, opts: &SyncOptions) -> Result<Endpoint> {
    match remote {
        Remote::Local { path } => {
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()
                    .map_err(|e| DsyncError::io(".", e))?
                    .join(path)
            };
            Ok(Endpoint::Local(Arc::new(LocalTransport::new(abs))))
        }
        Remote::Ssh { .. } => {
            let t = SshTransport::connect(remote, opts.transfer_threads, opts.compress, opts.compression_level, opts.quiet).await?;
            Ok(Endpoint::Ssh(Arc::new(t)))
        }
    }
}

pub async fn run(
    cfg: &Config,
    src_root: &Path,
    remote: &Remote,
    opts: &SyncOptions,
) -> Result<SyncSummary> {
    let start = Instant::now();

    let src = Endpoint::Local(Arc::new(LocalTransport::new(src_root.to_path_buf())));
    let dst = build_dst(remote, opts).await?;

    let ignore = IgnoreSet::build(src_root, cfg)?;

    // Direction → roles.
    let (sender, receiver) = match opts.direction {
        SyncDirection::Push => (&src, &dst),
        SyncDirection::Pull => (&dst, &src),
    };

    // Stage 1 — scan.
    let sender_entries = sender.scan_sender(&ignore, &cfg.ignore).await?;
    let receiver_entries = receiver.scan_receiver().await?;
    let sender_idx: Indexed = scan::index(sender_entries);
    let receiver_idx: Indexed = scan::index(receiver_entries);
    let symlinks_skipped = sender_idx.symlinks;

    let sender_t = sender.transport();
    let receiver_t = receiver.transport();

    // Stage 2 — plan.
    let plan = plan::build(
        &sender_idx,
        &receiver_idx,
        &*sender_t,
        &*receiver_t,
        &ignore,
        opts.delete,
        opts.checksum,
    )
    .await?;

    let total_sender_files = sender_idx.files.len();

    if opts.dry_run {
        let summary = dry_run_summary(opts, &plan, total_sender_files, symlinks_skipped, start);
        print_dry_run(&plan, opts.quiet);
        let _ = sender.shutdown().await;
        let _ = receiver.shutdown().await;
        return Ok(summary);
    }

    let progress = Arc::new(Progress::new(
        plan.transfer.len() as u64,
        plan.total_bytes,
        opts.quiet,
    ));

    // Pre-clean: remove receiver paths whose type changed (file↔dir) before creating/transferring.
    for rel in &plan.retype {
        receiver_t.remove(rel).await?;
    }

    // Stage 3a — create directories shallow→deep.
    for rel in &plan.create_dirs {
        let mode = sender_idx
            .dirs
            .get(rel)
            .map(|d| d.mode)
            .unwrap_or(0o755);
        receiver_t.mkdir_all(rel, mode).await?;
    }

    // Stage 3b — transfer.
    let outcome = transfer::run(
        sender_t.clone(),
        receiver_t.clone(),
        plan.transfer.clone(),
        opts.threads,
        cfg.delta_size_cap,
        progress.clone(),
    )
    .await;

    if !outcome.failures.is_empty() {
        // Surface failed paths and abort with a non-zero exit (handled by caller).
        let list = outcome
            .failures
            .iter()
            .map(|(p, e)| format!("  {}: {e}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = sender.shutdown().await;
        let _ = receiver.shutdown().await;
        return Err(DsyncError::Other(format!(
            "{} file(s) failed to transfer:\n{list}",
            outcome.failures.len()
        )));
    }

    // Stage 4 — delete extraneous files, then prune now-empty dirs deepest-first.
    let mut files_deleted = 0usize;
    if opts.delete {
        for rel in &plan.delete {
            receiver_t.remove(rel).await?;
            files_deleted += 1;
        }
        prune_dirs(&*receiver_t, &sender_idx, &receiver_idx, &ignore).await;
    }

    let _ = sender.shutdown().await;
    let _ = receiver.shutdown().await;

    let bytes_transferred = sender_t.bytes_sent() + receiver_t.bytes_sent();
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64().max(1e-9);
    let summary = SyncSummary {
        direction_label: opts.direction.label(),
        files_transferred: outcome.transferred,
        files_deleted,
        files_unchanged: total_sender_files.saturating_sub(plan.transfer.len()),
        symlinks_skipped,
        bytes_transferred,
        total_bytes: plan.total_bytes,
        elapsed,
        avg_speed_bps: bytes_transferred as f64 / secs,
        dry_run: false,
    };

    progress.finish_summary(&summary);
    Ok(summary)
}

/// Remove receiver directories that are absent on the sender, deepest-first. Best-effort:
/// non-empty dirs (e.g. holding ignored files we deliberately kept) fail silently.
async fn prune_dirs(
    receiver: &dyn Transport,
    sender_idx: &Indexed,
    receiver_idx: &Indexed,
    ignore: &IgnoreSet,
) {
    let mut to_prune: Vec<PathBuf> = receiver_idx
        .dirs
        .keys()
        .filter(|rel| !sender_idx.dirs.contains_key(*rel))
        .filter(|rel| !ignore.is_ignored(rel, true))
        .cloned()
        .collect();
    // Deepest first so children are removed before parents.
    to_prune.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for rel in to_prune {
        let _ = receiver.remove(&rel).await;
    }
}

fn dry_run_summary(
    opts: &SyncOptions,
    plan: &SyncPlan,
    total_sender_files: usize,
    symlinks_skipped: usize,
    start: Instant,
) -> SyncSummary {
    SyncSummary {
        direction_label: opts.direction.label(),
        files_transferred: plan.transfer.len(),
        files_deleted: if opts.delete { plan.delete.len() } else { 0 },
        files_unchanged: total_sender_files.saturating_sub(plan.transfer.len()),
        symlinks_skipped,
        bytes_transferred: 0,
        total_bytes: plan.total_bytes,
        elapsed: start.elapsed(),
        avg_speed_bps: 0.0,
        dry_run: true,
    }
}

fn print_dry_run(plan: &SyncPlan, quiet: bool) {
    println!(
        "dry-run: would transfer {} file(s) ({} bytes), delete {} file(s)",
        plan.transfer.len(),
        plan.total_bytes,
        plan.delete.len()
    );
    if quiet {
        return;
    }
    for item in &plan.transfer {
        println!("  transfer {}", item.rel.display());
    }
    for rel in &plan.delete {
        println!("  delete   {}", rel.display());
    }
}
