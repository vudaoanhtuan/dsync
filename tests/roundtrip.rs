//! Integration tests for the sync engine (local transport). See specs/testing-and-build.md.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use dsync::config::Config;
use dsync::sync::{self, SyncDirection, SyncOptions, SyncSummary};

/// Build a config rooted in `src_root` whose `default` remote points at `dst`.
fn make_config(dst: &Path, ignore: &str) -> Config {
    let mut remote = BTreeMap::new();
    remote.insert("default".to_string(), dst.display().to_string());
    Config {
        remote,
        ignore: ignore.to_string(),
        compression: true,
        compression_level: 3,
        threads: 0,
        delta_size_cap: 536_870_912,
    }
}

fn opts(direction: SyncDirection, delete: bool) -> SyncOptions {
    SyncOptions {
        direction,
        dry_run: false,
        threads: 4,
        compress: false,
        compression_level: 3,
        checksum: false,
        delete,
        quiet: true,
    }
}

async fn run(cfg: &Config, src_root: &Path, opts: &SyncOptions) -> SyncSummary {
    let remote = cfg.select_remote(None).unwrap();
    sync::run(cfg, src_root, &remote, opts).await.unwrap()
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, rel: &str) -> Vec<u8> {
    fs::read(root.join(rel)).unwrap()
}

/// Walk a tree (excluding `.dsync`) into a sorted (relpath, blake3) list.
fn fingerprint(root: &Path) -> Vec<(String, [u8; 32])> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().to_string();
            if rel.starts_with(".dsync") {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                let data = fs::read(&path).unwrap();
                out.push((rel, *blake3::hash(&data).as_bytes()));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn local_push_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write(src.path(), "a.txt", b"hello");
    write(src.path(), "sub/b.txt", b"world");
    write(src.path(), "sub/deep/c.bin", &vec![7u8; 100_000]);

    let cfg = make_config(dst.path(), "");
    let summary = run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;
    assert_eq!(summary.files_transferred, 3);

    assert_eq!(read(dst.path(), "a.txt"), b"hello");
    assert_eq!(read(dst.path(), "sub/b.txt"), b"world");
    assert_eq!(read(dst.path(), "sub/deep/c.bin"), vec![7u8; 100_000]);
    assert_eq!(fingerprint(src.path()), fingerprint(dst.path()));
}

#[tokio::test]
async fn local_pull_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // Populate the remote; pull brings it into src.
    write(dst.path(), "x.txt", b"remote-content");
    write(dst.path(), "d/y.txt", b"nested");

    let cfg = make_config(dst.path(), "");
    let summary = run(&cfg, src.path(), &opts(SyncDirection::Pull, true)).await;
    assert_eq!(summary.files_transferred, 2);
    assert_eq!(read(src.path(), "x.txt"), b"remote-content");
    assert_eq!(fingerprint(src.path()), fingerprint(dst.path()));
}

#[tokio::test]
async fn idempotent_second_run_transfers_nothing() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write(src.path(), "a.txt", b"hello");
    write(src.path(), "b.txt", b"again");

    let cfg = make_config(dst.path(), "");
    run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;
    let second = run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;
    assert_eq!(second.files_transferred, 0);
    assert_eq!(second.files_unchanged, 2);
}

#[tokio::test]
async fn delete_semantics() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write(src.path(), "keep.txt", b"k");
    // Extraneous files already on the receiver.
    write(dst.path(), "extra.txt", b"e");
    write(dst.path(), "junk.log", b"j");

    // With --no-delete the extraneous files stay.
    let cfg = make_config(dst.path(), "*.log\n");
    run(&cfg, src.path(), &opts(SyncDirection::Push, false)).await;
    assert!(dst.path().join("extra.txt").exists());

    // With --delete, extra.txt is removed but the ignored junk.log is kept.
    run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;
    assert!(!dst.path().join("extra.txt").exists());
    assert!(dst.path().join("junk.log").exists(), "ignored file must never be deleted");
    assert!(dst.path().join("keep.txt").exists());
}

#[tokio::test]
async fn ignore_end_to_end() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write(src.path(), "app.rs", b"code");
    write(src.path(), "debug.log", b"noise");
    // Excluded only by a repo .gitignore (NOT imported) → must still sync.
    write(src.path(), ".gitignore", b"app.rs\n");

    let cfg = make_config(dst.path(), "*.log\n");
    run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;

    assert!(dst.path().join("app.rs").exists(), "repo .gitignore must not affect sync");
    assert!(!dst.path().join("debug.log").exists(), "config ignore must exclude");
}

#[tokio::test]
async fn delta_efficiency_on_large_file() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let mut data = vec![0u8; 2_000_000];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    write(src.path(), "big.bin", &data);
    let cfg = make_config(dst.path(), "");
    run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;

    // Change a few bytes and re-sync. Use --checksum so the same-size rewrite is detected
    // regardless of mtime granularity; the delta path then sends only the changed range.
    data[1_000_000] ^= 0xFF;
    data[1_000_001] ^= 0xFF;
    write(src.path(), "big.bin", &data);
    let mut o = opts(SyncDirection::Push, true);
    o.checksum = true;
    let summary = run(&cfg, src.path(), &o).await;
    assert_eq!(summary.files_transferred, 1);
    assert!(
        summary.bytes_transferred < (data.len() as u64) / 5,
        "expected delta ≪ file, sent {} of {}",
        summary.bytes_transferred,
        data.len()
    );
    assert_eq!(fingerprint(src.path()), fingerprint(dst.path()));
}

#[tokio::test]
async fn end_state_guarantee_push_then_pull() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write(src.path(), "a.txt", b"alpha");
    write(src.path(), "dir/b.txt", b"beta");
    write(src.path(), "dir/c.bin", &vec![3u8; 50_000]);

    let cfg = make_config(dst.path(), "");
    run(&cfg, src.path(), &opts(SyncDirection::Push, true)).await;

    // Now pull into a fresh source: should reproduce the same tree.
    let src2 = tempfile::tempdir().unwrap();
    let cfg2 = make_config(dst.path(), "");
    run(&cfg2, src2.path(), &opts(SyncDirection::Pull, true)).await;
    assert_eq!(fingerprint(src.path()), fingerprint(src2.path()));
}

#[tokio::test]
async fn dry_run_makes_no_changes() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write(src.path(), "a.txt", b"hello");
    let cfg = make_config(dst.path(), "");
    let mut o = opts(SyncDirection::Push, true);
    o.dry_run = true;
    let summary = sync::run(&cfg, src.path(), &cfg.select_remote(None).unwrap(), &o)
        .await
        .unwrap();
    assert_eq!(summary.bytes_transferred, 0);
    assert!(!dst.path().join("a.txt").exists(), "dry-run must not write");
}
