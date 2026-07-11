# ANN-799 matched-condition ANN benchmark harness

Harness for the khive-vamana vs. external-ANN comparison defined in
`docs/design/adr-799-baseline-plan.md` (khive-work) and superseded on
platform by the macOS-ARM ruling in the parent PR. See
`docs/benchmarks/ann799-matched-ann.md` for the public methodology page and
claim scope.

## Layout

| Path                           | Purpose                                                                                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `protocol.toml`                | Fixed construction settings, dataset split, seeds, statistical thresholds -- the single source of truth `runner.py`/`report.py` read.      |
| `requirements-macos-arm64.txt` | Pinned `faiss-cpu`/`hnswlib`/`numpy` versions for Apple silicon. Hashes are filled in on the target host (see file header).                |
| `dataset.py`                   | `.fvecs`/`.ivecs` readers, SHA256 dataset manifest, count/dim validation.                                                                  |
| `runner.py`                    | Drives one adapter through build (x3) → calibration → ten warm evaluation blocks, writing JSONL raw samples and a per-system summary JSON. |
| `adapters/base.py`             | Shared `AnnAdapter` ABI: `build`, `load`, `set_search_width`, `search_one`, `save`, `artifact_paths`, `metadata`.                          |
| `adapters/faiss_cpu.py`        | `IndexFlatL2`, `IndexHNSWFlat`, `IndexIVFFlat` adapters. Import-guarded.                                                                   |
| `adapters/hnswlib_adapter.py`  | hnswlib L2 HNSW adapter. Import-guarded.                                                                                                   |
| `adapters/FOLLOW-UP.md`        | Status of the deferred `khive-vamana` (Rust) and optional-attempt `diskann-memory` (C++) adapters.                                         |
| `schema-v2.json`               | JSON Schema for raw per-query samples and per-system summary records.                                                                      |
| `report.py`                    | Percentiles, bootstrap CI, CV, paired permutation test, Cohen's dz, iso-recall eligibility; renders the results section of the docs page.  |

## Exact command lines

These mirror the plan's "Runnable command sequence after approval"; they
are not run as part of this harness slice. `ANN799_ROOT` is scratch space
outside the repo (gitignored).

```bash
set -euo pipefail
export ANN799_ROOT="$PWD/target/ann799"
export SIFT_DIR="$ANN799_ROOT/data/sift1m"
mkdir -p "$ANN799_ROOT" "$SIFT_DIR"

# 1. Fetch SIFT-1M (never commit this data; perf/ann799-runs/ and the data
#    directory are both gitignored).
curl -fL http://corpus-texmex.irisa.fr/sift.tar.gz -o "$ANN799_ROOT/sift.tar.gz"
tar -xzf "$ANN799_ROOT/sift.tar.gz" -C "$SIFT_DIR" --strip-components=1

# 2. Python env with hash-pinned, macOS-ARM wheels.
uv venv "$ANN799_ROOT/venv"
. "$ANN799_ROOT/venv/bin/activate"
uv pip install --require-hashes -r benchmarks/ann799/requirements-macos-arm64.txt

# 3. Fail-closed preflight: dataset manifest, thread-pinning env,
#    quiescence gate.
export OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1 NUMEXPR_NUM_THREADS=1
bash scripts/perf/ann799_preflight.sh --data "$SIFT_DIR" --out "$ANN799_ROOT/preflight"

# 4. Run the predeclared matrix (systems without an adapter yet are skipped
#    with a stderr note -- see adapters/FOLLOW-UP.md).
bash scripts/perf/ann799_run.sh \
  --protocol benchmarks/ann799/protocol.toml \
  --data "$SIFT_DIR" \
  --out "$ANN799_ROOT/run-$(date -u +%Y%m%dT%H%M%SZ)"

# 5. Validate + append to the ledger.
python3 scripts/perf/ann799_ingest.py \
  --runs "$ANN799_ROOT/run-<timestamp>" \
  --ledger perf/ann799-ledger.csv

# 6. Render the public results page.
python3 benchmarks/ann799/report.py \
  --runs "$ANN799_ROOT/run-<timestamp>" \
  --out docs/benchmarks/ann799-matched-ann.md
```

## Adapter equivalence smoke test

Before a 1M-vector run is attempted, run every enabled adapter over a
10,000-vector subset of the base set and confirm they all return the same
top-100 IDs as `faiss-flat` (the exact control) for a small query sample.
`runner.py` does not run this automatically yet -- wire it into
`ann799_preflight.sh` before the first real run, per the plan's gate 4.

## Known platform limitations (documented, not silently worked around)

- **No `taskset`/`numactl` on macOS.** `ann799_run.sh` requests QoS via
  `taskpolicy` when available and records `core_pin_mode:
  "requested-not-enforced"` in the environment manifest instead of
  claiming hard pinning.
- **RSS sampling is external, not a true peak-reset primitive.**
  `runner.py`'s `RssSampler` polls `ps -o rss=` every 100ms per the plan's
  cadence and reports the max seen during the wrapped phase; it measures
  whole-process RSS, not an isolated allocator delta.
- **DiskANN memory index is optional-attempt** on this platform -- see
  `adapters/FOLLOW-UP.md`.
