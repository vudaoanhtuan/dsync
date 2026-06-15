# dsync

A fast, rsync-inspired directory sync tool written in **Rust** — for local and remote-over-SSH targets.

`dsync` synchronizes the current working directory with one or more configured **remotes**.
A remote may be a **local path** (`/path/to/dir`) or a **remote path over SSH**
(`user@host:/path`). After a `push` or `pull`, the source and the selected remote contain
identical contents (subject to the ignore rules).

## Highlights

- **Rsync rolling-checksum block delta** — only changed byte ranges are transferred, not whole files.
- **Pure-Rust SSH transport** (`russh` + `russh-sftp`) — no C dependencies, clean static binary.
- **Remote-agent model** — the same binary, invoked as `dsync --server` over SSH, performs
  server-side scanning, hashing, and patching.
- **zstd compression** on the wire (toggleable per run).
- **gitignore-syntax ignore engine**, configured per project.
- **Live progress UI** (`indicatif`) with overall and per-file bars, plus a final summary.

## Installation

Build from source with a recent Rust toolchain (Rust 2021 edition, MSRV 1.74+):

```sh
cargo build --release
# binary at target/release/dsync
```

Static release builds for macOS and Linux are produced by the scripts under `scripts/`.

> **Note:** for SSH remotes the `dsync` binary must also be deployed on the remote host so it
> can run in remote-agent mode (`dsync --server`).

## Usage

```
dsync <COMMAND> [OPTIONS]

Commands:
  init     Initialize the current directory for syncing (seeds the `default` remote)
  push     Sync changes from this directory to a remote
  pull     Sync changes from a remote to this directory
  remote   Manage sync remotes (add / remove / list)
  ignore   Manage ignore patterns (add / update / remove)
  help     Print help for a command

Options:
  -h, --help       Print help
  -V, --version    Print version
```

### Getting started

```sh
# Initialize the current directory; sets up a remote named `default`
dsync init user@host:/srv/backup/project

# Push local changes to the default remote
dsync push

# Pull remote changes into the current directory
dsync pull

# Preview without transferring anything
dsync push --dry-run
```

### Sync options (`push` / `pull`)

| Flag | Description |
|------|-------------|
| `-n, --dry-run` | List what would change without transferring anything. |
| `-j, --threads <N>` | Worker threads (`0` = num CPUs). |
| `--no-compress` | Disable zstd compression for this run. |
| `--checksum` | Force full-content hashing (ignore the size+mtime fast path). |
| `--delete` / `--no-delete` | Delete extraneous files on the receiving side. **`--delete` is the default**, so the two trees end up identical. |
| `-q, --quiet` | Suppress progress bars; print only the final summary. |
| `-v, --verbose` | Per-file logging (repeat for more detail). |

`push` and `pull` take an optional `[remote]` name argument and default to `default`.

> ⚠️ Deletion of extraneous files on the receiving side is **on by default**. Use `--no-delete`
> to keep them, or `--dry-run` to preview first.

### Managing remotes

```sh
dsync remote add staging user@staging:/srv/app
dsync remote list
dsync remote remove staging
```

### Managing ignore patterns

```sh
dsync ignore add "target/" "*.log"     # add gitignore-syntax patterns
dsync ignore update .gitignore         # import patterns from gitignore files
dsync ignore remove "*.log"            # remove a pattern
```

## How it works

1. **Scan** the source and remote trees (respecting ignore rules).
2. **Diff** using rolling-checksum signatures to find changed byte ranges.
3. **Plan** the set of transfers and deletions.
4. **Transfer** deltas (optionally zstd-compressed) over the chosen transport.
5. **Verify** that source and remote match.

Configuration lives in a `.dsync/` directory created by `dsync init`.

## Project layout

```
src/
  cli.rs        Command-line surface (clap)
  config.rs     .dsync/ layout, named remotes, config schema
  delta.rs      Rolling-checksum signatures, delta generation, patching
  ignore.rs     gitignore-syntax ignore engine
  transport/    Transport trait, local + SSH impls, wire protocol
  sync/         Scan → diff → plan → transfer → verify orchestration
  progress.rs   indicatif progress UI
  server.rs     Remote-agent mode (dsync --server)
  error.rs      Crate-level error type
  main.rs       Thin entry point

specs/          Implementation specifications (see specs/README.md)
scripts/        Static build scripts (macOS, Linux)
tests/          Integration tests
```

## Documentation

Detailed design and implementation specs live under [`specs/`](specs/README.md).
