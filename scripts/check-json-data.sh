#!/usr/bin/env bash
# check-json-data.sh — block JSON/JSONL corpus files from being committed.
#
# Based on the machine-wide guard born 2026-06-10 after an agent session pushed
# an entire memory corpus as JSONL onto a public PR.
#
# Modes:
#   default (no args)    — inspect staged files (git diff --cached); use in pre-commit
#   --all                — scan entire working tree; use in CI
#
# Rules:
#   .jsonl / .ndjson     — ALWAYS blocked unless path looks like benchmark
#                          results (bench/, benchmark/, criterion/, *bench*result*)
#   .json                — blocked if larger than MAX_JSON_KB (default 256 KB),
#                          unless a known lockfile or benchmark-results path
#
# Bypass (deliberate, auditable): KHIVE_ALLOW_DATA=1 git commit ...
# Tune:    KHIVE_MAX_JSON_KB=512 git commit ...

set -uo pipefail

[ "${KHIVE_ALLOW_DATA:-0}" = "1" ] && exit 0

MAX_JSON_KB="${KHIVE_MAX_JSON_KB:-256}"
LOCKFILES='package-lock.json|deno.lock|flake.lock|composer.lock|bun.lock|.package.resolved'
BENCH_RE='(^|/)(bench|benches|benchmark|benchmarks|criterion)(/|$)|bench.*result|result.*bench'

fail=0

check_file() {
  local f="$1"
  [ -f "$f" ] || return 0
  local base lower size_kb
  base="$(basename "$f")"
  lower="$(printf '%s' "$f" | tr '[:upper:]' '[:lower:]')"

  case "$base" in
    *.jsonl|*.ndjson)
      if ! printf '%s' "$lower" | grep -qE "$BENCH_RE"; then
        echo "BLOCKED: $f — JSONL/NDJSON staged outside a benchmark-results path." >&2
        fail=1
      fi
      ;;
    *.json)
      printf '%s' "$base" | grep -qE "^(${LOCKFILES})$" && return 0
      printf '%s' "$lower" | grep -qE "$BENCH_RE" && return 0
      size_kb=$(( ($(wc -c < "$f") + 1023) / 1024 ))
      if [ "$size_kb" -gt "$MAX_JSON_KB" ]; then
        echo "BLOCKED: $f — ${size_kb}KB JSON exceeds ${MAX_JSON_KB}KB config-file ceiling." >&2
        fail=1
      fi
      ;;
  esac
}

if [ "${1:-}" = "--all" ]; then
  # CI mode: scan every tracked + untracked (non-ignored) file in the tree
  while IFS= read -r f; do
    check_file "$f"
  done < <(git ls-files && git ls-files --others --exclude-standard)
else
  # Pre-commit mode: staged files only
  while IFS= read -r f; do
    check_file "$f"
  done < <(git diff --cached --name-only --diff-filter=ACMR)
fi

if [ "$fail" -ne 0 ]; then
  cat >&2 <<'EOF'

Large JSON / any JSONL outside benchmark paths is treated as a data-corpus
leak risk (see the khive-used-to-be-oss incident, 2026-06).
  - Data exports belong in .khive/, data/, or object storage — not git.
  - Benchmark results: keep them under a bench*/criterion path.
  - Genuinely intentional? Re-run with: KHIVE_ALLOW_DATA=1 git commit ...
EOF
  exit 1
fi
exit 0
