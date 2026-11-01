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
if check_crates_ladder "$FIXTURE" __empty_ladder_sentinel__ 2>"$STDERR_CAPTURE"; then
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

echo "--- case 5: SemVer excludes have one source consumed by the publish gate ---"
SEMVER_EXCLUDES_FILE="$SCRIPT_DIR/lib/semver_excludes.txt"
PUBLISH_SCRIPT="$SCRIPT_DIR/publish.sh"
if grep -qE '^(<<<<<<<|=======|>>>>>>>)' "$PUBLISH_SCRIPT"; then
    echo "FAIL: unresolved conflict markers remain in a SemVer policy consumer" >&2
    exit 1
fi
if ! grep -qF 'done <"$SCRIPT_DIR/lib/semver_excludes.txt"' "$PUBLISH_SCRIPT"; then
    echo "FAIL: the publish gate must consume scripts/lib/semver_excludes.txt" >&2
    exit 1
fi
if grep -qE -- '--exclude[[:space:]]+khive-' "$PUBLISH_SCRIPT"; then
    echo "FAIL: a SemVer exclusion is duplicated outside the shared policy file" >&2
    exit 1
fi
while IFS= read -r crate; do
    [[ -n "$crate" ]] || continue
    if ! grep -qF "\"$crate\"" "$SCRIPT_DIR/../crates/Cargo.toml"; then
        echo "FAIL: SemVer exclusion is not a current workspace member: $crate" >&2
        exit 1
    fi
done < <(grep -v '^[[:space:]]*#' "$SEMVER_EXCLUDES_FILE" | grep -v '^[[:space:]]*$')
echo "PASS"

echo "--- case 6: local SemVer gate reports tool compatibility and evaluated work ---"
SEMVER_HELPER="$SCRIPT_DIR/lib/semver_gate.py"
if [[ ! -f "$SEMVER_HELPER" ]] ||
    ! grep -q 'semver_gate.py.*check-version' "$PUBLISH_SCRIPT" ||
    ! grep -q 'semver_gate.py.*summarize' "$PUBLISH_SCRIPT" ||
    ! grep -q 'CARGO_TERM_COLOR=never cargo semver-checks' "$PUBLISH_SCRIPT" ||
    ! grep -q 'set -o pipefail' "$PUBLISH_SCRIPT" ||
    ! grep -q 'SEMVER_STATUS=${PIPESTATUS\[0\]}' "$PUBLISH_SCRIPT"; then
    echo "FAIL: publish.sh must validate, capture, preserve, and summarize the SemVer gate" >&2
    exit 1
fi
echo "PASS"

echo "--- case 7: NO_VERIFY knob defaults to verify and refuses unknown values ---"
for value in "" 0; do
    mode=$(publish_verify_mode "$value")
    if [[ "$mode" != verify ]]; then
        echo "FAIL: NO_VERIFY='$value' must resolve to verify, got '$mode'" >&2
        exit 1
    fi
done
mode=$(unset NO_VERIFY; publish_verify_mode "${NO_VERIFY-}")
if [[ "$mode" != verify ]]; then
    echo "FAIL: an unset NO_VERIFY must resolve to verify, got '$mode'" >&2
    exit 1
fi
mode=$(publish_verify_mode 1)
if [[ "$mode" != no-verify ]]; then
    echo "FAIL: NO_VERIFY=1 must resolve to no-verify, got '$mode'" >&2
    exit 1
fi
for value in true yes 2 " 1" "1 "; do
    if publish_verify_mode "$value" >/dev/null 2>"$STDERR_CAPTURE"; then
        echo "FAIL: NO_VERIFY='$value' must be refused" >&2
        exit 1
    fi
    if ! grep -q "NO_VERIFY must be unset, 0 or 1" "$STDERR_CAPTURE"; then
        echo "FAIL: refusal of NO_VERIFY='$value' must name the accepted values" >&2
        exit 1
    fi
done
if [[ "$(grep -c -- '--no-verify' "$PUBLISH_SCRIPT" | tr -d ' ')" -lt 1 ]] ||
    [[ "$(grep -c -- 'cargo publish -p "\$crate" --no-verify' "$PUBLISH_SCRIPT" | tr -d ' ')" != 1 ]] ||
    ! grep -B1 -- 'cargo publish -p "\$crate" --no-verify' "$PUBLISH_SCRIPT" | grep -q 'VERIFY_MODE" == no-verify'; then
    echo "FAIL: publish.sh must pass --no-verify only on the branch selected by NO_VERIFY=1" >&2
    exit 1
fi
echo "PASS"

echo "--- case 8: the SemVer gate is skipped only for a clean checkout of the named commit ---"
HEAD40=0123456789abcdef0123456789abcdef01234567
OTHER40=89abcdef0123456789abcdef0123456789abcdef
expect_gate() {
    local want="$1" got
    shift
    got=$(semver_gate_mode "$@")
    if [[ "$got" != "$want" ]]; then
        echo "FAIL: semver_gate_mode $* must print $want, got '$got'" >&2
        exit 1
    fi
}
expect_gate skip "$HEAD40" "$HEAD40" clean
expect_gate run "$OTHER40" "$HEAD40" clean
expect_gate run "" "$HEAD40" clean
expect_gate run "$HEAD40" "$HEAD40" dirty
expect_gate run "${HEAD40:0:12}" "${HEAD40:0:12}" clean
expect_gate run "$(tr 'a-f' 'A-F' <<<"$HEAD40")" "$(tr 'a-f' 'A-F' <<<"$HEAD40")" clean
expect_gate run "$HEAD40" "" clean
mode=$(unset KHIVE_PUBLISH_SEMVER_CHECKED_AT; semver_gate_mode "${KHIVE_PUBLISH_SEMVER_CHECKED_AT-}" "$HEAD40" clean)
if [[ "$mode" != run ]]; then
    echo "FAIL: an unset KHIVE_PUBLISH_SEMVER_CHECKED_AT must run the gate, got '$mode'" >&2
    exit 1
fi
if ! grep -q 'semver_gate_mode "${KHIVE_PUBLISH_SEMVER_CHECKED_AT-}" "$PUBLISH_HEAD" "$PUBLISH_TREE"' "$PUBLISH_SCRIPT" ||
    [[ "$(grep -c 'cargo semver-checks check-release' "$PUBLISH_SCRIPT" | tr -d ' ')" != 1 ]] ||
    ! awk '/SEMVER_GATE_MODE" == skip/ {s=1} s && /^else$/ {e=1} e && /cargo semver-checks check-release/ {f=1} END {exit !f}' "$PUBLISH_SCRIPT"; then
    echo "FAIL: publish.sh must run cargo semver-checks only on the branch semver_gate_mode does not skip" >&2
    exit 1
fi
echo "PASS"

echo ""
echo "=== publish-guard-test: all cases passed ==="
