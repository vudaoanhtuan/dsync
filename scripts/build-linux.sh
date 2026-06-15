#!/usr/bin/env bash
# Build a fully static musl binary for Linux (no glibc/OpenSSL dependency — the SSH stack is
# pure-Rust). See specs/testing-and-build.md.
set -euo pipefail

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
OUT_NAME="${OUT_NAME:-dsync-linux-x86_64}"
DIST="dist"

echo "==> Adding target $TARGET"
rustup target add "$TARGET"

echo "==> Building --release for $TARGET"
cargo build --release --target "$TARGET"

mkdir -p "$DIST"
BIN="target/$TARGET/release/dsync"
cp "$BIN" "$DIST/$OUT_NAME"

echo "==> Verifying static linkage"
if command -v ldd >/dev/null 2>&1; then
  ldd "$DIST/$OUT_NAME" || true   # expected: "not a dynamic executable"
fi

SIZE="$(du -h "$DIST/$OUT_NAME" | cut -f1)"
echo "==> Done: $DIST/$OUT_NAME ($SIZE)"
