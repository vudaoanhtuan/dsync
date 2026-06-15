//! Stage 3 — concurrent per-file delta transfer with BLAKE3 integrity verification and a
//! whole-file retry/fallback. See specs/sync-engine.md, Stage 3.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::delta::block_size_for;
use crate::error::DsyncError;
use crate::progress::Progress;
use crate::sync::plan::TransferItem;
use crate::transport::Transport;

pub struct TransferOutcome {
    pub transferred: usize,
    pub failures: Vec<(PathBuf, String)>,
}

/// Transfer all items concurrently (bounded by `threads`). Errors on individual files are
/// collected; the caller fails the run if any occurred.
pub async fn run(
    sender: Arc<dyn Transport>,
    receiver: Arc<dyn Transport>,
    items: Vec<TransferItem>,
    threads: usize,
    delta_size_cap: u64,
    progress: Arc<Progress>,
) -> TransferOutcome {
    let sem = Arc::new(Semaphore::new(threads.max(1)));
    let mut handles = Vec::with_capacity(items.len());

    for item in items {
        let sender = sender.clone();
        let receiver = receiver.clone();
        let sem = sem.clone();
        let progress = progress.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore");
            let bar = progress.file_start(&item.rel, item.sender_len);
            let res = transfer_one(&*sender, &*receiver, &item, delta_size_cap).await;
            bar.inc(item.sender_len);
            (item.rel.clone(), res)
        }));
    }

    let mut outcome = TransferOutcome {
        transferred: 0,
        failures: Vec::new(),
    };
    for h in handles {
        match h.await {
            Ok((_, Ok(()))) => outcome.transferred += 1,
            Ok((rel, Err(e))) => outcome.failures.push((rel, e.to_string())),
            Err(e) => outcome
                .failures
                .push((PathBuf::from("<unknown>"), format!("task panicked: {e}"))),
        }
    }
    outcome
}

async fn transfer_one(
    sender: &dyn Transport,
    receiver: &dyn Transport,
    item: &TransferItem,
    delta_size_cap: u64,
) -> crate::error::Result<()> {
    let block_size = block_size_for(item.sender_len) as u64;
    let use_whole_file =
        !item.basis_exists || item.sender_len <= block_size || item.sender_len > delta_size_cap;

    if use_whole_file {
        return whole_file(sender, receiver, item).await;
    }

    // Delta path; on any algorithmic error, fall back to the whole-file path.
    match delta_path(sender, receiver, item).await {
        Ok(()) => Ok(()),
        Err(DsyncError::Integrity(_)) => {
            // Integrity mismatch → one retry via whole-file; propagate if it still fails.
            whole_file(sender, receiver, item).await
        }
        Err(_) => whole_file(sender, receiver, item).await,
    }
}

async fn delta_path(
    sender: &dyn Transport,
    receiver: &dyn Transport,
    item: &TransferItem,
) -> crate::error::Result<()> {
    let rel = &item.rel;
    let sig = receiver
        .signature(rel)
        .await?
        .ok_or_else(|| DsyncError::Other(format!("basis vanished for {}", rel.display())))?;
    let delta = sender.diff(rel, &sig).await?;
    let recv_digest = receiver
        .patch(rel, &delta, item.sender_mtime, item.sender_mode)
        .await?;
    let send_digest = sender.hash(rel).await?;
    if recv_digest != send_digest {
        return Err(DsyncError::Integrity(rel.display().to_string()));
    }
    Ok(())
}

async fn whole_file(
    sender: &dyn Transport,
    receiver: &dyn Transport,
    item: &TransferItem,
) -> crate::error::Result<()> {
    let rel = &item.rel;
    let data = sender.read_file(rel).await?;
    let send_digest = *blake3::hash(&data).as_bytes();
    let recv_digest = receiver
        .write_file(rel, &data, item.sender_mtime, item.sender_mode)
        .await?;
    if recv_digest != send_digest {
        return Err(DsyncError::Integrity(rel.display().to_string()));
    }
    Ok(())
}
