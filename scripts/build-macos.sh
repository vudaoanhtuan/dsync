#!/usr/bin/env bash
# Build macOS binaries: arm64 (Apple Silicon), x86_64 (Intel), and a universal (arm64 + x86_64)
# binary. macOS has no true static libc; the binaries are self-contained apart from system
# libraries (the platform norm). See specs/testing-and-build.md.
set -euo pipefail

DIST="dist"
OUT_ARM="$DIST/dsync-macos-arm64"
OUT_AMD="$DIST/dsync-macos-amd64"
OUT_UNIVERSAL="$DIST/dsync-macos-universal"

echo "==> Adding targets"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

echo "==> Building --release for both architectures"
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

mkdir -p "$DIST"

echo "==> Copying per-architecture binaries"
cp target/aarch64-apple-darwin/release/dsync "$OUT_ARM"
cp target/x86_64-apple-darwin/release/dsync "$OUT_AMD"

echo "==> Creating universal binary with lipo"
lipo -create \
  "$OUT_ARM" \
  "$OUT_AMD" \
  -output "$OUT_UNIVERSAL"

echo "==> Verifying"
for OUT in "$OUT_ARM" "$OUT_AMD" "$OUT_UNIVERSAL"; do
  lipo -info "$OUT"
  file "$OUT"
done

echo "==> Done:"
for OUT in "$OUT_ARM" "$OUT_AMD" "$OUT_UNIVERSAL"; do
  SIZE="$(du -h "$OUT" | cut -f1)"
  echo "    $OUT ($SIZE)"
done
