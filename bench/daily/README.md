# Daily bench rows

One JSONL row per day is appended to `YYYY-MM.jsonl` (schema `bench-row/v1`). Each row is
reproducible from the verbatim `command` field at the recorded `sha`, with host and run
configuration captured in the row and the native result stored under `bench/data/`.

## Registered suites

| Suite | Command | What it measures |
| --- | --- | --- |
| `load_harness_smoke` | `uv run scripts/perf/bench_load_harness.py --mode real --workers 20 --tenants 4 --ops-per-worker 20 --report <path>` | Daemon concurrency acceptance: client-measured recall latency percentiles, dispatch fallback count, SQLite busy/locked count, write-queue backpressure, WAL page floor, and per-tenant attribution consistency, driven against a scratch database. `--mode real` uses the real embedder and requires exclusive access to the machine's GPU test lock; `--mode bench` substitutes a hash embedder for GPU-free runs. |

Additional suites are registered here as their runs begin landing rows.

## Row conventions

- `lane` is `perf` for metric rows from the suites above.
- `n` and `dispersion` reflect what the harness natively emits (latency dimensions report
  percentiles over the per-op sample).
- A blocked day is recorded as an explicit miss row: `{"schema": "bench-row/v1", "date": ...,
  "repo": "khive", "miss": true, "reason": "<one line>"}`.
- `artifact` points at the committed native result under `bench/data/` (bulk ledger data
  stays off `main`; only the day's row and its directly referenced result land here).
