#!/bin/sh
set -e

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

echo "=== CI Passed ==="
