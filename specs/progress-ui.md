# Progress UI

## Goal
Show beautiful, informative progress while syncing and a clean summary at the end, using the
`indicatif` crate. Implemented in `src/progress.rs`.

## Layout
A `MultiProgress` with:
1. **Overall bar** (top) — tracks files: `synced / total` with remaining derivable; plus a
   secondary aggregate byte rate. Spinner + bar + counts.
2. **Per-file bars** (below) — one per in-flight transfer (up to `threads`), each showing the
   file name, bytes done / total, instantaneous speed, ETA, and elapsed time. Bars are added
   when a transfer starts and cleared (or finished) when it completes.

Because transfers are concurrent (spec 7), maintain a small pool of reusable per-file bars
sized to the worker count rather than one bar per file (which would flood the terminal).

## Templates (indicatif style strings)
Overall:
```
{spinner:.green} [{elapsed_precise}] {bar:30.cyan/blue} {pos}/{len} files  {bytes}/{total_bytes}  {binary_bytes_per_sec}
```
Per-file:
```
  {prefix:.dim} {bar:25.green/black} {bytes}/{total_bytes}  {binary_bytes_per_sec}  ETA {eta}
```
`{prefix}` = a shortened relative path (truncate long paths in the middle, keeping the
filename). Use `ProgressStyle::with_template(...)` and set the byte/throughput fields via
`ProgressBar::set_position` / `inc`.

## Driving updates
- The engine reports events to a `Progress` handle:
  ```rust
  pub struct Progress { /* MultiProgress + overall bar + per-file pool */ }
  impl Progress {
      pub fn new(total_files: u64, total_bytes: u64, quiet: bool) -> Progress;
      pub fn file_start(&self, rel: &Path, len: u64) -> FileBar;   // grabs a pooled bar
      pub fn finish_summary(&self, summary: &SyncSummary);
  }
  pub struct FileBar { /* handle to update bytes; auto-returns to pool on drop */ }
  impl FileBar { pub fn inc(&self, bytes: u64); }
  ```
- For the **delta path**, "bytes" tracked per file is the bytes of the new file processed
  (logical progress), so the bar reaches 100% even though fewer bytes crossed the wire; the
  summary separately reports actual bytes transferred and bytes saved.

## Final summary (printed after bars clear)
```
✓ Sync complete (push)
  Files:    42 transferred, 3 deleted, 1024 unchanged
  Data:     128.4 MiB changed → 11.2 MiB sent (91% saved by delta+zstd)
  Time:     3.20s    Avg: 40.1 MiB/s
```
Pull the counts (incl. `files_unchanged`) and bytes saved from `SyncSummary` (spec 7). Use
human-readable units (MiB/GiB, s/ms). If `summary.symlinks_skipped > 0`, print one extra
warning line: `⚠ N symlinks skipped (not supported in v1)`.

## Quiet & non-TTY behavior
- `--quiet` (spec 4): suppress all bars; print only the final one-line summary.
- Not a TTY (piped / CI): disable live bars automatically (`ProgressDrawTarget::hidden` or
  detect via `IsTerminal`); optionally emit periodic plain-text progress lines under `-v`.
- `--dry-run`: no progress bars; print the plan and a "would transfer N files / M bytes"
  summary.

## Edge cases
- Zero files to sync → skip bars, print "Already in sync (0 changes)".
- Very fast files → bars may complete instantly; that's fine, avoid flicker by setting a
  reasonable `enable_steady_tick`.
- Terminal resize / narrow width → templates use width-relative bar sizes; truncate paths.

## Dependencies
- `indicatif`. Driven by [sync-engine.md](sync-engine.md); reads `SyncSummary` from it.

## Acceptance criteria
- Running a sync in a TTY shows a moving overall bar plus per-file bars with speed/ETA/elapsed.
- The final summary prints total time, average speed, files/bytes, and bytes saved.
- `--quiet` shows only the summary; piping to a file produces no escape-code bar spam.
