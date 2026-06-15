#!/usr/bin/env bash
# scripts/bench_1m.sh — reproducible Vamana 1M-vector scale-proof bench
#
# Produces the SIFT-1M 3-point (100K/316K/1M) proof that confirms sublinear
# query scaling on khive-vamana. Results are written to BENCH_OUT as a JSON
# file and every row is appended to perf/ledger.csv.
#
# Usage:
#   bash scripts/bench_1m.sh [--ns <N1,N2,...>] [--dataset <name>] [--ci]
#
#   --ns       comma-separated subset sizes (default: 100000,316228,1000000)
#   --dataset  dataset name tag written into bench JSON (default: SIFT-1M-honest-3pt)
#   --ci       shorthand for --ns 10000,50000 (fast CI smoke-check)
#
# Environment:
#   SIFT_DIR   directory containing sift_base.fvecs and sift_query.fvecs
#              (default: /Users/lion/projects/khive/khive-sublinear/data/sift)
#   BENCH_OUT  output directory for bench JSON and logs
#              (default: target/bench-out; gitignored via target/)
#
# Exit codes:
#   0   bench ran and all assertions PASSed
#   1   bench ran but one or more assertions FAILed
#   2   prerequisite missing (SIFT data, binary, etc.)
#   3   usage error
#
# Notes:
#   - Run from the worktree root (the directory containing scripts/).
#   - Cargo is invoked from the crates/ subdirectory (no root Cargo.toml).
#   - Raw bench JSON is written to BENCH_OUT; that directory is outside the
#     repo tree by default (target/bench-out) and is gitignored.
#   - On success, ledger rows are appended to perf/ledger.csv.
#   - For CI, use --ci (runs 10K/50K; fast; still asserts PASS criteria).

set -euo pipefail

# ── Locate repo root (the directory that contains scripts/) ─────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATES_DIR="$REPO_ROOT/crates"

# ── Defaults ────────────────────────────────────────────────────────────────
SIFT_DIR="${SIFT_DIR:-/Users/lion/projects/khive/khive-sublinear/data/sift}"
BENCH_OUT="${BENCH_OUT:-$REPO_ROOT/target/bench-out}"
NS="100000,316228,1000000"
DATASET="SIFT-1M-honest-3pt"
CI_MODE=0

# ── Argument parsing ────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ns)
      NS="$2"; shift 2 ;;
    --dataset)
      DATASET="$2"; shift 2 ;;
    --ci)
      CI_MODE=1
      NS="10000,50000"
      DATASET="SIFT-CI-smoke"
      shift ;;
    -h|--help)
      sed -n '2,40p' "${BASH_SOURCE[0]}" | grep '^#' | sed 's/^# \?//'
      exit 0 ;;
    *)
      echo "ERROR: Unknown argument: $1" >&2
      exit 3 ;;
  esac
done

# ── Derived paths ────────────────────────────────────────────────────────────
BASE_FILE="$SIFT_DIR/sift_base.fvecs"
QUERY_FILE="$SIFT_DIR/sift_query.fvecs"
TARGETS_TOML="$REPO_ROOT/perf/targets.toml"
LEDGER_CSV="$REPO_ROOT/perf/ledger.csv"
TARGET_KEY="khive-vamana/1m-scale-proof/sift-1m"

mkdir -p "$BENCH_OUT"
BENCH_JSON="$BENCH_OUT/${DATASET}.json"
LOG_FILE="$BENCH_OUT/${DATASET}.log"

echo "=== khive-vamana 1M scale-proof bench ===" | tee "$LOG_FILE"
echo "Date:       $(date -Iseconds)" | tee -a "$LOG_FILE"
echo "Repo:       $REPO_ROOT" | tee -a "$LOG_FILE"
echo "SIFT_DIR:   $SIFT_DIR" | tee -a "$LOG_FILE"
echo "BENCH_OUT:  $BENCH_OUT" | tee -a "$LOG_FILE"
echo "ns:         $NS" | tee -a "$LOG_FILE"
echo "dataset:    $DATASET" | tee -a "$LOG_FILE"
echo "CI mode:    $CI_MODE" | tee -a "$LOG_FILE"
echo "" | tee -a "$LOG_FILE"

# ── Prerequisites ────────────────────────────────────────────────────────────
prereq_fail=0

if [[ ! -f "$BASE_FILE" ]]; then
  echo "ERROR: SIFT base vectors not found: $BASE_FILE" >&2
  echo "  Set SIFT_DIR to the directory containing sift_base.fvecs and sift_query.fvecs" >&2
  prereq_fail=1
fi

if [[ ! -f "$QUERY_FILE" ]]; then
  echo "ERROR: SIFT query vectors not found: $QUERY_FILE" >&2
  prereq_fail=1
fi

if [[ ! -f "$TARGETS_TOML" ]]; then
  echo "ERROR: targets.toml not found: $TARGETS_TOML" >&2
  prereq_fail=1
fi

if ! command -v cargo &>/dev/null; then
  echo "ERROR: cargo not found in PATH" >&2
  prereq_fail=1
fi

if [[ $prereq_fail -ne 0 ]]; then
  exit 2
fi

# ── Run the bench ────────────────────────────────────────────────────────────
echo "--- Running vec_bench ---" | tee -a "$LOG_FILE"

BENCH_EXIT=0
(
  cd "$CRATES_DIR"
  cargo run --release -p khive-vamana --example vec_bench -- \
    --base "$BASE_FILE" \
    --query "$QUERY_FILE" \
    --ns "$NS" \
    --dataset "$DATASET" \
    --targets "$TARGETS_TOML" \
    --target-key "$TARGET_KEY" \
    --out "$BENCH_JSON"
) 2>&1 | tee -a "$LOG_FILE"
BENCH_EXIT="${PIPESTATUS[0]}"

echo "" | tee -a "$LOG_FILE"
echo "--- vec_bench exit code: $BENCH_EXIT ---" | tee -a "$LOG_FILE"

if [[ $BENCH_EXIT -ne 0 ]]; then
  echo "ERROR: vec_bench exited with code $BENCH_EXIT" >&2
  exit 1
fi

# ── Ingest into ledger ───────────────────────────────────────────────────────
if [[ -f "$BENCH_JSON" ]]; then
  echo "--- Ingesting into ledger: $LEDGER_CSV ---" | tee -a "$LOG_FILE"
  INGEST_EXIT=0
  (
    cd "$REPO_ROOT"
    python3 scripts/perf/ingest_scale_proof.py \
      --in "$BENCH_JSON" \
      --ledger "$LEDGER_CSV"
  ) 2>&1 | tee -a "$LOG_FILE"
  INGEST_EXIT="${PIPESTATUS[0]}"

  if [[ $INGEST_EXIT -ne 0 ]]; then
    echo "WARNING: ledger ingest failed (exit $INGEST_EXIT) — bench result is still valid" >&2
  fi
else
  echo "WARNING: bench JSON not found after run ($BENCH_JSON) — ledger not updated" >&2
fi

# ── Report overall result ────────────────────────────────────────────────────
echo "" | tee -a "$LOG_FILE"
if [[ $BENCH_EXIT -eq 0 ]]; then
  echo "=== RESULT: PASS ===" | tee -a "$LOG_FILE"
  echo "    JSON: $BENCH_JSON" | tee -a "$LOG_FILE"
  echo "    Log:  $LOG_FILE" | tee -a "$LOG_FILE"
  exit 0
else
  echo "=== RESULT: FAIL (bench assertions failed) ===" | tee -a "$LOG_FILE"
  exit 1
fi
