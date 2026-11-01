# khive-fusion Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-fusion`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                                           | Reference median | A/A ratio | Baseline commit                            | Commit date |
| --------------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `fuse_dispatcher/KeywordOnly`                       | 162.709 ns       | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/Rrf`                               | 14.004 µs        | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/Union`                             | 8.992 µs         | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/VectorOnly`                        | 154.100 ns       | 1.038     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/Weighted`                          | 16.232 µs        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/rrf_topk/10`                       | 48.712 µs        | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/rrf_topk/100`                      | 49.075 µs        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/rrf_topk/50`                       | 48.720 µs        | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/weighted_rrf/2src_500items_top10`  | 64.644 µs        | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/weighted_rrf/2src_500items_top100` | 64.614 µs        | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/weighted_rrf/2src_500items_top50`  | 64.619 µs        | 1.005     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/weighted_rrf/3src_500items_top10`  | 95.120 µs        | 1.010     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/weighted_rrf/3src_500items_top100` | 95.350 µs        | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fuse_dispatcher/weighted_rrf/3src_500items_top50`  | 95.322 µs        | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `rrf/2src/150items`                                 | 14.292 µs        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `rrf/2src/500items`                                 | 49.295 µs        | 1.008     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `rrf/2src/50items`                                  | 4.584 µs         | 1.019     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `rrf/3src/150items`                                 | 20.882 µs        | 1.007     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `rrf/3src/500items`                                 | 71.222 µs        | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `union/2src/150items`                               | 8.975 µs         | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `union/2src/500items`                               | 32.096 µs        | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `union/2src/50items`                                | 2.841 µs         | 1.004     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `union/3src/150items`                               | 13.259 µs        | 1.004     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `union/3src/500items`                               | 46.344 µs        | 1.007     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils/is_normalized_false`                  | 2.443 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils/is_normalized_true`                   | 2.072 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils/normalize_2`                          | 15.539 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils/normalize_20`                         | 45.821 ns        | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils/normalize_3`                          | 16.970 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/finite_unnormalized/2`       | 17.234 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/finite_unnormalized/20`      | 52.248 ns        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/finite_unnormalized/3`       | 18.929 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/infinity_last/2`             | 1.412 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/infinity_last/20`            | 11.865 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/infinity_last/3`             | 2.047 ns         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/nan_last/2`                  | 1.412 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/nan_last/20`                 | 11.864 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/nan_last/3`                  | 2.004 ns         | 1.004     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/normalized/2`                | 17.234 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/normalized/20`               | 52.521 ns        | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weight_utils_fallible/normalized/3`                | 18.928 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weighted/2src/150items`                            | 16.318 µs        | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weighted/2src/500items`                            | 56.012 µs        | 1.004     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weighted/2src/50items`                             | 5.395 µs         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weighted/3src/150items`                            | 24.457 µs        | 1.004     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `weighted/3src/500items`                            | 83.200 µs        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

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

## Weighted RRF and fallible normalization coverage

The existing `fusion_bench` target adds 18 scenarios. From `crates/`, run
`cargo bench -p khive-fusion --benches -- --quick --noplot`.

| Group                   | New scenarios                                                    | Measured input                                                                              | Baseline |
| ----------------------- | ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | -------- |
| `fuse_dispatcher`       | `weighted_rrf/{2,3}src_500items_top{10,50,100}` (6 combinations) | 2 or 3 ordered sources, 500 items each, RRF smoothing k=60, source weights [1,2] or [1,2,3] | pending  |
| `weight_utils_fallible` | `normalized/{2,3,20}`, `finite_unnormalized/{2,3,20}`            | Positive finite vectors with unit and non-unit sums                                         | pending  |
| `weight_utils_fallible` | `nan_last/{2,3,20}`, `infinity_last/{2,3,20}`                    | Non-finite final entry, forcing validation of every preceding element                       | pending  |

Fixtures reuse the existing seed-42 source generator. Its actual shared-ID count
is `floor(per_source / 3)`, or 166 of 500 items; the older "30%" description is
approximate. Private IDs are distinct across sources. Weighted RRF uses positions
in each input list, not the generated score magnitudes. Unlike weighted linear
fusion, its source weights are **not normalized**. The new setup checks exact
output length, unique IDs and descending-score/ascending-ID ordering.

`try_normalize_weights` rejects the first non-finite entry and returns its index.
A finite sum different from one is valid and is normalized, so that requested
case is named `finite_unnormalized`, not an error case. Setup checks both accepted
outputs and the exact last-index refusals. This matrix uses positive finite input
for success cases; it does not claim to cover every finite fallback policy.

New dispatcher timings keep the existing boundary: source cloning, ownership
transfer, validation, fusion and output destruction are included. The fallible
normalization timing includes validation and any output allocation/destruction;
fixture construction and assertions stay outside. Thus these are end-to-end
public-function costs, not allocation-free kernel timings. Existing scenarios
retain their original source and comparison series.

The producer for pending baselines is the qualified-host `scoring-retrieval`
component run. Record commit, toolchain, machine, command, scenario and result for
each completed row. Original historical numbers above retain their provenance.
The old >10% comment policy is a request for explanation, not an automatic timing
failure threshold for these advisory measurements.

## Performance guarantees

| Operation                | Input                       | Metric               | Bound                                        |
| ------------------------ | --------------------------- | -------------------- | -------------------------------------------- |
| `reciprocal_rank_fusion` | 2 sources × 500 items, k=60 | throughput (items/s) | Advisory trend against the recorded baseline |
| `fuse` (WeightedRrf)     | 2 sources × 500 items, k=60 | median latency       | Advisory trend against the recorded baseline |
| `normalize_weights`      | 20-element vector           | median latency       | Advisory trend against the recorded baseline |
| `try_normalize_weights`  | 20-element valid vector     | median latency       | Advisory trend against the recorded baseline |

All rows are advisory comparisons against rolling and fixed references, never
fixed percentage failures. The existing RRF target reports latency; its 2×500
item throughput can be derived as 1,000 divided by the measured operation seconds,
including the documented clone/destruction costs. No throughput observation is
invented here. The WeightedRrf row has three explicit output limits above; compare
like-for-like scenarios. No new deterministic count or allocation bound is added.
