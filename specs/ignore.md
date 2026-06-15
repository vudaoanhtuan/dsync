# Ignore Rules

## Goal
Define how `dsync` decides which files to include in a sync, using **gitignore syntax**. Ignore
patterns live in **exactly one place — the `ignore` section of `config.yaml`** (spec 2).
`dsync` does **not** discover or honor repository `.gitignore` files during a sync. To pull
patterns *from* existing `.gitignore` files into the config, use `dsync ignore update` (below),
which is a one-time, explicit copy — after that the config is the single source of truth.
Implemented in `src/ignore.rs`.

## Behavior
- By default (empty `ignore` section), **everything** in the source directory is synced.
- The `ignore` field in `config.yaml` (spec 2) holds gitignore-syntax patterns that **exclude**
  files. Negation (`!pattern`) re-includes.
- Repository `.gitignore` files are **never** read automatically. A file is excluded only if a
  config `ignore` pattern matches it (or it is `.dsync/`).
- `.dsync/` is **always** ignored, regardless of config (hard-coded rule, highest priority).
- **Ignore rules are resolved from the source only** and applied authoritatively by the engine.
  The receiver is scanned with **no** ignore filtering (so `--delete` can see every path); the
  engine then refuses to delete any receiver path that the source-resolved `IgnoreSet` marks
  ignored. Consequence: a file ignored on the source is never created on the remote, and an
  extraneous ignored file already on the remote is left untouched (never deleted by `--delete`,
  see spec 7). For a remote *pull* the client sends the source-resolved patterns so the remote
  sender filters identically (spec 6, `Scan { ignore_patterns: Option<String> }`).

## Precedence (highest wins)
1. Hard-coded: `.dsync/` always ignored.
2. **`config.yaml` `ignore` patterns** — evaluated in file order; within this layer the **last
   matching pattern wins** (standard gitignore semantics), so a later `!pattern` re-includes a
   path excluded by an earlier pattern.

### gitignore semantics to honor
Standard gitignore matching via the `ignore`/`globset` crate:
- `*.log` matches at any depth; `/build` anchors to root; trailing `/` matches directories.
- `!pattern` negates a prior match. A negation cannot re-include a file if a parent directory
  is excluded (same as git) — document this limitation.
- Last matching pattern wins.

## Engine interface
```rust
pub struct IgnoreSet { /* compiled matcher */ }

impl IgnoreSet {
    /// Build from the config `ignore` patterns plus the always-ignore `.dsync/` rule,
    /// rooted at `src_root`. Does not read repo .gitignore files.
    pub fn build(src_root: &Path, cfg: &Config) -> Result<IgnoreSet>;

    /// True if `rel_path` (relative to root) should be excluded from sync.
    /// `is_dir` lets directory-only patterns match correctly.
    pub fn is_ignored(&self, rel_path: &Path, is_dir: bool) -> bool;
}
```

Implementation note: use the `ignore` crate's `GitignoreBuilder` to compile the config `ignore`
patterns into a single matcher, prepended with the always-ignore `.dsync/` rule. No
`.gitignore` discovery walker is configured.

## `dsync ignore add <patterns…>`
- Append each given pattern to the config's `ignore` block.
- **Merge & dedupe**: skip patterns already present (exact-string match after trimming).
- Preserve existing order; append new patterns at the end, one per line.
- Write back to `config.yaml` preserving the rest of the file's fields.
- Print which patterns were added vs already present.
- Example: `dsync ignore add "*.bak" "tmp/"`.

## `dsync ignore update <gitignore files…>`
A convenience command that **imports** patterns from one or more existing gitignore-syntax
files into the config `ignore` section (since `dsync` no longer reads `.gitignore` files during
sync, this is how a user "adopts" their repo's ignore rules).
- Read each given file (e.g. `.gitignore`, `.git/info/exclude`); error clearly if a path does
  not exist or is unreadable (`DsyncError::Io`).
- Collect their lines **in file order, files in the order given**, preserving comment (`# …`)
  and blank lines as-is.
- **Merge & dedupe** against the current `ignore` section using the same exact-string
  (trimmed) rule as `add`: patterns already present are skipped, new ones are appended at the
  end one per line.
- Write back to `config.yaml`, preserving all other fields.
- Print a summary: which patterns were imported vs already present, grouped per source file.
- Example: `dsync ignore update .gitignore .git/info/exclude`.
- This never deletes existing config patterns and never reads the files again after import — it
  is a copy, not a live link.

## `dsync ignore remove`
- Interactive. Read current `ignore` patterns (one entry per non-empty line).
- Present a **multi-select checklist** (via `dialoguer::MultiSelect`): space toggles an entry,
  Enter submits.
- Remove the selected entries, rewrite `config.yaml`, and print the removed entries.
- If `ignore` is empty → print "no ignore patterns to remove" and exit 0.
- Non-interactive / no TTY → error with guidance to edit `config.yaml` directly.

## Edge cases
- Comment lines (`# ...`) and blank lines in the `ignore` block are preserved on
  add/update/remove and ignored by the matcher.
- Duplicate add/update → reported as already present, file unchanged for those entries.
- A config negation `!x` with no corresponding exclude is harmless (no-op).
- `ignore update` with a file containing only comments/blanks → nothing new to import; report
  it and leave the config unchanged.

## Dependencies
- Reads/writes `Config` from [config.md](config.md).
- Consumed by [sync-engine.md](sync-engine.md) (scan filters paths via `is_ignored`).
- `ignore remove` UI uses `dialoguer`; see also [cli.md](cli.md).

## Acceptance criteria
- Unit test: `.dsync/` is always ignored even with an empty config `ignore` section.
- Unit test: a path matched by an `ignore` pattern is excluded; a later `!path` negation
  re-includes it (last match wins).
- Unit test: repo `.gitignore` files are **not** consulted during sync (a `.gitignore`-only
  exclusion has no effect unless imported into the config).
- `ignore add` merges without duplicating; `ignore update` imports patterns from given files,
  dedupes against the existing section, and leaves other config fields intact; `ignore remove`
  removes exactly the toggled entries.
