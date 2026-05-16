#!/usr/bin/env bash
set -euo pipefail

# Publish khive crates to crates.io in dependency order.
# Each crate waits for the previous to propagate on the index.
#
# Usage:
#   ./publish.sh          # dry-run (default)
#   ./publish.sh --live   # actual publish
#
# Prerequisites:
#   cargo login  (one-time crates.io token setup)

DRY_RUN="--dry-run"
if [[ "${1:-}" == "--live" ]]; then
    DRY_RUN=""
    echo "=== LIVE PUBLISH ==="
else
    echo "=== DRY RUN (pass --live to publish for real) ==="
fi

cd "$(dirname "$0")/../crates"

# Dependency order: each crate only depends on crates above it.
CRATES=(
    khive-types
    khive-score
    khive-storage
    khive-db
    khive-query
    khive-runtime
    khive-mcp
)

DELAY=30  # seconds to wait for crates.io index between publishes

for crate in "${CRATES[@]}"; do
    echo ""
    echo "--- Publishing $crate ---"

    # Check if this version is already on crates.io — skip if so.
    VERSION=$(cargo metadata --format-version=1 --no-deps 2>/dev/null \
        | python3 -c "import sys,json; pkgs=json.load(sys.stdin)['packages']; print(next(p['version'] for p in pkgs if p['name']=='$crate'))" 2>/dev/null || echo "0.1.0")
    if cargo search "$crate" 2>/dev/null | grep -q "^${crate} = \"${VERSION}\""; then
        echo "    $crate $VERSION already on crates.io — skipping"
        continue
    fi

    cargo publish -p "$crate" $DRY_RUN

    if [[ -z "$DRY_RUN" ]]; then
        echo "    Waiting ${DELAY}s for crates.io index propagation..."
        sleep "$DELAY"
    fi
done

echo ""
if [[ -z "$DRY_RUN" ]]; then
    echo "=== All ${#CRATES[@]} crates published ==="
else
    echo "=== Dry run complete. Run './publish.sh --live' to publish for real ==="
fi
