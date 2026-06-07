#!/bin/bash
set -euo pipefail

# Publish khive npm packages: platform binaries + main package.
#
# Usage:
#   ./npm-publish.sh              # publish all (platform packages first, then main)
#   ./npm-publish.sh --dry-run    # show what would be published without uploading
#
# Prerequisites:
#   - npm login (one-time auth)
#   - Platform binaries must already be in npm/kernel-{platform}/bin/
#     (placed by CI cross-compile or `make local` for the current platform)
#
# The script publishes platform packages BEFORE the main package because
# the main package lists them as optionalDependencies — npm resolves them
# at install time, so they must already exist on the registry.

DRY_RUN=false
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=true
    echo "=== DRY RUN (no actual publish) ==="
fi

cd "$(dirname "$0")/.."

VERSION=$(jq -r .version npm/package.json)
echo "Publishing khive v${VERSION} to npm..."

# Platform packages — publish each one that has binaries in bin/.
PLATFORMS=(
    kernel-darwin-arm64
    kernel-darwin-x64
    kernel-linux-x64-gnu
    kernel-linux-x64-musl
    kernel-linux-arm64
    kernel-win32-x64
)

publish_pkg() {
    local dir="$1"
    local name
    name=$(jq -r .name "$dir/package.json")
    local ver
    ver=$(jq -r .version "$dir/package.json")

    # Check if this version is already on npm — skip if so.
    if npm view "${name}@${ver}" version 2>/dev/null | grep -q "${ver}"; then
        echo "  $name@$ver already on npm — skipping"
        return 0
    fi

    # Check if bin/ has actual binaries (not just .gitkeep).
    local bin_count
    bin_count=$(find "$dir/bin" -type f -not -name ".gitkeep" 2>/dev/null | wc -l | tr -d ' ')
    if [[ "$bin_count" -eq 0 ]]; then
        echo "  $name — no binaries in bin/, skipping (CI hasn't built this platform)"
        return 0
    fi

    if $DRY_RUN; then
        echo "  [dry-run] would publish $name@$ver (${bin_count} binaries)"
    else
        echo "  Publishing $name@$ver..."
        (cd "$dir" && npm publish --access public)
        echo "  Published $name@$ver"
    fi
}

echo ""
echo "--- Platform packages ---"
for platform in "${PLATFORMS[@]}"; do
    dir="npm/${platform}"
    if [[ -d "$dir" ]]; then
        publish_pkg "$dir"
    else
        echo "  npm/${platform} — directory not found, skipping"
    fi
done

echo ""
echo "--- Main package ---"
if npm view "khive@${VERSION}" version 2>/dev/null | grep -q "${VERSION}"; then
    echo "  khive@${VERSION} already on npm — skipping"
else
    if $DRY_RUN; then
        echo "  [dry-run] would publish khive@${VERSION}"
    else
        echo "  Publishing khive@${VERSION}..."
        (cd npm && npm publish --access public)
        echo "  Published khive@${VERSION}"
    fi
fi

echo ""
echo "=== Done ==="
echo "Verify: npm view khive@${VERSION}"
