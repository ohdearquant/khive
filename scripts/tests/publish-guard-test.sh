#!/usr/bin/env bash
# Standalone fixture test for scripts/lib/publish_guard.sh (#1069).
# No network, no live cargo publish — run with:
#   bash scripts/tests/publish-guard-test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=../lib/publish_guard.sh
source "$SCRIPT_DIR/lib/publish_guard.sh"

FIXTURE=$(mktemp)
STDERR_CAPTURE=$(mktemp)
trap 'rm -f "$FIXTURE" "$STDERR_CAPTURE"' EXIT

cat >"$FIXTURE" <<'JSON'
{
  "packages": [
    {"name": "crate-a", "publish": null},
    {"name": "crate-b", "publish": ["crates-io"]},
    {"name": "crate-c", "publish": []}
  ]
}
JSON

echo "--- case 1: complete ladder passes (and publish=false crate-c is not required) ---"
if check_crates_ladder "$FIXTURE" crate-a crate-b; then
    echo "PASS"
else
    echo "FAIL: complete ladder [crate-a, crate-b] was rejected" >&2
    exit 1
fi

echo "--- case 2: ladder missing a publishable crate fires and names it ---"
if check_crates_ladder "$FIXTURE" crate-a 2>"$STDERR_CAPTURE"; then
    echo "FAIL: guard did not fire when crate-b was omitted" >&2
    exit 1
fi
if grep -q "crate-b" "$STDERR_CAPTURE"; then
    echo "PASS"
else
    echo "FAIL: guard fired but did not name the missing crate (crate-b)" >&2
    cat "$STDERR_CAPTURE" >&2
    exit 1
fi

echo "--- case 3: publish=false crate never appears as missing, even from an empty ladder ---"
if check_crates_ladder "$FIXTURE" 2>"$STDERR_CAPTURE"; then
    echo "FAIL: guard passed with an empty ladder (should have flagged crate-a, crate-b)" >&2
    exit 1
fi
if grep -q "crate-c" "$STDERR_CAPTURE"; then
    echo "FAIL: publish=false crate-c was falsely flagged as missing" >&2
    cat "$STDERR_CAPTURE" >&2
    exit 1
else
    echo "PASS"
fi

echo "--- case 4: malformed metadata makes the guard fail CLOSED (non-zero), not silently pass ---"
BAD_FIXTURE=$(mktemp)
printf 'this is not valid json\n' >"$BAD_FIXTURE"
if check_crates_ladder "$BAD_FIXTURE" crate-a crate-b 2>"$STDERR_CAPTURE"; then
    rm -f "$BAD_FIXTURE"
    echo "FAIL: guard passed on malformed metadata (fail-open)" >&2
    exit 1
fi
rm -f "$BAD_FIXTURE"
if grep -qiE "parser failed|could not enumerate" "$STDERR_CAPTURE"; then
    echo "PASS"
else
    echo "FAIL: guard failed on malformed metadata but without a clear parser-failure error" >&2
    cat "$STDERR_CAPTURE" >&2
    exit 1
fi

echo ""
echo "=== publish-guard-test: all cases passed ==="
