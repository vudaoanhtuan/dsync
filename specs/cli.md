# CLI Surface

## Goal
Define the command-line interface: subcommands, arguments, flags, help text, and exit codes.
Implemented with `clap` (derive API) in `src/cli.rs`; dispatched from `src/main.rs`.

## Top-level
```
dsync <COMMAND> [OPTIONS]

A fast, rsync-inspired directory sync tool (local and remote-over-SSH).

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

A hidden `--server` flag (not shown in help) puts the binary into remote-agent mode
(see [transport.md](transport.md), `src/server.rs`).

## Commands

### `dsync init <path>`
```
Arguments:
  <path>           Remote path for the `default` remote. Local (/path) or SSH (user@host:/path).
```
Creates `.dsync/` with a single remote named `default`. Errors if the directory is already
initialized (no `--force`; use `dsync remote` to manage targets). Behavior in
[config.md](config.md).

### `dsync push` / `dsync pull`
Identical args/flags; differ only in `SyncDirection`.
```
Arguments:
  [remote]             Named remote to sync with. Defaults to `default` if omitted;
                       errors if the name is unknown or `default` was removed.

Options:
  -n, --dry-run        List what would change without transferring anything.
  -j, --threads <N>    Processing workers for local delta/hash work
                       (overrides config; 0 = num CPUs).
  -J, --transfer-threads <N>
                       Concurrent SSH transfer channels (overrides config;
                       default 1). Each is a remote SSH session — keep ≤ the
                       remote sshd's MaxSessions (default 10).
      --no-compress    Disable zstd compression for this run.
      --checksum       Force full-content hashing for change detection
                       (ignore the size+mtime fast path).
      --delete         Delete extraneous files on the receiving side (default).
      --no-delete      Keep extraneous files on the receiving side.
  -q, --quiet          Suppress progress bars; print only the final summary.
  -v, --verbose        Per-file logging (repeat for more detail).
```
- Default is `--delete` ON so the two trees end up identical (the stated guarantee). Document
  this prominently in help, since it can remove files.
- `push`: receiving side = the selected remote. `pull`: receiving side = source (cwd).
- Behavior in [sync-engine.md](sync-engine.md).

### `dsync remote add <name> <path>` / `remote remove <name>` / `remote list`
```
add     <name> <path>   Add a named remote. Local (/path) or SSH (user@host:/path).
                        Errors if <name> already exists.
remove  <name>          Remove a named remote. Errors if <name> is unknown.
list                    List configured remotes (name → path); marks the `default`.
```
Behavior in [config.md](config.md) (Managing remotes).

### `dsync ignore add <patterns…>`
```
Arguments:
  <patterns>...    One or more gitignore-syntax patterns to add.
```

### `dsync ignore update <gitignore files…>`
```
Arguments:
  <files>...       One or more gitignore-syntax files to import patterns from
                   (e.g. .gitignore .git/info/exclude).
```
Imports the patterns from the given files into the config `ignore` section (merge + dedupe).
A one-time copy — `dsync` does not read `.gitignore` files during sync. Behavior in
[ignore.md](ignore.md).

### `dsync ignore remove`
No args; interactive multi-select. Behavior in [ignore.md](ignore.md).

### `dsync --server` (hidden)
Spawned on the remote host over SSH. Reads length-prefixed protocol frames from stdin, writes
responses to stdout. Never invoked by users directly. See [transport.md](transport.md).

## Exit codes
| Code | Meaning |
|------|---------|
| 0 | Success (including dry-run). |
| 1 | Generic error (`DsyncError::Other`). |
| 2 | Usage error (bad args) — clap default. |
| 3 | Not initialized (`DsyncError::NotInitialized`). |
| 4 | Config error (`DsyncError::Config`, `DsyncError::AlreadyInitialized`). |
| 5 | Transport/SSH error. |
| 6 | Integrity check failed. |

`main.rs` maps `DsyncError` variants to these codes. Variants without a dedicated row
(`DsyncError::Io`, `DsyncError::Other`) map to **1**.

## Dispatch sketch
```rust
#[derive(clap::Parser)]
struct Cli {
    #[arg(long, hide = true)]
    server: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    Init { path: String },
    Push(SyncArgs),
    Pull(SyncArgs),
    Remote { #[command(subcommand)] action: RemoteAction },
    Ignore { #[command(subcommand)] action: IgnoreAction },
}

#[derive(clap::Args)]
struct SyncArgs {
    remote: Option<String>,   // positional; None => the `default` remote
    #[arg(short = 'n', long)] dry_run: bool,
    #[arg(short = 'j', long)] threads: Option<usize>,
    #[arg(short = 'J', long)] transfer_threads: Option<usize>,
    #[arg(long)] no_compress: bool,
    #[arg(long)] checksum: bool,
    #[arg(long, overrides_with = "no_delete", default_value_t = true)] delete: bool,
    #[arg(long)] no_delete: bool,
    #[arg(short, long)] quiet: bool,
    #[arg(short, long, action = clap::ArgAction::Count)] verbose: u8,
}

#[derive(clap::Subcommand)]
enum RemoteAction {
    Add { name: String, path: String },
    Remove { name: String },
    List,
}

#[derive(clap::Subcommand)]
enum IgnoreAction {
    Add { patterns: Vec<String> },
    Update { files: Vec<PathBuf> },   // import patterns from gitignore-syntax files
    Remove,
}
```
`main.rs`: if `--server`, enter `server::run()`. Otherwise initialize `tracing`, dispatch the
subcommand, map errors to exit codes.

## Edge cases
- `push`/`pull` without prior `init` → exit 3 with the `dsync init <path>` hint.
- `init` in an already-initialized directory → `AlreadyInitialized`, mapped to exit 4.
- `push`/`pull` with an unknown remote name, or with no name when `default` was removed →
  config error (exit 4).
- `remote add` with a duplicate name, or `remote remove` of an unknown name → exit 4.
- `ignore add` with zero patterns, or `ignore update` with zero files → usage error (exit 2).
- `ignore update` with a file that does not exist / is unreadable → IO error (exit 1).
- Conflicting `--delete --no-delete` → `--no-delete` wins (`overrides_with`).
- `--dry-run` never mutates either side and never deletes.

## Dependencies
- [config.md](config.md), [ignore.md](ignore.md), [sync-engine.md](sync-engine.md),
  [transport.md](transport.md), [progress-ui.md](progress-ui.md).

## Acceptance criteria
- `dsync --help` and `dsync push --help` render the surfaces above; `--server` is hidden.
- Every subcommand parses its documented flags; unknown flags exit 2.
- Error variants map to the documented exit codes (unit/integration test).
