#!/bin/sh
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/../crates"

echo "=== Forward-Deployed Crates Check ==="
# Excluded workspace crates (forward-deployed infrastructure) must still compile,
# pass clippy under -D warnings across all targets, and pass their test suite.
RUSTFLAGS="-D warnings" cargo check --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --all-targets
cargo clippy --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --all-targets -- -D warnings
cargo test --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml"

echo "=== Format Check ==="
cargo fmt --all -- --check

echo "=== SQL Lint ==="
sh "$SCRIPT_DIR/lint-sql.sh"

echo "=== No-Stub Guard ==="
sh "$SCRIPT_DIR/check-no-stubs.sh"

echo "=== Clippy ==="
cargo clippy --workspace --all-targets -- -D warnings

echo "=== Tests ==="
cargo test --workspace

echo "=== No-Default-Features Check ==="
cargo check --workspace --no-default-features

echo "=== Build (release) ==="
cargo build --workspace --release

echo "=== Contract Tests ==="
python3 "$SCRIPT_DIR/../tests/contract_test.py"

echo "=== Deno Tests ==="
(cd "$SCRIPT_DIR/../cli" && deno test --allow-all .)

echo "=== Smoke Test ==="
python3 "$SCRIPT_DIR/../tests/smoke_test.py"

echo "=== CI Passed ==="
