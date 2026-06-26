# perf/ — Performance Ledger

This directory holds the machine-generated performance record for khive-vamana's
approximate nearest-neighbour (ANN) engine.

## Directory contents

| File | Purpose |
|------|---------|
| `ledger.csv` | Banked scale-proof results, one row per (run, N) point |
| `targets.toml` | Assertion thresholds used by the bench harness and CI |
| `bench-runs/` | Raw provenance JSON files banked by `scripts/bench_1m.sh` |

---

## ledger.csv — column definitions

The ledger is the single source of truth for banked performance numbers. Rows
are appended by `scripts/perf/ingest_scale_proof.py` after a successful
bench run; they are never hand-typed.

| Column | Type | Meaning |
|--------|------|---------|
| `date` | ISO-8601 UTC | Wall-clock time at which the bench run completed |
| `sha` | hex prefix | `git rev-parse --short HEAD` at bench time |
| `target` | string | Assertion target key from `targets.toml` |
| `n` | integer | Number of base vectors in the index |
| `beam` | integer | Iso-recall search beam (binary-searched to hit target recall) |
| `recall_at_10` | float | Recall@10 measured against brute-force ground truth |
| `p50_us` | float | Warm-cache query latency at the 50th percentile, microseconds |
| `p95_us` | float | Warm-cache query latency at the 95th percentile, microseconds |
| `p99_us` | float | Warm-cache query latency at the 99th percentile, microseconds |
| `build_ms` | float | Index build wall-clock time, milliseconds |
| `speedup` | float | `brute_us / p50_us` — how much faster ANN query is than brute-force |
| `brute_us` | float | Brute-force p50 latency baseline (see provenance note below) |
| `pass` | PASS/FAIL | Whether all `targets.toml` checks passed for this row |
| `loadavg` | float | 1-minute load average on the runner at measurement time |
| `notes` | string | Free-form provenance annotation (dataset, runner, derivation flags) |

### Source of ledger values

Numbers in the ledger originate from `crates/khive-vamana/examples/vec_bench.rs`.
The bench binary measures ANN query latency, brute-force latency, recall, and
build time, then writes a JSON file. `scripts/perf/ingest_scale_proof.py` reads
that JSON and appends one CSV row per N point. No column is hand-computed after
a run.

---

## brute_us provenance

The three rows currently in `ledger.csv` (banked by PR #153, sha `eb6696c`,
2026-06-15) predate the column's addition to the ledger schema (PR #239).

For those rows, `brute_us` is **back-derived**: it was computed as
`round(p50_us * speedup, 1)` and added retroactively when PR #239 introduced
the column. This derivation is arithmetically consistent because the bench
binary computes `speedup = bf_p50 / warm_p50` from a directly measured
`bf_p50`; multiplying back recovers that measured value. The derivation is
explicitly flagged in the `notes` field of each affected row as
`brute_us=back-derived`.

Back-derived `brute_us` values are therefore **not independently measured
baselines** for those rows. They are reconstructed from the `speedup` ratio.
Rows produced by future runs (post-PR #239) will carry a directly measured
`brute_us` read from `bruteforce_p50_us` in the bench JSON.

---

## Headline result — SIFT-1M

The numbers below are taken directly from `perf/ledger.csv` (rows with
`notes` field `SIFT-1M-honest-3pt macos-arm64 brute_us=back-derived`) and
are confirmed in PR #153.

**On 1 000 000 SIFT-1M vectors (128-dimensional, L2), khive-vamana achieved
recall@10 of 0.9521 at a warm-cache p50 query latency of 171 µs on a
macos-arm64 laptop (PR #153, sha `eb6696c`, 2026-06-15).**

Full three-point table (from `perf/ledger.csv`):

| N | recall@10 | p50 (µs) | p95 (µs) | beam |
|---|-----------|----------|----------|------|
| 100 000 | 0.9504 | 71.0 | 92.9 | 30 |
| 316 228 | 0.9523 | 129.8 | 176.6 | 43 |
| 1 000 000 | 0.9521 | 170.8 | 216.1 | 49 |

### Scaling exponents (from PR #153, crates/khive-vamana/PERF.md)

Power-law fits over the three N points above:

| Signal | Exponent | R² |
|--------|----------|----|
| iso-recall query (warm) | 0.381 | 0.955 |
| beam growth | 0.213 | 0.932 |
| index build | 1.460 | 0.999 |

Query latency grows sub-linearly with N (exponent 0.381, R² 0.955). The
sub-linear claim applies to query latency only. Index build grows
super-linearly (exponent 1.46).

### Speedup figure

The ledger records a 341x speedup at N=1M. That figure compares ANN query
latency against a naive scalar L2 brute-force loop, not against a vectorized
or FAISS-flat baseline. It is a sanity check that the index beats brute-force,
not a competitive benchmark. PR #153 explicitly notes: "Do not lead with 341x."

### Run caveats

1. Three-point fit (100K, 316K, 1M). Scaling exponents carry wider confidence
   intervals than a larger N sweep would provide.
2. Ground-truth for 100K and 316K subsets was recomputed by brute-force L2 on
   each subset. The distributed SIFT-1M GT file indexes only the full 1M base
   and is not valid for subsets.
3. Latency was measured with the index resident in memory (warm cache).
4. The run occurred under load (loadavg1 = 5.54). Latency on a quiescent
   machine will be lower; the scaling exponents are unaffected by constant
   additive noise.
5. Sub-linear query scaling was confirmed at low intrinsic dimensionality.
   SIFT-1M has intrinsic dimensionality approximately 10. Higher intrinsic-
   dimensionality corpora (for example GloVe-100, intrinsic dim approximately
   20) exhibit near-linear query growth, as recorded in project memory.

---

## Reproducing the banked run

```bash
# Set SIFT_DIR to the directory holding sift_base.fvecs and sift_query.fvecs.
export SIFT_DIR=/path/to/sift

# Full 1M run (approximately 7 minutes on Apple Silicon):
make bench-1m

# CI smoke-check (10K/50K synthetic, under 60 seconds):
make bench-1m-ci
```

Output JSON lands in `target/bench-out/` (gitignored). The ingest script
appends a ledger row on a PASS result.
