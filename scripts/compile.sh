#!/bin/bash
set -euo pipefail

VERSION="${1:-0.1.0}"
OUT_DIR="npm/bin"

mkdir -p "$OUT_DIR"

echo "Compiling khive CLI v${VERSION}..."

# macOS ARM64 (Apple Silicon)
echo "  -> darwin-arm64"
deno compile \
  --allow-read --allow-write --allow-run --allow-env \
  --target aarch64-apple-darwin \
  --output "${OUT_DIR}/khive-darwin-arm64" \
  cli/main.ts

# macOS x64 (Intel)
echo "  -> darwin-x64"
deno compile \
  --allow-read --allow-write --allow-run --allow-env \
  --target x86_64-apple-darwin \
  --output "${OUT_DIR}/khive-darwin-x64" \
  cli/main.ts

# Linux x64
echo "  -> linux-x64"
deno compile \
  --allow-read --allow-write --allow-run --allow-env \
  --target x86_64-unknown-linux-gnu \
  --output "${OUT_DIR}/khive-linux-x64" \
  cli/main.ts

# Linux ARM64
echo "  -> linux-arm64"
deno compile \
  --allow-read --allow-write --allow-run --allow-env \
  --target aarch64-unknown-linux-gnu \
  --output "${OUT_DIR}/khive-linux-arm64" \
  cli/main.ts

# Windows x64
echo "  -> win32-x64"
deno compile \
  --allow-read --allow-write --allow-run --allow-env \
  --target x86_64-pc-windows-msvc \
  --output "${OUT_DIR}/khive-win32-x64" \
  cli/main.ts

echo "Done. Binaries in ${OUT_DIR}/"
ls -lh "${OUT_DIR}/"
