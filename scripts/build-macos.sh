#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) macOS binary. macOS has no true static libc; the binary is
# self-contained apart from system libraries (the platform norm). See specs/testing-and-build.md.
set -euo pipefail

DIST="dist"
OUT="$DIST/dsync-macos-universal"

echo "==> Adding targets"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

echo "==> Building --release for both architectures"
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

mkdir -p "$DIST"
echo "==> Creating universal binary with lipo"
lipo -create \
  target/aarch64-apple-darwin/release/dsync \
  target/x86_64-apple-darwin/release/dsync \
  -output "$OUT"

echo "==> Verifying"
lipo -info "$OUT"
file "$OUT"

SIZE="$(du -h "$OUT" | cut -f1)"
echo "==> Done: $OUT ($SIZE)"
