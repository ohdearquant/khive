# khive-score Benchmark Ledger

**Bench target:** `crates/khive-score/benches/score_ops.rs`
**Harness:** Criterion (`harness = false` in `Cargo.toml`)

## Run command

```bash
cd crates && cargo bench -p khive-score --bench score_ops
```

Results land in `target/criterion/score_ops/`.

## Benchmark suite

| Scenario              | What it measures                                                  |
| --------------------- | ----------------------------------------------------------------- |
| `distance_cosine`     | `score_from_distance_lossy` with cosine metric, 1000 samples      |
| `distance_l2`         | `score_from_distance_lossy` with L2 metric, 1000 samples          |
| `distance_dot`        | `score_from_distance_lossy` with dot-product metric, 1000 samples |
| `try_distance_cosine` | `try_score_from_distance` with cosine metric, valid inputs        |
| `sum_scores_100`      | `sum_scores` over 100-element slice                               |
| `avg_scores_100`      | `avg_scores` over 100-element slice                               |
| `rrf_score_k60`       | `rrf_score_one_based` at k=60, ranks 1–100                        |
| `weighted_sum_10`     | `weighted_sum` over 10 scores/weights                             |
| `ranked_heap_1000`    | `BinaryHeap<Ranked<u64>>` push+pop, 1000 items                    |

## Environment notes

- Run on a quiet machine (no background load) for reproducible results.
- Pin CPU frequency if available: `sudo cpupower frequency-set -g performance` (Linux).
- Criterion warms up for 3 seconds by default; increase via `Criterion::warm_up_time` if
  variance is high.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-score --bench score_ops`
- Dataset: inline numeric inputs generated in bench source; no external fixture; seed N/A
  (deterministic fixed inputs)
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Score Operations

| Scenario               | Low      | Median   | High     | Outliers |
| ---------------------- | -------- | -------- | -------- | -------- |
| ops/sum_scores/10      | 4.404 ns | 4.414 ns | 4.423 ns | —        |
| ops/sum_scores/100     | 34.65 ns | 34.82 ns | 35.04 ns | —        |
| ops/sum_scores/1000    | 332.8 ns | 334.0 ns | 335.2 ns | —        |
| ops/avg_scores/10      | 6.669 ns | 6.682 ns | 6.696 ns | —        |
| ops/avg_scores/100     | 35.07 ns | 35.20 ns | 35.34 ns | —        |
| ops/avg_scores/1000    | 334.8 ns | 335.6 ns | 336.5 ns | —        |
| ops/max_min/max_1000   | 146.8 ns | 147.1 ns | 147.5 ns | —        |
| ops/max_min/min_1000   | 146.7 ns | 149.7 ns | 154.3 ns | —        |
| ops/rrf_score/rank_1   | 1.795 ns | 1.800 ns | 1.808 ns | —        |
| ops/rrf_score/batch_1k | 2.465 µs | 2.471 µs | 2.476 µs | —        |
| ops/weighted_sum/2     | 5.095 ns | 5.111 ns | 5.129 ns | —        |
| ops/weighted_sum/8     | 15.93 ns | 15.97 ns | 16.01 ns | —        |
| ops/weighted_sum/32    | 58.56 ns | 59.83 ns | 61.65 ns | —        |

#### Distance-to-Score Conversion

| Scenario                      | Low      | Median   | High     |
| ----------------------------- | -------- | -------- | -------- |
| score_from_distance/cosine    | 1.524 ns | 1.529 ns | 1.536 ns |
| score_from_distance/l2        | 2.920 ns | 3.075 ns | 3.253 ns |
| score_from_distance/dot       | 1.239 ns | 1.262 ns | 1.294 ns |
| score_from_distance/cosine_1k | 3.298 µs | 3.702 µs | 4.137 µs |
| score_from_distance/l2_1k     | 4.462 µs | 4.690 µs | 4.949 µs |
| score_from_distance/dot_1k    | 2.049 µs | 2.215 µs | 2.410 µs |

#### Comparator

| Scenario                 | Low      | Median   | High     |
| ------------------------ | -------- | -------- | -------- |
| comparator/cmp_desc      | 1.373 ns | 1.419 ns | 1.472 ns |
| comparator/sort_1k_pairs | 9.957 µs | 10.14 µs | 10.36 µs |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)
