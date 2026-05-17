#!/bin/sh
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/../crates"

echo "=== Format Check ==="
cargo fmt --all -- --check

echo "=== Clippy ==="
cargo clippy --workspace -- -D warnings

echo "=== Tests ==="
cargo test --workspace

echo "=== No-Default-Features Check ==="
cargo check --workspace --no-default-features

echo "=== Build (release) ==="
cargo build --workspace --release

echo "=== Contract Tests ==="
python3 "$SCRIPT_DIR/../tests/contract_test.py"

echo "=== CI Passed ==="
