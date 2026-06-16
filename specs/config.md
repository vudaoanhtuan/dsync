# Config & Initialization

## Goal
Define the `.dsync/` directory, the `dsync init` command's behavior, the `config.yaml`
schema, the **named remotes** model (`dsync remote add/remove/list`), and how a remote path
string is parsed into a structured target. Implemented in `src/config.rs`.

## `.dsync/` layout
Created in the **source directory** (the cwd where the user runs `dsync init`):

```
.dsync/
├── .gitignore        # contains exactly the line: *
└── config.yaml       # the per-directory config (schema below)
```

The `.gitignore` containing `*` ensures the whole `.dsync/` folder is never committed to the
user's own git repo. `dsync` itself also always ignores `.dsync/` during sync (see spec 3).

## `dsync init <path>` behavior
1. Resolve cwd as the **source** root.
2. If `.dsync/config.yaml` already exists, abort with `DsyncError::AlreadyInitialized` —
   re-init is not supported; manage targets with `dsync remote` instead.
3. Parse `<path>` into a `Remote` (see below). Reject empty.
4. Create `.dsync/` (mode 0755), write `.dsync/.gitignore` with content `*\n`.
5. Write `.dsync/config.yaml` with a single named remote `default: <path>` and all other
   fields at defaults.
6. Validate: if the remote is **local** and resolves to a path **inside** the source root (or
   vice versa), abort with `DsyncError::Config` — a directory cannot sync into itself.
7. Print the resolved config path and the `default` remote.

## `config.yaml` schema
```yaml
# Required. One or more named remotes (sync targets). Each value is a local path or a
# remote SSH path (user@host:/path) — see parsing below. `dsync init` seeds one named
# `default`; add more with `dsync remote add <name> <path>`.
remote:
  default: /backup/myproject
  staging: user@server.com:/srv/app

# Optional. Multiline gitignore-syntax patterns. Default: empty.
ignore: |
  *.log
  *.tmp
  node_modules/
  dist/
  !dist/keep.txt

# Optional. zstd compression of transferred payloads. Default: true.
compression: true

# Optional. zstd compression level 1..=19. Default: 3.
compression_level: 3

# Optional. Worker threads for hashing/transfer. Default: 0 = number of CPU cores.
threads: 0

# Optional. Max file size (bytes) eligible for the delta path; larger files use the
# whole-file fast path (spec 5). Default: 536870912 (512 MiB).
delta_size_cap: 536870912
```

### Rust types
```rust
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Config {
    // name -> raw path string; each value parsed via Remote::parse. Must be non-empty.
    // BTreeMap keeps a stable, sorted order for listing and serialization.
    pub remote: BTreeMap<String, String>,
    #[serde(default)]
    pub ignore: String,                // multiline patterns, "" if absent
    #[serde(default = "default_true")]
    pub compression: bool,
    #[serde(default = "default_level")]
    pub compression_level: i32,        // 1..=19
    #[serde(default)]
    pub threads: usize,                // 0 => num_cpus
    #[serde(default = "default_delta_cap")]
    pub delta_size_cap: u64,           // bytes; files above this skip the delta path
}
```

CLI flags (`--threads`, `--no-compress`, `--checksum`, etc.) **override** config values at
runtime without mutating the file.

## Remote parsing
```rust
pub enum Remote {
    Local { path: PathBuf },
    Ssh { user: Option<String>, host: String, port: u16, path: PathBuf },
}
impl Remote {
    pub fn parse(s: &str) -> Result<Remote>;
}
```

Parsing rules (checked in order):
1. If `s` matches `[user@]host:path` **and** the part before the first `:` is not an existing
   local path and not a Windows drive letter → `Ssh`. Examples:
   - `myuser@server.com:/srv/app` → user=myuser, host=server.com, port=22, path=/srv/app
   - `server.com:relative/path` → user=None (use ssh config / current user), path relative to
     the remote login home.
   - `myvm:/data/test` → `Ssh { user: None, host: "myvm", port: 22, path: /data/test }`, where
     `host` may be an `~/.ssh/config` `Host` **alias**.
   - `[2001:db8::1]:/path` → bracketed IPv6 host.
2. Custom SSH port via `ssh://user@host:2222/path` URL form, or rely on `~/.ssh/config`.
3. Otherwise → `Local { path }`. Relative local paths resolve against cwd at sync time.

**`Remote::parse` does not touch the filesystem.** It never reads `~/.ssh/config`, so the parsed
`host` may be a literal hostname *or* an alias, and `user=None` / `port=22` are placeholders
meaning "defer to ssh_config, then to the current OS user / port 22". The alias and any
ssh_config `HostName`/`User`/`Port`/`IdentityFile` are resolved later by `SshTransport` at
connect time, where an explicit `user@` or `ssh://…:port` always wins over ssh_config (see
[transport.md](transport.md)).

## Managing remotes (`dsync remote …`)
The `remote` map is edited only through these commands (and seeded by `init`); the CLI surface
is in [cli.md](cli.md).
- **`remote add <name> <path>`**: parse `<path>` into a `Remote`; if `<name>` already exists,
  abort with `DsyncError::Config` (remove it first to change its path); apply the same self-sync
  validation as `init` for **local** paths; insert the entry and rewrite `config.yaml`.
- **`remote remove <name>`**: abort with `DsyncError::Config` if `<name>` is absent; otherwise
  remove the entry and rewrite. Removing `default` is allowed (subsequent name-less `push`/`pull`
  will error — see selection below).
- **`remote list`**: print each `name → path`, marking the `default` entry.

### Remote selection for sync
`push`/`pull` take an optional remote name:
- A **name given** → that entry; if it does not exist, abort with `DsyncError::Config`
  (`no such remote: <name>`).
- **No name** → the entry named `default`; if `default` is absent, abort with
  `DsyncError::Config` (`no default remote; pass a name or run dsync remote add <name> <path>`).

The selected entry's path is parsed via `Remote::parse` to build the destination transport
([sync-engine.md](sync-engine.md)).

## Loading config at runtime
- `Config::load()` walks up from cwd to find `.dsync/config.yaml` (so commands work from
  subdirectories of the source root, like git). The directory **containing `.dsync/`** is the
  source root.
- Missing → `DsyncError::NotInitialized`.
- Malformed YAML or `compression_level` out of `1..=19` → `DsyncError::Config` with a clear
  message.

## Edge cases
- `init` when already initialized → `DsyncError::AlreadyInitialized` (no overwrite; use
  `dsync remote` to manage targets).
- A remote's local path that does not exist yet → allowed; created on first `push`.
- Source root contains no files → valid; sync results in a matching (possibly empty) remote.
- `remote add` with a duplicate name, or `remote remove`/sync against an unknown name → config
  error (see Managing remotes / Remote selection).
- `threads` resolution happens **once**, where the effective `SyncOptions` is assembled (CLI
  flag → config → default): `0` maps to num_cpus; a value larger than `cores × 4` is clamped to
  that max with a warning. `SyncOptions.threads` is therefore always a final, resolved count.

## Dependencies
- Used by [cli.md](cli.md) (init/remote commands, runtime overrides), [ignore.md](ignore.md)
  (`ignore` field), [transport.md](transport.md) (the selected `Remote` builds the
  transport), [sync-engine.md](sync-engine.md).

## Acceptance criteria
- `dsync init /tmp/dest` creates `.dsync/.gitignore` (`*`) and `.dsync/config.yaml` with
  `remote: { default: /tmp/dest }` and defaults.
- `Remote::parse` correctly classifies the example strings above (unit tests).
- Re-running `init` in an initialized directory errors (`AlreadyInitialized`) and does not
  modify the config.
- `remote add` inserts a new named entry, rejects a duplicate name, and rejects a local path
  that sits inside the source root; `remote remove` deletes an entry and errors on an unknown
  name; `remote list` shows all entries and marks `default`.
- Sync with no name selects `default`; with `default` absent it errors; an unknown name errors.
- Loading config from a nested subdirectory finds the root `.dsync/`.
- Self-sync (a local remote inside the source) is rejected at init and at `remote add`.
