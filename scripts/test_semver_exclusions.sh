#!/usr/bin/env bash
# Regression tests for scripts/semver-exclusions.sh. Run with:
#   bash scripts/test_semver_exclusions.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FAILURES=0

fail() {
    echo "FAIL: $1" >&2
    FAILURES=$((FAILURES + 1))
}

source "$SCRIPT_DIR/semver-exclusions.sh"

EXPECTED_CRATES=(khive-quant khive-channel khive-channel-email khive-pack-formal khive-pack-session)

if [[ "${#SEMVER_EXCLUDED_CRATES[@]}" -ne "${#EXPECTED_CRATES[@]}" ]]; then
    fail "expected ${#EXPECTED_CRATES[@]} excluded crates, got ${#SEMVER_EXCLUDED_CRATES[@]}"
else
    for i in "${!EXPECTED_CRATES[@]}"; do
        if [[ "${SEMVER_EXCLUDED_CRATES[$i]}" != "${EXPECTED_CRATES[$i]}" ]]; then
            fail "position $i: expected ${EXPECTED_CRATES[$i]}, got ${SEMVER_EXCLUDED_CRATES[$i]}"
        fi
    done
fi

UNIQUE_COUNT=$(printf '%s\n' "${SEMVER_EXCLUDED_CRATES[@]}" | sort -u | wc -l | tr -d ' ')
if [[ "$UNIQUE_COUNT" -ne "${#SEMVER_EXCLUDED_CRATES[@]}" ]]; then
    fail "SEMVER_EXCLUDED_CRATES contains duplicates"
fi

if [[ "${#SEMVER_EXCLUDE_ARGS[@]}" -ne 10 ]]; then
    fail "expected 10 CLI tokens (5 pairs), got ${#SEMVER_EXCLUDE_ARGS[@]}"
fi
for ((i = 0; i < ${#SEMVER_EXCLUDE_ARGS[@]}; i += 2)); do
    if [[ "${SEMVER_EXCLUDE_ARGS[$i]}" != "--exclude" ]]; then
        fail "SEMVER_EXCLUDE_ARGS[$i] expected --exclude, got ${SEMVER_EXCLUDE_ARGS[$i]}"
    fi
done

EXPECTED_CSV="khive-quant,khive-channel,khive-channel-email,khive-pack-formal,khive-pack-session"
ACTUAL_CSV="$(semver_exclude_csv)"
if [[ "$ACTUAL_CSV" != "$EXPECTED_CSV" ]]; then
    fail "semver_exclude_csv() expected '$EXPECTED_CSV', got '$ACTUAL_CSV'"
fi

if ! grep -q 'source "\$SCRIPT_DIR/semver-exclusions.sh"' "$SCRIPT_DIR/publish.sh"; then
    fail "publish.sh no longer sources semver-exclusions.sh"
fi
if grep -q -- '--exclude khive-quant' "$SCRIPT_DIR/publish.sh"; then
    fail "publish.sh reintroduced a literal --exclude khive-quant argument"
fi

RELEASE_YML="$SCRIPT_DIR/../.github/workflows/release.yml"
if ! grep -q 'steps.semver-exclusions.outputs.exclude' "$RELEASE_YML"; then
    fail "release.yml no longer consumes the semver-exclusions step output"
fi
if ! grep -q 'source scripts/semver-exclusions.sh' "$RELEASE_YML"; then
    fail "release.yml semver-exclusions step no longer sources the shared helper"
fi

if [[ "$FAILURES" -eq 0 ]]; then
    echo "OK: semver-exclusions helper, publish.sh, and release.yml agree on the exclusion policy."
    exit 0
else
    echo "$FAILURES failure(s)." >&2
    exit 1
fi
