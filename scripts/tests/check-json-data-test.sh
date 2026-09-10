#!/usr/bin/env bash
# Fixture test for scripts/check-json-data.sh.
# Builds a scratch repository per case; nothing is committed and no hook runs. Run with:
#   bash scripts/tests/check-json-data-test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
GUARD="${CJD_SCRIPT:-$SCRIPT_DIR/check-json-data.sh}"
[ -f "$GUARD" ] || { echo "guard not found at $GUARD" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

fresh_repo() {
  local d="$WORK/$1"
  rm -rf "$d"
  mkdir -p "$d"
  git -C "$d" init -q
  printf '%s\n' "$d"
}

# run_guard <repo> [--all] -> prints rc, captures stderr in $ERR
ERR="$WORK/stderr"
run_guard() {
  local repo="$1"; shift
  local rc=0
  (cd "$repo" && KHIVE_ALLOW_DATA=0 bash "$GUARD" "$@") 2>"$ERR" || rc=$?
  printf '%s' "$rc"
}

pass() { echo "PASS"; }
fail() { echo "FAIL: $1" >&2; cat "$ERR" >&2 || true; exit 1; }

echo "--- case 1: staged JSONL present in the working tree is refused (control) ---"
R=$(fresh_repo c1)
printf '{"a":1}\n' >"$R/data.jsonl"; git -C "$R" add data.jsonl
[ "$(run_guard "$R")" = 1 ] && grep -q 'data.jsonl' "$ERR" && pass || fail "staged data.jsonl was not refused"

echo "--- case 2: staged JSONL whose working-tree file was removed is still refused ---"
R=$(fresh_repo c2)
printf '{"a":1}\n' >"$R/data.jsonl"; git -C "$R" add data.jsonl; rm "$R/data.jsonl"
[ "$(run_guard "$R")" = 1 ] && grep -q 'data.jsonl' "$ERR" && pass || fail "staged data.jsonl with no working-tree file passed"

echo "--- case 3: staged oversize JSON is measured from the index, not the working tree ---"
R=$(fresh_repo c3)
head -c 307200 /dev/zero | tr '\0' 'x' >"$R/big.json"; git -C "$R" add big.json
printf 'x' >"$R/big.json"
[ "$(run_guard "$R")" = 1 ] && grep -q '300KB' "$ERR" && pass || fail "staged 300KB big.json passed after the working-tree copy was truncated"

echo "--- case 4: staged small JSON commits (must-pass control) ---"
R=$(fresh_repo c4)
printf '{"ok":true}\n' >"$R/config.json"; git -C "$R" add config.json
[ "$(run_guard "$R")" = 0 ] && pass || fail "small config.json was refused"

echo "--- case 5: staged JSONL with no working-tree file, deliberate bypass still works ---"
R=$(fresh_repo c5)
printf '{"a":1}\n' >"$R/data.jsonl"; git -C "$R" add data.jsonl; rm "$R/data.jsonl"
rc=0; (cd "$R" && KHIVE_ALLOW_DATA=1 bash "$GUARD") 2>"$ERR" || rc=$?
[ "$rc" = 0 ] && pass || fail "KHIVE_ALLOW_DATA=1 did not bypass"

echo "--- case 6: --all scans the working tree (untracked JSONL refused; index not consulted) ---"
R=$(fresh_repo c6)
printf '{"a":1}\n' >"$R/loose.jsonl"
[ "$(run_guard "$R" --all)" = 1 ] && grep -q 'loose.jsonl' "$ERR" && pass || fail "--all did not refuse an untracked loose.jsonl"

echo "--- case 7: benchmark-path JSONL staged with no working-tree file still passes ---"
R=$(fresh_repo c7)
mkdir -p "$R/bench"; printf '{"t":1}\n' >"$R/bench/run.jsonl"; git -C "$R" add bench/run.jsonl; rm "$R/bench/run.jsonl"
[ "$(run_guard "$R")" = 0 ] && pass || fail "bench/run.jsonl was refused"

echo "all cases passed"
