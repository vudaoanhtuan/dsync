# Testing & Build

## Goal
Specify the test coverage expected for the important logic and the build scripts that produce
single, standalone static binaries for macOS and Linux.

## Unit tests (per module, `#[cfg(test)]` blocks)
| Module | Tests |
|--------|-------|
| `delta` | **Round-trip invariant** `apply(basis, diff(signature(basis), new)) == new` (our `fast_rsync` wrapper) across: identical files, fully different, insert at start/middle/end, delete, single-byte flip, empty basis, empty new, file < block size. Property-style with randomized byte buffers (fixed RNG seed — deterministic). Assert identical files yield a near-empty delta, and that a large file with a few changed bytes yields a delta ≪ file size. |
| `config` | `Remote::parse` classifies local vs `user@host:/path` vs IPv6 vs `ssh://`; round-trip YAML serialize/deserialize with defaults and the `remote` map; multiple named remotes round-trip; default-selection picks `default`; missing-`default` and unknown-name selection error; `remote add` rejects a duplicate name; `init` errors when already initialized; `compression_level` range validation; load-from-subdirectory finds root `.dsync/`. |
| `ignore` | `.dsync/` always ignored (even with an empty `ignore` section); a config `ignore` pattern excludes a path and a later `!path` negation re-includes it (last match wins); repo `.gitignore` files are NOT consulted during sync; `ignore add` dedupes; `ignore update` imports patterns from given files and dedupes against the existing section; `ignore remove` removes exactly selected entries (drive the removal logic directly, not the interactive UI). |
| `sync::plan` | Change detection: equal size+mtime → unchanged; differing size or mtime → transfer; `--checksum` compares hashes; sender-only → transfer; receiver-only → delete (and never for ignored paths). |
| `transport::protocol` | Frame encode/decode round-trip for every `Request`/`Response` variant (incl. `Diff`/`Diffed`, `WriteFile`, `Scan` with `ignore_patterns: None`); compressed vs raw header flag; version-mismatch handling. |
| `cli` | Flag parsing for each subcommand; `--delete/--no-delete` precedence; error→exit-code mapping. |

## Integration tests (`tests/roundtrip.rs`, using `tempfile`)
- **Local push round-trip:** create a populated source tree + a temp local remote; `run(Push)`;
  assert every non-ignored file exists on the remote with identical bytes and no extraneous
  files.
- **Local pull round-trip:** reverse direction.
- **Idempotency:** a second `run` transfers zero files (already in sync).
- **Delete semantics:** extraneous file on receiver removed with `--delete`, kept with
  `--no-delete`; ignored extraneous file never removed.
- **Ignore end-to-end:** files matching config `ignore` patterns are absent on the remote; a
  file excluded only by a repo `.gitignore` (not imported into the config) **is** synced.
- **Delta efficiency:** modify a few bytes of a large file, re-sync, assert
  `summary.bytes_transferred` ≪ file size.
- **End-state guarantee:** after push then pull, both trees are byte-identical (walk + blake3).
- **SSH (optional / gated):** behind an env flag (e.g. `DSYNC_TEST_SSH=1`) targeting
  `localhost`, run a remote round-trip via `dsync --server`. Skipped by default in CI without
  sshd.

Run with `cargo test`. Keep tests deterministic (fixed RNG seeds; no wall-clock dependence in
assertions).

## Build scripts (`scripts/`)
Both produce a **single standalone static binary** with `--release` optimizations
(`Cargo.toml`: `[profile.release] lto = true, codegen-units = 1, strip = true, panic = "abort"`).

### `scripts/build-linux.sh`
- Target `x86_64-unknown-linux-musl` (and optionally `aarch64-unknown-linux-musl`) for a
  fully static binary (musl libc, no glibc dependency).
- Since the SSH stack is pure-Rust (`russh`, no OpenSSL/libssh2 C deps), musl static linking
  works without vendoring C libraries — this is the reason for the pure-Rust transport choice.
- Steps: `rustup target add x86_64-unknown-linux-musl` → `cargo build --release --target …` →
  emit `dist/dsync-linux-x86_64`. Verify with `ldd` reporting "not a dynamic executable".

### `scripts/build-macos.sh`
- Build both `aarch64-apple-darwin` and `x86_64-apple-darwin`, then `lipo -create` them into a
  universal binary `dist/dsync-macos-universal`. (macOS has no true static libc; the binary is
  self-contained apart from system libraries, which is the platform norm.)
- Verify with `lipo -info` and `file`.

Both scripts: fail fast (`set -euo pipefail`), print the output path and final binary size.

## Acceptance criteria
- `cargo test` passes; the delta round-trip and end-state-guarantee tests are present and
  green.
- `scripts/build-linux.sh` yields a musl static binary (`ldd` → not dynamic).
- `scripts/build-macos.sh` yields a universal binary (`lipo -info` → arm64 + x86_64).
- Release profile enables LTO + strip for a small single-file artifact.

## Dependencies
- Exercises every other spec; see [README.md](README.md) for build order.
