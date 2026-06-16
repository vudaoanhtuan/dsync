# Transport Layer

## Goal
Define the `Transport` abstraction that the sync engine uses for **all** filesystem access,
its two implementations (local and SSH), the `dsync --server` remote-agent protocol, and
payload compression. Implemented in `src/transport/` and `src/server.rs`.

## The `Transport` trait (`src/transport/mod.rs`)
One trait, two implementations. Async (the SSH impl needs it); `LocalTransport` runs sync work
on a blocking thread pool so the signature is uniform.

All wire-crossing types derive `serde::Serialize`/`Deserialize` so they can be
`postcard`-encoded in protocol frames (below).

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind { File, Dir, Symlink }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub rel_path: PathBuf,   // relative to the transport root
    pub len: u64,
    pub mtime: i64,          // unix milliseconds (consistent everywhere: protocol, patch/write_file, plan)
    pub kind: EntryKind,     // File | Dir | Symlink — symlinks are skipped in v1 (see edge cases)
    pub mode: u32,           // unix permission bits
}

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Recursively list entries under root. The RECEIVER is scanned with NO ignore
    /// filtering (`ignore` = None) so `--delete` can see every extraneous path; the
    /// SENDER (source) is scanned with the source-resolved ignore set. See ignore.md.
    async fn scan(&self, ignore: Option<&IgnoreSet>) -> Result<Vec<FileEntry>>;

    /// Block signature of an existing file (basis side). None if file absent.
    async fn signature(&self, rel: &Path) -> Result<Option<Signature>>;

    /// Produce a delta turning the basis described by `sig` into THIS side's current
    /// file at `rel`. Runs on the side that holds the new file (the sender) so for
    /// remote pull the delta is computed on the remote agent.
    async fn diff(&self, rel: &Path, sig: &Signature) -> Result<Delta>;

    /// Apply a delta to the existing file (or create it) atomically, then set the
    /// file's mtime/mode in the same call; returns the blake3 of the resulting file
    /// for integrity verification. Metadata is applied here (not via a separate
    /// round-trip) so SSH needs one request per transferred file.
    async fn patch(&self, rel: &Path, delta: &Delta, mtime: i64, mode: u32) -> Result<[u8; 32]>;

    /// Whole-file fast path: write `data` to `rel` atomically (temp + rename) and set
    /// its mtime/mode, returning the blake3 of the written file. Used for
    /// new/small/oversized files where the delta path is skipped (see delta.md,
    /// sync-engine.md Stage 3).
    async fn write_file(&self, rel: &Path, data: &[u8], mtime: i64, mode: u32) -> Result<[u8; 32]>;

    /// Read whole-file content for the fast path (new/small files).
    async fn read_file(&self, rel: &Path) -> Result<Vec<u8>>;

    /// blake3 of an existing file (for --checksum diffing and verification).
    async fn hash(&self, rel: &Path) -> Result<[u8; 32]>;

    async fn mkdir_all(&self, rel: &Path, mode: u32) -> Result<()>;
    async fn remove(&self, rel: &Path) -> Result<()>;          // file or empty dir
}
```

`patch` and `write_file` both write to a temp file in the destination directory, apply
mtime/mode to it, then `rename` into place (atomic replace) so an interrupted transfer never
leaves a half-written file and a successful one is already complete with its final metadata —
no separate `set_meta` step. (Metadata is folded into the write call deliberately: it keeps the
trait and the wire protocol in agreement and avoids an extra SSH round-trip per file.)

## `LocalTransport` (`src/transport/local.rs`)
- Root = a local `PathBuf`. All methods operate on `root.join(rel)`.
- `scan` uses the `ignore` crate's parallel walker, filtering via `IgnoreSet::is_ignored`.
- `signature`/`patch`/`hash` are CPU/IO bound → run on `tokio::task::spawn_blocking` or rayon.

## `SshTransport` (`src/transport/ssh.rs`)
- Built from `Remote::Ssh` (spec 2).
- **Verify host key** against `~/.ssh/known_hosts`; on unknown host, error with guidance (no
  blind trust).

### Resolving the target (`~/.ssh/config`)
The `Remote::Ssh` `host` field may be an OpenSSH `Host` alias (e.g. `myvm:/data/test` →
`host = "myvm"`). Before connecting, `SshTransport::connect` (or a helper it calls) reads
`~/.ssh/config` and resolves the alias. `Remote::parse` itself stays filesystem-free — all
ssh_config lookup happens here, at connect time (see [config.md](config.md)).

Resolved fields:
- `HostName` → the actual TCP host to connect to. If no alias matches, the token is used
  literally as the hostname.
- `User` → login user, used **only when the remote string did not specify one**.
- `Port` → connect port, used **only when not given via the `ssh://…:port` URL form**.
- `IdentityFile` → an additional private-key path, tried during key-file auth (below) **ahead
  of** the built-in `id_ed25519`/`id_rsa` defaults.

Precedence for every field: **explicit remote-string value > `~/.ssh/config` > built-in
default** (current OS user, port 22). A missing `~/.ssh/config`, or an alias with no match, is
**not an error** — fall back to the literal token and the built-in defaults.

### Authentication (in order)
`russh` client; each method is tried until one succeeds:
1. **ssh-agent** — every identity offered by the agent.
2. **Key files** — the ssh_config `IdentityFile` (if any), then `~/.ssh/id_ed25519`, then
   `~/.ssh/id_rsa`. Passphrase-protected keys that fail to load are skipped, not fatal.
3. **Interactive password prompt (new)** — only if 1–2 all fail **and** stdin is an interactive
   TTY. Read the password with hidden input (reuse `dialoguer`'s `Password`, already a
   dependency, or `rpassword`). Attempt `russh`'s `authenticate_password`; if the server offers
   only `keyboard-interactive` (common on cloud VMs, including GCP), fall back to
   `authenticate_keyboard_interactive`, feeding the same password to the prompt response(s).
   - The password is requested **once per run**: dsync opens a single authenticated `russh`
     connection and then opens the channel pool on it (see Concurrency below), so the password
     is entered once and never re-prompted per channel.
   - **Non-interactive contexts** — no TTY, `--server` mode, or `--quiet` (treated as
     non-interactive) — skip the prompt entirely and fail with the guidance error below, so
     automation/CI never hangs waiting on input.
   - The password lives only in memory for the duration of auth; it is never logged and never
     written to config.

If all methods fail, error with `DsyncError::Ssh` describing the full chain, e.g.
*"authentication failed for {user}; tried ssh-agent, key files (id_ed25519/id_rsa + ssh_config
IdentityFile), and password (a password prompt is only shown on an interactive terminal)."*

- **Spawn agent:** open an SSH `exec` channel running `dsync --server` on the remote (the
  binary must be on the remote `PATH`; document this and surface a clear error if missing).
  The channel's stdin/stdout carry the wire protocol below.
- Each `Transport` method becomes a request/response round-trip with the agent. The agent runs
  rooted at the SSH `Remote`'s path and does the heavy work (scan/hash/signature/patch)
  **locally to the remote files** — only signatures and deltas cross the network.
- `russh-sftp` is used for simple metadata ops (`mkdir_all`, `remove`, stat) where a full
  protocol message is unnecessary; the custom protocol is used for scan/signature/delta to keep
  CPU work remote.

### Concurrency — a pool of agent channels (not one shared channel)
The sync engine issues many `signature`/`diff`/`patch` calls **concurrently** (Stage 3, up to
`opts.threads`). The per-channel wire protocol is strictly synchronous (one request, then read
its response), so concurrent calls must **not** share a single channel — interleaved frames on
one stdin/stdout would corrupt the stream. Design:

- `SshTransport` opens **one** authenticated `russh` connection, then opens a **pool of N exec
  channels** (`N = opts.transfer_threads`, default 1), each running its own `dsync --server`
  process. SSH natively multiplexes independent channels over the single connection. The channel
  count is deliberately **decoupled** from `opts.threads`: each channel is a remote SSH session,
  and most sshd configs cap concurrent sessions (`MaxSessions`, default 10), so opening one
  channel per processing worker would fail on hosts with more cores than that limit.
- Each channel owns an independent synchronous request/response loop. A worker checks out a
  channel from the pool (e.g. an `async` semaphore + `Vec<AgentChannel>` guarded by a `Mutex`,
  or an `mpsc` of idle channels), issues its round-trip, and returns the channel.
- Pool size is bounded by `opts.transfer_threads`, so the SSH transport never has more in-flight
  round-trips than transfer channels. The up-to-`opts.threads` processing workers still run their
  local CPU-bound delta/hash work in parallel; they serialize only on the shared channel pool when
  they need a remote round-trip.
- The handshake (version + remote root) happens **once per channel** as it is opened. The
  one-time `scan` of the remote root runs on any single channel before Stage 3 fan-out.
- On `Shutdown`/EOF each agent process exits; closing the connection tears down all channels.

This keeps every channel's framing dead simple (no request-ID multiplexing) while preserving
end-to-end concurrency.

## Remote-agent wire protocol (`src/transport/protocol.rs`, `src/server.rs`)
Length-prefixed binary frames over the SSH exec channel. Each frame: `u32` little-endian
payload length, then a `postcard`-encoded message. Optionally zstd-compressed (see below).

```rust
enum Request {
    // Scan the remote root. When the remote is the RECEIVER, `ignore_patterns` is
    // None (scan everything). When it is the SENDER (remote pull), the client sends
    // the source-resolved ignore patterns so both ends agree (see ignore.md). There is
    // no repo-.gitignore discovery on the remote — ignore rules are config-only.
    Scan { ignore_patterns: Option<String> },
    Signature { rel: PathBuf },
    Diff { rel: PathBuf, sig: Signature },               // remote computes delta vs its file
    Patch { rel: PathBuf, delta: Delta, mtime: i64, mode: u32 },
    WriteFile { rel: PathBuf, data: Vec<u8>, mtime: i64, mode: u32 },
    ReadFile { rel: PathBuf },
    Hash { rel: PathBuf },
    Mkdir { rel: PathBuf, mode: u32 },
    Remove { rel: PathBuf },
    Shutdown,
}
enum Response {
    Scanned(Vec<FileEntry>),
    Sig(Option<Signature>),
    Diffed(Delta),
    Patched([u8; 32]),       // blake3 of patched file (also used for WriteFile)
    FileData(Vec<u8>),
    Hashed([u8; 32]),
    Ok,
    Error(String),           // maps to DsyncError::Protocol on the client
}
```

`server::run()` on the remote: read the destination root from the first handshake frame (or
from an argument passed on the exec line), then loop reading `Request`, executing it via a
local `LocalTransport` rooted at the remote path, and writing the matching `Response`. Exit on
`Shutdown` or EOF. The agent must apply the **same** `IgnoreSet` so both ends agree.

### Handshake & versioning
First exchange includes a protocol version + the remote root path. Mismatched versions →
`DsyncError::Protocol` with an "upgrade dsync on both ends" message.

## Compression
- When `compression: true`, frame payloads above a small threshold (e.g. 256 bytes) are
  zstd-compressed at `compression_level`. A one-byte frame header flags compressed vs raw so
  tiny messages skip compression overhead.
- `--no-compress` (spec 4) disables it for the run.
- Compression applies to wire frames only; local→local sync does no compression.

## Edge cases
- Remote `dsync` missing / not on PATH → clear `DsyncError::Ssh` telling the user to install
  `dsync` on the remote.
- No usable key and not an interactive terminal (no TTY, `--server`, or `--quiet`) → the
  password prompt is suppressed and auth fails fast with the guidance message, never blocking
  on input.
- `~/.ssh/config` absent, or the host token matches no `Host` alias → use the token as a literal
  hostname with built-in defaults (no error).
- ssh_config `IdentityFile` that is passphrase-protected and cannot be loaded non-interactively
  → skip that key and fall through to the next auth method (ultimately the password prompt).
- Connection drop mid-transfer → abort with an error; because `patch` renames atomically, no
  partial files remain. The next run resumes naturally (only changed files re-transfer).
- Permission denied on remote path → `Response::Error` → `DsyncError`.
- Symlinks (decided for v1): `scan` classifies them as `EntryKind::Symlink` and the engine
  **skips** them with a single aggregated warning (`N symlinks skipped`). They are never
  transferred, never used as a delta basis, and never deleted by `--delete`. Full symlink
  replication is an explicit non-goal for v1. Do **not** follow symlinked directories during
  the walk (avoids cycles and escaping the root).

## Dependencies
- [delta-algorithm.md](delta-algorithm.md) (`Signature`, `Delta`), [ignore.md](ignore.md)
  (`IgnoreSet`), [config.md](config.md) (`Remote`, compression settings).
- An `~/.ssh/config` parser (e.g. the `ssh2-config` crate) to resolve host aliases
  (HostName/User/Port/IdentityFile).
- A hidden-input prompt for the password step — reuse `dialoguer`'s `Password` (already in
  `Cargo.toml`) or add `rpassword`.
- Consumed by [sync-engine.md](sync-engine.md).

## Acceptance criteria
- `LocalTransport` and `SshTransport` both implement `Transport`; the engine compiles against
  `dyn Transport` only.
- Round-trip over a loopback SSH connection (integration/manual test) reproduces a local sync.
- Protocol frames are length-prefixed and version-checked; compression toggles via config/flag.
- `patch` is atomic (temp file + rename); an interrupted run leaves no partial files.
- A target using an `~/.ssh/config` alias (e.g. `myvm:/data/test`) connects using the alias's
  `HostName`, `User`, `Port`, and `IdentityFile`.
- An explicit `user@` or `ssh://…:port` in the remote string overrides the ssh_config `User`/`Port`.
- On an interactive terminal, a host with no usable key prompts once and authenticates via
  password — and via `keyboard-interactive` when that is the only method the server offers.
- The same key-less host in a non-TTY context fails fast with the guidance message instead of
  hanging on a prompt.
