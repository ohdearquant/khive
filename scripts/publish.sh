#!/usr/bin/env bash
set -euo pipefail

# Publish khive crates to crates.io in dependency order.
# Each crate waits for the previous to propagate on the index.
#
# Usage:
#   ./publish.sh          # preflight (default) — see scope below
#   ./publish.sh --live   # actual publish
#
# Preflight scope (default mode):
#   `cargo package --list --allow-dirty` is run per crate, then
#   `cargo check --workspace` once. This validates, for every crate
#   in the publish chain:
#     - Cargo.toml parses, package metadata is well-formed
#     - The `include` / `exclude` patterns resolve to a non-empty file set
#       (catches missing files, accidental excludes, license drift)
#     - The whole workspace compiles against current path deps
#
#   It does NOT exercise `cargo publish --dry-run`, because that command
#   tries to resolve transitive deps against the live crates.io index.
#   For any workspace bump (e.g. 0.1.2 → 0.1.3), downstream crates fail
#   immediately on the second crate: cargo cannot find `khive-score 0.1.3`
#   in the index until it has actually been published. There is no flag
#   (`--no-verify`, `--allow-dirty`, …) that bypasses this resolution
#   step. The only path that exercises the full transitive build with
#   published deps is `--live`, one crate at a time, with the 30s index
#   wait between each.
#
# Prerequisites:
#   cargo login  (one-time crates.io token setup)

LIVE_MODE=false
if [[ "${1:-}" == "--live" ]]; then
    LIVE_MODE=true
    echo "=== LIVE PUBLISH ==="
else
    echo "=== PREFLIGHT (metadata + tarball file list + workspace check; pass --live to publish for real) ==="
fi

cd "$(dirname "$0")/../crates"

# Dependency order: each crate only depends on crates above it.
CRATES=(
    khive-types
    khive-score
    khive-vamana
    khive-fold
    khive-storage
    khive-bm25
    khive-fusion
    khive-db
    khive-hnsw
    khive-query
    khive-gate
    khive-gate-rego
    khive-runtime
    khive-request
    khive-retrieval
    khive-vcs-adapters
    khive-vcs
    # khive-merge — excluded from workspace (ADR-043 forward-deployed, ahead of khive-vcs)
    khive-pack-kg
    khive-pack-gtd
    khive-pack-brain
    khive-pack-memory
    khive-pack-comm
    khive-pack-schedule
    khive-pack-knowledge
    khive-pack-template
    khive-mcp
    kkernel
)

DELAY=5  # seconds to wait for crates.io index between publishes

for crate in "${CRATES[@]}"; do
    echo ""
    if $LIVE_MODE; then
        echo "--- Publishing $crate ---"
    else
        echo "--- Preflight $crate ---"
    fi

    # Check if this version is already on crates.io — skip if so.
    VERSION=$(cargo metadata --format-version=1 --no-deps 2>/dev/null \
        | python3 -c "import sys,json; pkgs=json.load(sys.stdin)['packages']; print(next(p['version'] for p in pkgs if p['name']=='$crate'))" 2>/dev/null || echo "0.1.0")
    if cargo search "$crate" 2>/dev/null | grep -q "^${crate} = \"${VERSION}\""; then
        echo "    $crate $VERSION already on crates.io — skipping"
        continue
    fi

    if $LIVE_MODE; then
        cargo publish -p "$crate"
        echo "    Waiting ${DELAY}s for crates.io index propagation..."
        sleep "$DELAY"
    else
        # Validate Cargo.toml + tarball file list without touching the
        # registry; see comment at top for why `cargo publish --dry-run`
        # is unusable for workspace bumps.
        cargo package -p "$crate" --list --allow-dirty >/dev/null
        echo "    $crate $VERSION packaging metadata + file list OK"
    fi
done

echo ""
if $LIVE_MODE; then
    echo "=== All ${#CRATES[@]} crates published ==="
else
    # Final workspace check covers what per-crate `cargo publish --dry-run`
    # would have covered (compile against current path deps).
    echo "--- Workspace compile check ---"
    cargo check --workspace --all-targets >/dev/null
    echo "    workspace check OK"
    echo ""
    echo "=== Preflight complete. Run './publish.sh --live' to publish for real ==="
fi
