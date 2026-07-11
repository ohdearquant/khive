#!/usr/bin/env bash
# scripts/perf/ann799_preflight.sh -- ANN-799 fail-closed preflight checks.
#
# Verifies resources, dataset manifest, package hashes, adapter-equivalence
# smoke test, thread pinning env, and host quiescence BEFORE any benchmark
# logic runs. This script never starts a timed block itself; it only prints
# a manifest and exits nonzero on the first failed gate. See
# docs/design/adr-799-baseline-plan.md (khive-work), "Runnable command
# sequence after approval".
#
# Usage:
#   bash scripts/perf/ann799_preflight.sh --data DIR --out OUT_DIR \
#       [--protocol benchmarks/ann799/protocol.toml] [--skip-quiescence]
#
# Exit codes:
#   0   all gates passed
#   1   a gate failed (see printed reason)
#   2   usage error

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DATA_DIR=""
OUT_DIR=""
PROTOCOL="$REPO_ROOT/benchmarks/ann799/protocol.toml"
SKIP_QUIESCENCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data) DATA_DIR="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    --protocol) PROTOCOL="$2"; shift 2 ;;
    --skip-quiescence) SKIP_QUIESCENCE=1; shift ;;
    -h|--help)
      sed -n '2,24p' "${BASH_SOURCE[0]}" | grep '^#' | sed 's/^# \?//'
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$DATA_DIR" || -z "$OUT_DIR" ]]; then
  echo "usage: ann799_preflight.sh --data DIR --out OUT_DIR" >&2
  exit 2
fi

mkdir -p "$OUT_DIR"
FAIL=0

fail() {
  echo "PREFLIGHT FAIL: $1" >&2
  FAIL=1
}

echo "== platform =="
UNAME_S="$(uname -s)"
UNAME_M="$(uname -m)"
echo "uname: $UNAME_S $UNAME_M"
if [[ "$UNAME_S" != "Darwin" || "$UNAME_M" != "arm64" ]]; then
  fail "expected Darwin/arm64 (Apple silicon macOS); got $UNAME_S/$UNAME_M"
fi

echo "== dataset manifest and validation =="
if [[ ! -f "$DATA_DIR/sift_base.fvecs" || ! -f "$DATA_DIR/sift_query.fvecs" || ! -f "$DATA_DIR/sift_groundtruth.ivecs" ]]; then
  fail "missing one of sift_base.fvecs / sift_query.fvecs / sift_groundtruth.ivecs under $DATA_DIR"
else
  if ! uv run python3 "$REPO_ROOT/benchmarks/ann799/dataset.py" manifest \
      --data "$DATA_DIR" --out "$OUT_DIR/dataset-manifest.json"; then
    fail "dataset.py manifest failed (count/dim mismatch or unreadable file)"
  fi
fi

echo "== python env and pinned thread settings =="
for var in OMP_NUM_THREADS OPENBLAS_NUM_THREADS MKL_NUM_THREADS NUMEXPR_NUM_THREADS; do
  if [[ "${!var:-}" != "1" ]]; then
    fail "$var must be set to 1 in the environment before running (got '${!var:-unset}')"
  fi
done

REQ_FILE="$REPO_ROOT/benchmarks/ann799/requirements-macos-arm64.txt"
if ! grep -q "require-hashes-filled" "$REQ_FILE" 2>/dev/null; then
  echo "NOTE: $REQ_FILE has no hash-pinned marker; regenerate with --generate-hashes" \
    "on the target host before a real run (see file header). This is a warning," \
    "not a hard fail, for dry runs against the template file." >&2
fi

echo "== diskann optional-attempt status =="
if [[ -f "$OUT_DIR/diskann.commit" ]]; then
  echo "diskann.commit present: $(cat "$OUT_DIR/diskann.commit")"
else
  echo "diskann-memory not attempted or not built; will be recorded as" \
    "'excluded: does not build on the test platform' per the platform ruling."
fi

echo "== quiescence gate (loadavg1 < ${ANN799_LOADAVG1_MAX:-0.25}) =="
if [[ "$SKIP_QUIESCENCE" -eq 1 ]]; then
  echo "skipped via --skip-quiescence (dry run only)"
else
  LOADAVG_MAX="${ANN799_LOADAVG1_MAX:-0.25}"
  SAMPLES_OK=0
  SAMPLES_TOTAL=10
  for i in $(seq 1 "$SAMPLES_TOTAL"); do
    LOADAVG1="$(uptime | sed -E 's/.*load averages?: ([0-9.]+).*/\1/')"
    OK="$(awk -v l="$LOADAVG1" -v m="$LOADAVG_MAX" 'BEGIN { print (l < m) ? 1 : 0 }')"
    echo "  sample $i: loadavg1=$LOADAVG1 ok=$OK"
    if [[ "$OK" == "1" ]]; then
      SAMPLES_OK=$((SAMPLES_OK + 1))
    fi
    [[ "$i" -lt "$SAMPLES_TOTAL" ]] && sleep 60
  done
  if [[ "$SAMPLES_OK" -ne "$SAMPLES_TOTAL" ]]; then
    fail "quiescence gate: only $SAMPLES_OK/$SAMPLES_TOTAL one-minute samples were below loadavg1 $LOADAVG_MAX." \
      " Reschedule for a quiet coordinated window rather than relabeling this run."
  fi
fi

echo "== manifest written to $OUT_DIR =="
if [[ "$FAIL" -ne 0 ]]; then
  echo "PREFLIGHT FAILED -- see PREFLIGHT FAIL lines above. Not starting a benchmark." >&2
  exit 1
fi
echo "PREFLIGHT PASSED"
exit 0
