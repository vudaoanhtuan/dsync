# dsync — Implementation Specs

`dsync` is an rsync-inspired CLI written in **Rust** that synchronizes two directories. The
sync source is always the current working directory; the **remotes** (sync targets) are
configured via `dsync init` (which seeds one named `default`) and managed with
`dsync remote add/remove/list`. Each remote may be a **remote path over SSH**
(`user@host:/path`) — the common case — or a **local path**. After a `push` or `pull`, source
and the selected remote contain identical contents (subject to the ignore rules).

## Confirmed design decisions
- **SSH transport:** pure-Rust `russh` + `russh-sftp` (no C dependencies → clean static
  binary).
- **Diff algorithm:** full **rsync rolling-checksum block delta** — only changed byte ranges
  are transferred.
- **Remote model:** **remote agent** — the same binary, invoked as `dsync --server`, runs on
  the remote host over SSH to do server-side scanning, hashing, and patching. The binary must
  be deployed on the remote.

## Spec index (recommended read / build order)

| # | Spec | What it covers |
|---|------|----------------|
| 1 | [architecture.md](architecture.md) | Crate layout, module tree, dependencies, core types, `Transport` boundary, data flow. |
| 2 | [config.md](config.md) | `.dsync/` layout, `init` behavior, `config.yaml` schema, named remotes, remote-string parsing. |
| 3 | [ignore.md](ignore.md) | config-only gitignore-syntax ignore engine (no repo `.gitignore` discovery), precedence rules, `ignore add` / `ignore update` / `ignore remove`. |
| 4 | [cli.md](cli.md) | Command surface (`init`, `push`, `pull`, `remote`, `ignore`, `--server`), flags, help, exit codes. |
| 5 | [delta-algorithm.md](delta-algorithm.md) | Rolling-checksum signatures, delta generation, patch application. |
| 6 | [transport.md](transport.md) | `Transport` trait, local impl, SSH impl, remote-agent wire protocol, compression. |
| 7 | [sync-engine.md](sync-engine.md) | Scan → diff → plan → transfer → verify orchestration, concurrency, deletion. |
| 8 | [progress-ui.md](progress-ui.md) | `indicatif` overall + per-file progress bars and final summary. |
| 9 | [testing-and-build.md](testing-and-build.md) | Unit/integration test plan, static build scripts for macOS & Linux. |

**Build order for an implementing agent:** start at the leaves and work up — `config` and
`ignore` first (no dependencies), then `delta-algorithm` (pure, self-contained), then
`transport`, then `sync-engine` (ties them together), then `cli` and `progress-ui`, then
tests and build scripts.

## Shared conventions
- **Crate type:** a single binary crate named `dsync`. Library logic lives in modules under
  `src/` and is exercised by unit tests; `src/main.rs` is a thin entry point.
- **Edition / MSRV:** Rust 2021 edition, MSRV 1.74+.
- **Error handling:** one crate-level error enum `DsyncError` (via `thiserror`) and
  `type Result<T> = std::result::Result<T, DsyncError>;` in `src/error.rs`. `main` uses
  `anyhow` only at the top boundary to render errors. Never `unwrap()`/`panic!` on
  recoverable conditions; reserve panics for invariant violations.
- **Logging:** use `tracing` with an env filter (`DSYNC_LOG`). User-facing output goes through
  the progress UI (spec 8), not logging.
- **Naming:** modules and files `snake_case`; types `CamelCase`; one responsibility per module.
- **Function size:** keep functions small and named for intent; extract helpers rather than
  writing large multi-purpose functions.
- **Comments:** concise but comprehensive — explain *why*, not *what the code obviously does*.
- **Async:** the SSH transport requires async; use `tokio` (multi-thread runtime). Local sync
  CPU work uses a worker pool (`rayon` or `tokio` tasks) — see spec 7.
