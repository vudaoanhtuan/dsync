# Architecture

## Goal
Define the overall structure of `dsync`: the crate, its module tree, third-party
dependencies, the core shared types, and how data flows through a sync. Every other spec
slots into the module boundaries defined here.

## Crate layout
A single binary crate. Suggested source tree:

```
dsync/
├── Cargo.toml
├── scripts/
│   ├── build-macos.sh        # universal binary (spec 9)
│   └── build-linux.sh        # musl static binary (spec 9)
├── src/
│   ├── main.rs               # thin entry: parse args, dispatch, render errors
│   ├── error.rs              # DsyncError + Result alias
│   ├── cli.rs                # clap definitions + command dispatch (spec 4)
│   ├── config.rs             # .dsync/, config.yaml load/save, named remotes, remote parsing (spec 2)
│   ├── ignore.rs             # ignore-rule engine + ignore add/remove (spec 3)
│   ├── delta.rs              # thin wrapper over fast_rsync: signature/diff/apply (spec 5)
│   ├── sync/                 # orchestration (spec 7)
│   │   ├── mod.rs            # SyncEngine::run(direction)
│   │   ├── scan.rs           # walk + ignore → file list
│   │   ├── plan.rs           # diff scans → add/update/delete sets
│   │   └── transfer.rs       # concurrent per-file delta transfer + verify
│   ├── transport/            # Transport trait + impls (spec 6)
│   │   ├── mod.rs            # trait Transport
│   │   ├── local.rs          # LocalTransport
│   │   ├── ssh.rs            # SshTransport (russh) + agent spawn
│   │   └── protocol.rs       # client↔server wire frames
│   ├── server.rs             # `dsync --server` agent loop (spec 6)
│   └── progress.rs           # indicatif UI (spec 8)
└── tests/
    └── roundtrip.rs          # integration tests (spec 9)
```

## Dependencies (`Cargo.toml`)
| Crate | Purpose |
|-------|---------|
| `clap` (derive) | CLI parsing, help, subcommands (spec 4). |
| `serde`, `serde_yaml` | `config.yaml` (de)serialization (spec 2). |
| `ignore` | gitignore-syntax matching for the config `ignore` patterns + directory walker (spec 3). No repo `.gitignore` discovery. |
| `fast_rsync` | rsync block-delta core: signature / diff / apply (spec 5). Pure-Rust, SIMD. |
| `memmap2` | Memory-map files so delta ops read mapped slices without full in-memory reads (spec 5). |
| `blake3` | Whole-file integrity hash + `--checksum` change detection (specs 5, 7). |
| `zstd` | Payload compression over the wire (spec 6). |
| `russh`, `russh-keys`, `russh-sftp` | Pure-Rust SSH client + sftp + key/agent auth (spec 6). |
| `tokio` (rt-multi-thread, macros, io, process) | Async runtime for SSH transport. |
| `rayon` | CPU-bound parallel hashing/delta for the local fast path (spec 7). |
| `indicatif` | Progress bars (spec 8). |
| `dialoguer` | Interactive multi-select for `ignore remove` (spec 3). |
| `thiserror` | `DsyncError` definition (this spec). |
| `anyhow` | Top-level error rendering in `main` only. |
| `tracing`, `tracing-subscriber` | Diagnostic logging. |
| `walkdir` | Directory traversal (used via `ignore` crate's walker). |
| `bincode` or `postcard` | Binary framing of wire messages (spec 6). |
| `tempfile` (dev) | Integration tests (spec 9). |

> Choose `postcard` for wire framing to keep the static build dependency-light; either is fine
> as long as both client and server agree.

## Core shared types (`src/error.rs` and module roots)

```rust
// src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum DsyncError {
    #[error("not a dsync directory: run `dsync init <path>` first")]
    NotInitialized,
    #[error("already a dsync directory (use `dsync remote` to manage targets)")]
    AlreadyInitialized,
    #[error("config error: {0}")]
    Config(String),
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("ssh error: {0}")]
    Ssh(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("integrity check failed for {0}")]
    Integrity(String),
    #[error("{0}")]
    Other(String),
}
pub type Result<T> = std::result::Result<T, DsyncError>;
```

`SyncDirection` is shared by CLI and engine:
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncDirection { Push, Pull }
```

## The `Transport` boundary
All filesystem access — local or remote — goes through the `Transport` trait (spec 6). The
sync engine is written **once** against this trait; `push` and `pull` differ only in which
side is the "source" transport and which is the "remote" transport. Both ends are
expressed as `Box<dyn Transport>` (or an enum of `LocalTransport`/`SshTransport`). This is the
single most important design seam: the rsync delta algorithm and the orchestration logic never
know whether a file is local or remote.

## Data flow (one `push`/`pull`)
```
            ┌── source Transport          ┌── remote Transport
scan(src) ──┤                   scan(dst)─┤
            └─→ FileEntry list            └─→ FileEntry list
                     │                            │
                     └──────── plan() ────────────┘   (sync/plan.rs)
                                  │
                  add / update / delete sets (relative paths)
                                  │
                         transfer() concurrently        (sync/transfer.rs)
                     per file: signature → diff → patch  (delta/*)
                                  │
                       integrity verify (blake3)
                                  │
                          progress UI updates             (progress.rs)
```

For a **local→local** sync both transports are `LocalTransport`. For **remote** sync, one
transport is `SshTransport`, which transparently forwards `scan`/`signature`/`patch` requests
to the `dsync --server` agent on the remote and performs the CPU-heavy work *on the side where
the data lives* (so block hashing happens locally to the file, not over the network).

## Acceptance criteria
- The module tree above exists and compiles; `cargo build` produces a single `dsync` binary.
- The sync engine compiles against `dyn Transport` with no `local`/`remote` branching inside
  `sync/plan.rs` or `delta/*`.
- `DsyncError` is the only error type returned by library functions; `main.rs` is the only
  place `anyhow`/process-exit-code handling appears.
