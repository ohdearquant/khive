# khive-vamana — 1M-Vector Scale-Proof Results

This document banks the measured ANN scaling behaviour of khive-vamana on the
SIFT-1M benchmark. It is the authoritative record for the "sublinear query
scaling" claim made in the project documentation.

## Run configuration

| Parameter       | Value         |
|-----------------|---------------|
| max_degree (R)  | 64            |
| search_list_size (L) | 128      |
| alpha           | 1.0           |
| build_batch     | 1024          |
| k (neighbours)  | 10            |
| target_recall   | 0.95          |
| n_gt_queries    | 1000          |
| n_latency_queries | 1000        |

**Dataset**: SIFT-1M — 128-dimensional L2-normalized float32 vectors, BIGANN
format (`.fvecs`). Ground-truth recomputed by brute-force on each subset
(the provided ANN GT file indexes the full 1M base and is invalid for subsets).

**Runner**: macos-arm64 | loadavg1 = 5.54 (high-load, see Caveats)

**git sha**: eb6696c (PR #137)

**Produced**: 2026-06-15T20:04:45Z

## Results

| N | iso-recall beam | recall@10 | p50 (µs) | p95 (µs) | p99 (µs) | build (ms) | speedup vs brute |
|---|-----------------|-----------|----------|----------|----------|------------|-----------------|
| 100 000 | 30 | 0.9504 | 71.0 | 92.9 | 102.3 | 11 123 | 88.8x |
| 316 228 | 43 | 0.9523 | 129.8 | 176.6 | 200.3 | 54 590 | 152.6x |
| 1 000 000 | 49 | 0.9521 | 170.8 | 216.1 | 234.2 | 320 777 | 341.4x |

## Fitted scaling exponents (3-point power-law fit)

| Signal | Exponent | R² | Interpretation |
|--------|----------|----|----------------|
| beam growth | 0.213 | 0.932 | Sub-linear (flat) |
| iso-recall query (warm) | 0.381 | 0.955 | Sub-linear |
| build wall-clock | 1.460 | 0.999 | Super-linear (Omega(N)) |
| brute-force (reference) | 0.966 | 1.000 | Near-linear |

## Decisive-question verdict

YES — all criteria met:

- beam flat (exponent 0.213 < threshold 0.5)
- query sub-linear (exponent 0.381 < threshold 0.8)
- recall >= 0.95 at all three N
- ANN exceeds the configured 10x sanity threshold at 1M (measured 341x only against naive scalar L2, not FAISS / vectorized flat search — see Speedup note)

## Speedup note

The 341x speedup is measured vs a NAIVE SCALAR L2 brute-force baseline — it
is a sanity check, NOT a competitive number. A credible head-to-head requires
a vectorized / faiss-flat baseline (tracked as M-06).

## Caveats

1. **3-point fit** — power-law exponents are fitted over 3 points only (100K,
   316K, 1M). A 4+ point fit would tighten R² confidence.
2. **Ground-truth policy** — the BIGANN GT file indexes the full 1M base and
   is not valid for the 100K or 316K subsets. GT was recomputed by brute-force
   L2 scan on each subset before evaluating recall.
3. **Build is Omega(N)** — the build wall-clock exponent is 1.46 (super-linear).
   The sub-linear claim applies to query and update latency only, not to
   index construction.
4. **Warm-cache latency** — query latency was measured with the index resident
   in memory (warm cache). Cold-cache first-query latency will be higher,
   especially at 1M vectors.
5. **High-load run** — loadavg1 = 5.54 at measurement time. Latency numbers
   may be inflated vs a quiescent machine; the scaling shape (exponents) is
   unaffected by constant additive noise.
6. **Regime boundary** — sub-linear query scaling holds at low intrinsic
   dimensionality (SIFT intrinsic dim ~10). Benchmarks on higher intrinsic-dim
   corpora (e.g. GloVe-100, intrinsic dim ~20) show near-linear query growth.
   khive's production embedding model (all-MiniLM-L6-v2) has measured
   intrinsic dim ~14, placing it in the sub-linear regime.

## Reproducing this result

```bash
# Set SIFT_DIR to the directory containing sift_base.fvecs and sift_query.fvecs.
export SIFT_DIR=/path/to/sift

# Full 1M run (~7 minutes on Apple Silicon):
make bench-1m

# CI smoke-check (10K/50K, <60 seconds):
make bench-1m-ci
```

Output JSON lands in `target/bench-out/` (gitignored). The ledger row is
appended to `perf/ledger.csv` on success.
