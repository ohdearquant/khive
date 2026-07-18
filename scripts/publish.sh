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
#   cargo login                                 (one-time crates.io token setup)
#   cargo install cargo-semver-checks --locked  (SemVer release gate — see below)

LIVE_MODE=false
if [[ "${1:-}" == "--live" ]]; then
    LIVE_MODE=true
    echo "=== LIVE PUBLISH ==="
else
    echo "=== PREFLIGHT (metadata + tarball file list + workspace check; pass --live to publish for real) ==="
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/publish_guard.sh
source "$SCRIPT_DIR/lib/publish_guard.sh"

cd "$SCRIPT_DIR/../crates"

# Dependency order: each crate only depends on crates above it.
CRATES=(
    khive-types
    khive-score
    khive-quant
    khive-vamana
    khive-fold
    khive-storage
    khive-bm25
    khive-text          # dev-dep on khive-bm25 (versioned), so publish after it
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
    khive-changeset      # needs khive-types (above)
    # khive-merge — excluded from workspace (ADR-043 forward-deployed, ahead of khive-vcs)
    khive-pack-formal    # needs khive-runtime + khive-types (both above); dev-dep of khive-pack-kg, so publish first
    khive-pack-kg
    khive-pack-git       # needs khive-runtime/storage + khive-pack-kg (all above)
    khive-pack-code      # needs khive-runtime/storage + khive-pack-kg (all above)
    khive-pack-gtd
    khive-brain-core
    khive-pack-brain
    khive-pack-memory
    khive-pack-comm
    khive-pack-schedule
    khive-pack-knowledge
    khive-pack-session   # needs khive-pack-kg + khive-runtime/storage/types (all above)
    khive-pack-workspace # needs khive-pack-kg/gtd/git/session (all above)
    khive-pack-template
    khive-channel          # no khive-* deps; transport abstraction
    khive-channel-email    # needs khive-channel (above); optional dep of khive-mcp
    khive-channel-telegram # needs khive-channel (above); optional dep of khive-mcp
    khive-mcp
    kkernel
)

DELAY=10  # seconds to wait for crates.io index between publishes

# Preflight guard (#1069): fail loud if any publishable workspace member is
# missing from CRATES. Closes the gap #1068 hit — khive-channel-telegram was
# added to the workspace but never wired into the publish ladder, invisible
# until `make publish-dry` failed at the SemVer gate. Runs in preflight and
# live alike, before any real publish work.
echo ""
echo "--- Publish ladder guard (CRATES membership vs cargo metadata) ---"
METADATA_JSON=$(mktemp)
trap 'rm -f "$METADATA_JSON"' EXIT
cargo metadata --no-deps --format-version=1 >"$METADATA_JSON"
if ! check_crates_ladder "$METADATA_JSON" "${CRATES[@]}"; then
    exit 1
fi
echo "    CRATES ladder OK (${#CRATES[@]} publishable workspace members all present)"

# SemVer gate (ADR-066 §3 release-gate component, relocated from per-PR #216).
# cargo-semver-checks compares each publishable crate's public API against its
# crates.io baseline and fails if a breaking change ships under a non-breaking
# version bump. This runs at the publish boundary — where the version actually
# bumps — because mid-cycle on a fixed dev version the check is permanently red
# (which is why it is NOT a per-PR CI gate). Runs in preflight and live alike so
# `make publish-dry` validates SemVer before any real publish. The exclude list
# (crates with no crates.io baseline yet) lives in scripts/lib/semver_excludes.txt
# (#568) — also consumed by .github/workflows/release.yml's semver-gate job, so
# the two gates cannot drift out of sync with each other.
echo ""
echo "--- SemVer gate (cargo-semver-checks vs crates.io baseline) ---"
if ! command -v cargo-semver-checks >/dev/null 2>&1; then
    echo "ERROR: cargo-semver-checks is required for the publish SemVer gate." >&2
    echo "       Install it:  cargo install cargo-semver-checks --locked" >&2
    exit 1
fi
mapfile -t SEMVER_EXCLUDES < <(grep -v '^[[:space:]]*#' "$SCRIPT_DIR/lib/semver_excludes.txt" | grep -v '^[[:space:]]*$')
SEMVER_EXCLUDE_ARGS=()
for crate in "${SEMVER_EXCLUDES[@]}"; do
    SEMVER_EXCLUDE_ARGS+=(--exclude "$crate")
done
cargo semver-checks check-release --workspace "${SEMVER_EXCLUDE_ARGS[@]}"
echo "    SemVer gate OK"

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
