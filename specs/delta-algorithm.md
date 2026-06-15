# Delta Algorithm (rsync block delta via `fast_rsync`)

## Goal
When updating a file, transfer only the **changed byte ranges**, not the whole file. The
receiver computes a compact *signature* of its current (basis) file; the sender uses it to
produce a *delta*; the receiver *applies* the delta to its basis to reconstruct the new file
exactly. Implemented in `src/delta.rs` as a **thin wrapper over the [`fast_rsync`] crate** —
we do **not** hand-roll the rolling checksum, diff, or patch logic.

[`fast_rsync`]: https://github.com/dropbox/fast_rsync

## Why use `fast_rsync` instead of writing our own
- Pure Rust, SIMD-optimized port of librsync — directly serves the "as fast as rsync" goal and
  the "single static binary" goal (no C / OpenSSL linkage, like our russh choice).
- The rolling-checksum + match-finding code is exactly the tricky, well-tested part best left
  to a battle-tested library. We own only the orchestration around it.

## Why this shape (which side does what)
The side that **has the basis file** (receiver's current version) computes the signature.
The side that **has the new file** (sender) computes the delta. Only the small signature and
the (hopefully small) delta cross the network. For remote sync the `dsync --server` agent runs
whichever half lives on the remote host.

## `fast_rsync` primitives we wrap
```text
Signature::calculate(data: &[u8], &mut storage, opts) -> Signature   // build basis signature
signature.index() -> IndexedSignature                                // index for fast lookup
diff(&IndexedSignature, new_data: &[u8], &mut out: impl Write)       // produce delta bytes
apply(base: &[u8], delta: &[u8], &mut out: impl Write)               // reconstruct new file
```
- `SignatureOptions { block_size, crypto_hash_size }` — set `block_size` via the heuristic
  below; `crypto_hash_size` is fast_rsync's internal (MD4) strong-hash truncation, **not** our
  integrity hash.
- `fast_rsync` only supports librsync's **MD4** format (insecure). MD4 is used solely for block
  matching inside the algorithm; integrity is guaranteed separately by our BLAKE3 check (below).

## Our wrapper API (`src/delta.rs`)
Keep the surface the rest of the codebase already expects, so `Transport` and `sync-engine`
don't depend on `fast_rsync` types directly:

Both blobs derive `serde::Serialize`/`Deserialize` so they can be `postcard`-encoded directly
into wire frames (spec 6) without the rest of the codebase touching `fast_rsync` types.

```rust
/// Opaque signature blob (fast_rsync's serialized signature bytes).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Signature(pub Vec<u8>);

/// Opaque delta blob (fast_rsync's serialized delta bytes).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Delta(pub Vec<u8>);

/// Block-size heuristic, rsync-like. `round_pow2` = nearest power of two (ties → up),
/// so tests can assert exact sizes (e.g. len=1_000_000 → sqrt≈1000 → 1024).
pub fn block_size_for(file_len: u64) -> u32; // clamp(round_pow2(sqrt(len)), 1024, 131072)

/// Build a signature for the basis bytes.
pub fn signature(basis: &[u8]) -> Result<Signature>;

/// Produce a delta turning `basis` (described by `sig`) into `new_data`.
pub fn diff(sig: &Signature, new_data: &[u8]) -> Result<Delta>;

/// Apply a delta to `basis`, returning the reconstructed new file bytes.
pub fn apply(basis: &[u8], delta: &Delta) -> Result<Vec<u8>>;
```
Each function is a few lines delegating to `fast_rsync`, mapping its errors into `DsyncError`
and (de)serializing the opaque blobs that travel over the wire (spec 6).

## Memory model — in-memory buffers, not streaming
`fast_rsync` operates on `&[u8]` slices, not streams. Implication: basis, new file, and delta
buffers must fit in memory. Mitigations the engine applies (spec 7):
- **Memory-map** files with `memmap2` so the OS pages them in on demand rather than reading the
  whole file into a `Vec` up front (`signature`/`diff`/`apply` then read from the mapped slice).
- **Size threshold:** only take the delta path for files below a configurable cap
  (`delta_size_cap`, default 512 MiB). Above the cap, use the whole-file fast path — see below.

## Whole-file fast path
If the receiver has **no basis file** (new file), the file is tiny (`<= block_size`), or the
file exceeds `delta_size_cap`, skip signature/diff entirely and send the whole file via
`Transport::write_file` (zstd-compressed on the wire, spec 6). The sync engine decides this
before calling `diff` (spec 7, Stage 3). This path does **not** synthesize a "literal delta" —
there is no such constructor on the opaque `Delta` blob; `write_file` is the dedicated path.

## Integrity verification (our responsibility, not fast_rsync's)
Because fast_rsync uses MD4 and its docs explicitly warn that `apply` output must be verified
out-of-band, the receiver computes `blake3` of the reconstructed file and the engine compares
it against the sender's `blake3` of the source file (spec 7, Stage 3). This is the real
correctness gate.

## Correctness invariant (must hold for all inputs)
```
apply(basis, diff(signature(basis), new)) == new   // for inputs within the size cap
blake3(reconstructed) == blake3(sender_source)      // verified by the engine
```
Covered by round-trip tests (spec 9): identical files (near-empty delta), fully different,
insertions/deletions, single-byte change at start/middle/end, empty basis, empty new file,
file smaller than one block, and a large file with a few changed bytes (assert delta ≪ file).

## Edge cases
- Empty basis or empty new file → delegate to fast_rsync; verify round-trip.
- New file shorter/longer than basis → handled by the algorithm.
- File above the size cap → never reaches `diff`; engine uses whole-file path.
- fast_rsync returns an error (e.g. malformed delta from a protocol bug) → `DsyncError::Other`;
  the engine then retries the file via the whole-file fast path (spec 7).

## Dependencies
- `fast_rsync` (delta core), `memmap2` (file mapping), `blake3` (integrity — used by the
  engine, not this module). Pure module — no transport or filesystem-layout knowledge beyond
  reading mapped byte slices.
- Consumed by [transport.md](transport.md) (signatures/deltas cross the wire) and
  [sync-engine.md](sync-engine.md) (per-file transfer + BLAKE3 verification).

## Acceptance criteria
- `signature`, `diff`, `apply`, `block_size_for` implemented as thin `fast_rsync` wrappers with
  the signatures above; no hand-written rolling-checksum code.
- Round-trip invariant holds across the enumerated edge cases (tests).
- For two identical large files, the delta is near-empty (only framing overhead).
- Files above the size cap bypass delta and use the whole-file path.
- Reconstructed files are BLAKE3-verified by the engine before being accepted.
