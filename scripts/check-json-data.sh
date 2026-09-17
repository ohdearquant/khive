#!/usr/bin/env bash
# check-json-data.sh — block JSON/JSONL corpus files from being committed.
#
# Based on the machine-wide guard born 2026-06-10 after an agent session pushed
# an entire memory corpus as JSONL onto a public PR.
#
# Modes:
#   default (no args)    — inspect staged files (git diff --cached); use in pre-commit.
#                          Reads the staged blobs from the index, never the working
#                          tree: a staged file that was since deleted or rewritten on
#                          disk is judged by what the commit will contain.
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
#
# Per-path exemptions (optional, consumer-repo owned):
#   A repository that must track a JSON file above the ceiling declares it in
#   .check-json-data-exemptions at the repository root. One exemption per line:
#
#     <anchored path regex><whitespace><ceiling in KB>
#     ^data/manifest\.json$    2048
#
#   Blank lines and lines beginning with # are ignored. The regex MUST be
#   anchored with ^ and $: an unanchored pattern would exempt every path it
#   appears in, which is how a per-file exemption turns into a disabled guard.
#   The ceiling is still enforced, so an exempt file that grows past its own
#   declared size is still blocked.
#
#   The file is read identically in pre-commit and --all mode. When it does not
#   exist, behaviour is exactly as it was before exemptions existed. A malformed
#   line is a configuration error, not a pass: the guard exits 2 naming the file
#   and the line, so a typo can never read as permission.
#
#   SCOPE, stated so it can be argued with: exemptions reach the .json SIZE arm
#   ONLY. They do not exempt .jsonl/.ndjson, because that arm blocks a file
#   FORMAT regardless of size and a size ceiling cannot express it. What would
#   falsify this choice is a consumer that legitimately tracks a small JSONL
#   fixture outside a benchmark path; today no such case is known, and such a
#   consumer should get its own arm rather than a size number that means nothing.

set -uo pipefail

[ "${KHIVE_ALLOW_DATA:-0}" = "1" ] && exit 0

MAX_JSON_KB="${KHIVE_MAX_JSON_KB:-256}"
LOCKFILES='package-lock.json|deno.lock|flake.lock|composer.lock|bun.lock|.package.resolved'
BENCH_RE='(^|/)(bench|benches|benchmark|benchmarks|criterion)(/|$)|bench.*result|result.*bench'
SHOWCASE_GOLDEN_RE='^(docs/schemas/examples/khive-repo-v1-khive\.json|apps/kg-editor/public/showcase/khive-repo-v1-khive\.json)$'
SHOWCASE_MAX_JSON_KB=8192

EXEMPTION_FILE_NAME='.check-json-data-exemptions'
# Validated exemptions, one per line, "<regex> <ceiling>". A newline-delimited
# string rather than two arrays on purpose: under `set -u` an empty array is an
# unbound variable on bash 3.2, which macOS still ships as /bin/bash, and the
# empty case is the one every repository without an exemption file takes.
EXEMPTIONS=''

# A configuration error exits here rather than returning a verdict. The guard
# decides whether a commit is allowed, so an unreadable rule is the one thing it
# must not resolve in the permissive direction.
config_error() {
  echo "check-json-data: $1" >&2
  echo "check-json-data: refusing to run with an unreadable exemption rule; fix $EXEMPTION_FILE_NAME." >&2
  exit 2
}

load_exemptions() {
  local root path line lineno pattern ceiling
  root=$(git rev-parse --show-toplevel 2>/dev/null) || return 0
  path="$root/$EXEMPTION_FILE_NAME"
  [ -f "$path" ] || return 0

  lineno=0
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    case "$line" in
      ''|'#'*) continue ;;
    esac
    # shellcheck disable=SC2086
    set -- $line
    # A line that is only whitespace, or whose first word begins the comment,
    # carries no rule. This is checked AFTER word splitting so that indented
    # comments and whitespace-only lines are ignored the same way bare ones are;
    # checking the raw line alone treated "   " as a malformed rule.
    [ "$#" -eq 0 ] && continue
    case "$1" in '#'*) continue ;; esac
    [ "$#" -eq 2 ] || config_error "$EXEMPTION_FILE_NAME:$lineno: expected '<anchored regex> <ceiling KB>', got: $line"
    pattern="$1"
    ceiling="$2"
    case "$pattern" in
      '^'*) ;;
      *) config_error "$EXEMPTION_FILE_NAME:$lineno: pattern must start with ^ so it cannot match a path it was not written for: $pattern" ;;
    esac
    case "$pattern" in
      *'$') ;;
      *) config_error "$EXEMPTION_FILE_NAME:$lineno: pattern must end with \$ so it cannot match a longer path: $pattern" ;;
    esac
    case "$ceiling" in
      ''|*[!0-9]*) config_error "$EXEMPTION_FILE_NAME:$lineno: ceiling must be a positive integer number of KB, got: $ceiling" ;;
    esac
    [ "$ceiling" -gt 0 ] || config_error "$EXEMPTION_FILE_NAME:$lineno: ceiling must be greater than zero, got: $ceiling"
    printf '' | grep -qE "$pattern" 2>/dev/null
    [ "$?" -le 1 ] || config_error "$EXEMPTION_FILE_NAME:$lineno: pattern is not a valid extended regular expression: $pattern"
    EXEMPTIONS="${EXEMPTIONS}${pattern} ${ceiling}
"
  done < "$path"
}

# Prints the declared ceiling for a path and returns 0 when one is declared.
# First match wins, so an earlier line in the file takes precedence.
exempt_ceiling_for() {
  local f="$1" entry pattern ceiling
  [ -n "$EXEMPTIONS" ] || return 1
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    pattern="${entry% *}"
    ceiling="${entry##* }"
    if printf '%s' "$f" | grep -qE "$pattern"; then
      printf '%s' "$ceiling"
      return 0
    fi
  done <<EOF
$EXEMPTIONS
EOF
  return 1
}

MODE=tree
fail=0

# Byte size of the object the mode judges: the staged blob in pre-commit mode, the
# working-tree file in --all mode. Prints nothing and returns non-zero when unreadable.
object_bytes() {
  if [ "$MODE" = staged ]; then
    git cat-file -s ":$1" 2>/dev/null
  else
    wc -c < "$1"
  fi
}

check_file() {
  local f="$1"
  [ "$MODE" = staged ] || [ -f "$f" ] || return 0
  local base lower bytes size_kb ceiling_kb
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
      if ! bytes=$(object_bytes "$f") || [ -z "$bytes" ]; then
        echo "BLOCKED: $f — staged JSON could not be read from the index." >&2
        fail=1
        return 0
      fi
      size_kb=$(( (bytes + 1023) / 1024 ))
      # ADR-147 requires one canonical public golden and its byte-identical browser
      # asset. Keep this exception exact and below the renderer's closed 8 MiB cap;
      # the KG Studio contract job validates both JSON shape and byte parity.
      if printf '%s' "$lower" | grep -qE "$SHOWCASE_GOLDEN_RE"; then
        if [ "$size_kb" -gt "$SHOWCASE_MAX_JSON_KB" ]; then
          echo "BLOCKED: $f — ${size_kb}KB showcase golden exceeds ${SHOWCASE_MAX_JSON_KB}KB ceiling." >&2
          fail=1
        fi
        return 0
      fi
      # Consumer-declared exemption. The path is matched as tracked, not
      # lower-cased: these regexes name exact files a repository chose to keep,
      # and a case-folded match would silently cover a sibling on a
      # case-sensitive filesystem.
      if ceiling_kb=$(exempt_ceiling_for "$f"); then
        if [ "$size_kb" -gt "$ceiling_kb" ]; then
          echo "BLOCKED: $f — ${size_kb}KB exceeds the ${ceiling_kb}KB ceiling declared for it in ${EXEMPTION_FILE_NAME}." >&2
          fail=1
        fi
        return 0
      fi
      if [ "$size_kb" -gt "$MAX_JSON_KB" ]; then
        echo "BLOCKED: $f — ${size_kb}KB JSON exceeds ${MAX_JSON_KB}KB config-file ceiling." >&2
        fail=1
      fi
      ;;
  esac
}

load_exemptions

if [ "${1:-}" = "--all" ]; then
  # CI mode: scan every tracked + untracked (non-ignored) file in the tree
  while IFS= read -r f; do
    check_file "$f"
  done < <(git ls-files && git ls-files --others --exclude-standard)
else
  # Pre-commit mode: staged files only, judged from the index
  MODE=staged
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
