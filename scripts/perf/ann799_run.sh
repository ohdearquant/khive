#!/usr/bin/env bash
# scripts/perf/ann799_run.sh -- run the ANN-799 predeclared matrix.
#
# Pins single-threaded math libraries and requests (best-effort) QoS/core
# affinity, then invokes benchmarks/ann799/runner.py over the systems
# declared in protocol.toml. macOS has no user-space equivalent of Linux's
# `taskset`/`numactl`: there is no syscall that pins a process to one
# physical core from the shell. This script uses `taskpolicy -c utility`
# (best-effort QoS/scheduling hint) when available and records in the run
# manifest that core pinning is REQUESTED, not enforced -- do not claim
# hard pinning in the public docs page.
#
# Usage:
#   bash scripts/perf/ann799_run.sh --protocol PROTOCOL.toml --data DIR --out OUT_DIR \
#       [--systems faiss-flat,faiss-hnswflat,faiss-ivfflat,hnswlib]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PROTOCOL="$REPO_ROOT/benchmarks/ann799/protocol.toml"
DATA_DIR=""
OUT_DIR=""
SYSTEMS="faiss-flat,faiss-hnswflat,faiss-ivfflat,hnswlib"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --protocol) PROTOCOL="$2"; shift 2 ;;
    --data) DATA_DIR="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    --systems) SYSTEMS="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | grep '^#' | sed 's/^# \?//'
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$DATA_DIR" || -z "$OUT_DIR" ]]; then
  echo "usage: ann799_run.sh --protocol P --data DIR --out OUT_DIR [--systems ...]" >&2
  exit 2
fi

export OMP_NUM_THREADS=1
export OPENBLAS_NUM_THREADS=1
export MKL_NUM_THREADS=1
export NUMEXPR_NUM_THREADS=1

mkdir -p "$OUT_DIR"

CORE_PIN_MODE="requested-not-enforced"
if command -v taskpolicy >/dev/null 2>&1; then
  RUN_CMD=(taskpolicy -c utility)
else
  RUN_CMD=()
fi

cat > "$OUT_DIR/environment-manifest.json" <<EOF
{
  "uname": "$(uname -a)",
  "hostname": "$(hostname)",
  "cpu_brand": "$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)",
  "physical_cores": "$(sysctl -n hw.physicalcpu 2>/dev/null || echo unknown)",
  "logical_cores": "$(sysctl -n hw.logicalcpu 2>/dev/null || echo unknown)",
  "memsize_bytes": "$(sysctl -n hw.memsize 2>/dev/null || echo unknown)",
  "python_version": "$(uv run python3 --version 2>&1)",
  "core_pin_mode": "$CORE_PIN_MODE",
  "omp_num_threads": "$OMP_NUM_THREADS",
  "command_line": "$0 $*"
}
EOF

echo "environment manifest written to $OUT_DIR/environment-manifest.json"
echo "core pinning: $CORE_PIN_MODE (macOS has no taskset/numactl equivalent)"

"${RUN_CMD[@]}" uv run python3 "$REPO_ROOT/benchmarks/ann799/runner.py" \
  --protocol "$PROTOCOL" \
  --data "$DATA_DIR" \
  --out "$OUT_DIR" \
  --systems "$SYSTEMS"
