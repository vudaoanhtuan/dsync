# Sync Engine

## Goal
Orchestrate a full `push` or `pull`: scan both sides, decide what changed, transfer only
changed data via the delta algorithm, delete extraneous files, verify integrity, and report
progress — ending with the source and the selected remote identical (subject to ignore rules).
Implemented in `src/sync/`.

## Entry point
```rust
pub struct SyncOptions {
    pub direction: SyncDirection,   // Push | Pull
    pub dry_run: bool,
    pub threads: usize,             // resolved (0 already mapped to num_cpus)
    pub compress: bool,
    pub checksum: bool,             // force full-hash diffing
    pub delete: bool,               // remove extraneous on receiver
    pub quiet: bool,
}

// `remote` is the already-resolved target (CLI mapped name → entry → Remote::parse).
pub async fn run(cfg: &Config, remote: &Remote, opts: &SyncOptions) -> Result<SyncSummary>;
```

## Direction → roles
- The CLI resolves the selected remote name → its `config.remote` entry → `Remote::parse`
  before calling `run`, so the engine receives an already-resolved `Remote` (see config.md,
  Remote selection).
- Build a `src_transport` (always `LocalTransport` rooted at the source = cwd's `.dsync` root)
  and a `dst_transport` (`LocalTransport` or `SshTransport` from the selected `Remote`).
- `Push`: **sender** = src, **receiver** = dst.
- `Pull`: **sender** = dst, **receiver** = src.
- The rest of the engine refers only to `sender`/`receiver`, so the two commands share one code
  path.

## Stage 1 — Scan (`src/sync/scan.rs`)
- Build the source-resolved `IgnoreSet` once (spec 3). Scan the **sender** with `Some(&ignore)`
  and the **receiver** with `None` (scan everything) — the receiver must report every path so
  `--delete` can find extraneous files, and ignore decisions are driven authoritatively by the
  source (spec 3). The engine still never deletes a path that the source-side `IgnoreSet` marks
  ignored.
- Each returns `Vec<FileEntry>` (spec 6). Index both into `HashMap<PathBuf, FileEntry>` keyed
  by `rel_path`.
- Symlink entries (`EntryKind::Symlink`) are dropped from both maps and counted for the
  "N symlinks skipped" warning; they never enter the plan.
- Directories are tracked so they can be created on the receiver before their files and
  removed after their contents when emptied.

## Stage 2 — Plan (`src/sync/plan.rs`)
Compare the two maps into three sets of relative paths:
```rust
pub struct SyncPlan {
    pub create_dirs: Vec<PathBuf>,           // dirs missing on receiver
    pub transfer: Vec<TransferItem>,         // new or changed files
    pub delete: Vec<PathBuf>,                // extraneous on receiver (if opts.delete)
    pub total_bytes: u64,                    // sum of transfer item sender-sizes
}
pub struct TransferItem { pub rel: PathBuf, pub sender_len: u64, pub basis_exists: bool }
```

**Change-detection rule** for a file present on both sides:
1. Fast path (default): considered **unchanged** iff `size` is equal **and** `mtime` (unix ms,
   spec 6) is within a **±2000 ms** tolerance to absorb cross-filesystem granularity (FAT/NFS).
   Otherwise → transfer.
2. `--checksum`: ignore size/mtime; compare `sender.hash(rel)` vs `receiver.hash(rel)` and
   transfer only on mismatch. Slower but exact.
- Present on sender only → transfer (new file, `basis_exists = false`).
- Present on receiver only → `delete` (if `opts.delete`), but **never** delete ignored paths.
- `create_dirs` = directories on sender absent on receiver.

## Stage 3 — Transfer (`src/sync/transfer.rs`)
Concurrent, bounded by `opts.threads`. For each `TransferItem`:
1. Create parent dirs on the receiver (`mkdir_all`) — done once up front from `create_dirs`,
   ordered shallow→deep.
2. If `!basis_exists`, `sender_len <= block_size`, or `sender_len > delta_size_cap`
   (spec 5) → **whole-file fast path**:
   `data = sender.read_file(rel)`; `digest = receiver.write_file(rel, &data, mtime, mode)`.
   The sender's blake3 for the integrity check is computed in-process from `data` (no second
   `sender.hash(rel)` read).
3. Else **delta path**:
   - `sig = receiver.signature(rel)` (basis side computes signature).
   - `delta = sender.diff(rel, &sig)` (sender computes delta against its new file).
   - `digest = receiver.patch(rel, delta, mtime, mode)` (receiver reconstructs, sets metadata,
     returns blake3).
4. **Integrity check:** compare `digest` (receiver's blake3 of the result) against the sender's
   blake3 of the source (`sender.hash(rel)` on the delta path; the in-process digest from step 2
   on the whole-file path). Mismatch → `DsyncError::Integrity(rel)`; the engine retries the file
   once via the whole-file fast path (`write_file`), and if it still fails, aborts.
5. `patch`/`write_file` already applied `mtime`/`mode` atomically with the write (spec 6), so no
   separate metadata step is needed. (`mtime` is unix milliseconds — spec 6.)
6. Update the progress UI (spec 8) — overall counters + this file's per-file bar.

Concurrency model:
- Use a bounded task pool (`tokio` semaphore with `opts.threads` permits, or a `rayon` pool for
  the local-only case). Cap in-flight memory by streaming reads where the transport allows and
  by limiting concurrent whole-file buffers.
- Order is not significant for files; directory creation precedes file transfer; deletions run
  last.

## Stage 4 — Delete (`src/sync/mod.rs`)
- If `opts.delete`, remove `delete` files first, then prune now-empty directories
  deepest-first. Never remove ignored paths (they were excluded from both scans).
- Skipped entirely on `--dry-run`.

## Dry-run
- Compute the full `SyncPlan`, print it (counts + per-path list under `-v`), perform **no**
  writes, deletes, or `mkdir`. Return a summary with `bytes_transferred = 0`.

## Summary
```rust
pub struct SyncSummary {
    pub files_transferred: usize,
    pub files_deleted: usize,
    pub files_unchanged: usize,   // present+identical on both sides (for the summary line)
    pub symlinks_skipped: usize,  // v1 non-goal; surfaced as a warning
    pub bytes_transferred: u64,   // actual on-wire bytes (post-delta, post-compression)
    pub total_bytes: u64,         // logical size of changed files
    pub elapsed: Duration,
    pub avg_speed_bps: f64,
}
```
`bytes_transferred` is measured at the transport boundary: each transport method reports the
on-wire byte count of the frame(s) it sent (post-zstd for SSH; for local→local it is the delta
or whole-file byte length). The engine sums these so "bytes saved by delta+zstd" is
`total_bytes - bytes_transferred`.
Rendered by the progress UI (spec 8): total time, avg speed, files/bytes moved, bytes saved by
delta+compression.

## End-state guarantee
After a non-dry-run completes successfully, for every non-ignored path: it exists on both
sides with identical content (blake3-verified for transferred files), and no extraneous
non-ignored file remains on the receiver (when `--delete`). This is the property the spec
promises and tests must assert (spec 9).

## Edge cases
- Receiver remote does not exist yet (first push) → created.
- File changes type (file↔dir) between scans → remove then recreate.
- mtime granularity differences across filesystems → tolerance in the fast-path comparison;
  `--checksum` is the escape hatch.
- Zero changed files → fast no-op with a "already in sync" summary.
- Transfer error on one file → record, continue others, fail the run at the end with a
  non-zero exit and a list of failed paths (do not silently skip).

## Dependencies
- [transport.md](transport.md), [delta-algorithm.md](delta-algorithm.md),
  [ignore.md](ignore.md), [config.md](config.md), [progress-ui.md](progress-ui.md).

## Acceptance criteria
- After `push` then `pull` (and vice versa) on a populated tree, both sides are byte-identical
  for non-ignored files (integration test, spec 9).
- Unchanged files are not transferred (assert via progress counters / a transfer hook).
- `--no-delete` leaves extraneous receiver files; `--delete` removes them; ignored files are
  never deleted.
- Integrity mismatch triggers one retry then a hard error.
- `--dry-run` performs zero writes.
