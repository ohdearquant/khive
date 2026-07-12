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

echo "=== ADR Reference Lint ==="
sh "$SCRIPT_DIR/lint-adr-refs.sh"

echo "=== No-Stub Guard (clippy restriction lints) ==="
# AST-aware "No stubs. Ever." enforcement. clippy parses the macros, so it is
# immune to the grep failure modes (spacing like `todo !()`, brace forms like
# `unimplemented!{}`, macro names inside comments or string literals). Scoped to
# --lib --bins = shipping source only (excludes tests/benches/examples), matching
# the prior policy. khive-merge is excluded from the workspace (forward-deployed),
# so it gets its own pass to preserve coverage.
NOSTUB_LINTS="-Dclippy::todo -Dclippy::unimplemented -Dclippy::dbg_macro"
# shellcheck disable=SC2086
cargo clippy --workspace --lib --bins -- $NOSTUB_LINTS
# shellcheck disable=SC2086
cargo clippy --manifest-path "$SCRIPT_DIR/../crates/khive-merge/Cargo.toml" --lib --bins -- $NOSTUB_LINTS

echo "=== Clippy ==="
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "=== Doc Build (-D warnings) ==="
# Mirrors the "Doc build" CI job (.github/workflows/ci.yml): intra-doc link
# breakage and other rustdoc lints are a distinct gate that check/clippy/test
# do not cover.
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace

echo "=== Tests ==="
cargo test --workspace

echo "=== Channel-Email Feature Tests (channel-email feature) ==="
# `--workspace` alone never runs any of the several `#[cfg(feature =
# "channel-email")]` test modules in khive-mcp (ADR-094 channel lifecycle
# sequencing, issue #449 cursor_commit gating, bootstrap-floor regressions,
# etc.) -- the all-features clippy pass above only type-checks them. A prior
# name filter here (`channel_lifecycle`) ran only one of those modules and
# silently skipped the rest, including the daemon's durable-cursor
# regression tests. Run the whole crate under the feature, unfiltered, so
# every one of those modules fails CI on a regression.
cargo test -p khive-mcp --features channel-email

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
python3 "$SCRIPT_DIR/../tests/smoke_brain.py"
python3 "$SCRIPT_DIR/../tests/smoke_comm.py"
python3 "$SCRIPT_DIR/../tests/smoke_knowledge.py"
python3 "$SCRIPT_DIR/../tests/smoke_schedule.py"

echo "=== Vector Smoke (embed/recall path gate) ==="
# smoke_vector.py self-guards empirically: it spawns kkernel, attempts one
# memory.remember, and prints "SKIP: ..." + exits 0 when the embedder is not
# usable (model weights absent or no engine resolves).  GitHub Actions runners
# that lack the model weights are unaffected.  Set KHIVE_NO_EMBED=1 to bypass.
python3 "$SCRIPT_DIR/../tests/smoke_vector.py"

echo "=== Contract Suite (khive-contract) ==="
(cd "$SCRIPT_DIR/../tests/khive-contract" && uv run pytest -q)

echo "=== CI Passed ==="
