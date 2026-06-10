# khive-fusion Benchmark Ledger

## Benchmark Suite: `fusion_bench`

**Source:** `crates/khive-fusion/benches/fusion_bench.rs`
**Harness:** Criterion 0.5 (`harness = false`)

## Run Command

```bash
# From workspace root
cd crates && cargo bench -p khive-fusion --bench fusion_bench

# With HTML report (requires gnuplot)
cd crates && cargo bench -p khive-fusion --bench fusion_bench -- --output-format bencher

# Single group only
cd crates && cargo bench -p khive-fusion --bench fusion_bench -- rrf
```

## Benchmark Groups

| Group             | What it measures                              | Scenarios                                           |
| ----------------- | --------------------------------------------- | --------------------------------------------------- |
| `rrf`             | `reciprocal_rank_fusion` throughput           | (2 src, 50/150/500 items), (3 src, 150/500 items)   |
| `weighted`        | `weighted_fusion` throughput                  | same matrix as rrf                                  |
| `union`           | `union_fusion` throughput                     | same matrix as rrf                                  |
| `fuse_dispatcher` | `fuse()` dispatch overhead per strategy       | all 5 strategies + top_k sensitivity at k=10/50/100 |
| `weight_utils`    | `normalize_weights`, `weights_are_normalized` | 2/3/20-element weight vectors                       |

## Dataset Shape

Sources are generated deterministically via a linear congruential generator (seed 42).
Each source has 30% overlap IDs across sources to exercise the HashMap merge path.

Input IDs are `u64`; scores are `DeterministicScore` derived from LCG output in `[0, 1)`.

## Timing Methodology Note

Clone cost is intentionally included in the measured path (sources are cloned inside
`b.iter`). This reflects the real call site where `fuse()` takes ownership of source
vectors. The benchmark measures allocation + fusion together to give a realistic end-to-end
number. If algorithm-only timing is needed in future, use `iter_batched` to pre-clone
outside the measured path and document the change here.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-fusion --bench fusion_bench`
- Dataset: LCG-generated sources, seed 42, 30% cross-source ID overlap; item counts 50/150/500;
  sample size 50 (rrf/weighted/union/dispatcher), 200 (weight_utils)
- vs prior: first formal release ledger entry — no prior comparable baseline

#### RRF Fusion

| Scenario         | Low      | Median   | High     | Outliers    |
| ---------------- | -------- | -------- | -------- | ----------- |
| rrf/2src/50items | 3.249 µs | 3.267 µs | 3.299 µs | 2/50 (4%)   |
| rrf/2src/150     | 9.582 µs | 9.851 µs | 10.35 µs | 2/50 (4%)   |
| rrf/2src/500     | 35.81 µs | 39.90 µs | 45.05 µs | 10/50 (20%) |
| rrf/3src/150     | 14.62 µs | 14.66 µs | 14.71 µs | 6/50 (12%)  |
| rrf/3src/500     | 50.28 µs | 51.08 µs | 52.25 µs | 10/50 (20%) |

#### Weighted Fusion

| Scenario              | Low      | Median    | High      | Outliers    |
| --------------------- | -------- | --------- | --------- | ----------- |
| weighted/2src/50items | 5.212 µs | 5.862 µs  | 6.733 µs  | 3/50 (6%)   |
| weighted/2src/150     | 14.38 µs | 16.22 µs  | 18.29 µs  | 2/50 (4%)   |
| weighted/2src/500     | 46.73 µs | 50.22 µs  | 55.31 µs  | 9/50 (18%)  |
| weighted/3src/150     | 21.57 µs | 24.10 µs  | 26.97 µs  | 11/50 (22%) |
| weighted/3src/500     | 85.55 µs | 105.95 µs | 126.76 µs | 11/50 (22%) |

#### Union Fusion

| Scenario           | Low      | Median   | High     | Outliers         |
| ------------------ | -------- | -------- | -------- | ---------------- |
| union/2src/50items | 2.295 µs | 2.429 µs | 2.626 µs | 8/50 (16%)       |
| union/2src/150     | 7.296 µs | 7.886 µs | 8.671 µs | 3/50 (6%)        |
| union/2src/500     | 25.27 µs | 26.39 µs | 27.89 µs | 10/50 (20%)      |
| union/3src/150     | 10.11 µs | 10.31 µs | 10.69 µs | 2/50 (4%)        |
| union/3src/500     | —        | —        | —        | not yet recorded |

- Notes: union/3src/500 and fuse_dispatcher/weight_utils groups not yet recorded in this ledger
  entry; numbers will be added on next full bench run.

## Regression Policy

A >10% wall-time regression in `rrf` or `weighted` at the 2×150-item scenario
requires a comment in the PR explaining the cause before merge.

Last reviewed: v0.2.8 (2026-06-08)
