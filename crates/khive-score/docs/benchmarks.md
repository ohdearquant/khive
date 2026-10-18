# khive-score Benchmark Ledger

**Bench targets:** `score_ops` and `arithmetic_paths` under `benches/`.
**Harness:** Criterion (`harness = false` in `Cargo.toml`).

## Run commands

A qualified-host baseline runs every scenario with its target's own Criterion
configuration and saves the results under a named Criterion baseline:

```bash
cd crates
cargo bench -p khive-score --benches -- --noplot --save-baseline <name>
```

The pull-request trend lane runs the same targets in Criterion's quick mode:

```bash
cd crates
cargo bench -p khive-score --benches -- --quick --noplot
```

Quick mode skips the warm-up and ignores the configured sample count. It doubles
the iteration count until two consecutive timings agree within the significance
level after at least 100 ms, or until the measurement time runs out, and keeps
only those two timings, so its median and mean rest on two points. Quick-mode
results are trend observations only and never enter a qualified baseline.

Results land under `target/criterion/`, one directory per Criterion ID, and a
target's filename is not a Criterion group. Criterion replaces each `/` inside a
group name with `_`: scenario `10` of group `ops/sum_scores` is written to
`target/criterion/ops_sum_scores/10/`. Record the median per-operation estimate
from each scenario's `<name>/estimates.json` from a baseline run, with its commit,
toolchain, features, machine and command. Every run rewrites `new/` and, by
default, copies it to `base/`, so a later run in the same target directory,
including a quick-mode run, replaces both; the named baseline is left alone.
These are Criterion sample-based estimates, not an empirical percentile of
individually timed calls. The pure scalar paths use Criterion's iteration timer
to avoid adding an `Instant` measurement around each nanosecond operation.

## Benchmark suite

Join the group and scenario columns with `/` to obtain the exact Criterion ID.
Comma-separated scenarios and brace alternatives each denote separate IDs.
`score_ops` is unchanged; historical release rows below retain their original
names and measurements and are not the current scenario inventory.

| Target             | Group                           | Scenarios                                                                                                                                                                     | What it measures                               |
| ------------------ | ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- |
| `score_ops`        | `DeterministicScore/from_f64`   | `scalar`, `batch_1000`, `nan`, `pos_inf`                                                                                                                                      | Fixed-point construction                       |
| `score_ops`        | `DeterministicScore/from_f32`   | `scalar`, `batch_1000`                                                                                                                                                        | Fixed-point construction                       |
| `score_ops`        | `DeterministicScore/to_f64`     | `scalar`, `batch_1000`                                                                                                                                                        | Floating conversion                            |
| `score_ops`        | `ops/sum_scores`                | `10`, `100`, `1000`                                                                                                                                                           | Saturating sum                                 |
| `score_ops`        | `ops/avg_scores`                | `10`, `100`, `1000`                                                                                                                                                           | Mean                                           |
| `score_ops`        | `ops/max_min`                   | `max_1000`, `min_1000`                                                                                                                                                        | Extrema                                        |
| `score_ops`        | `ops/rrf_score`                 | `rank_1_k60`, `batch_1000_k60`                                                                                                                                                | Legacy one-based RRF                           |
| `score_ops`        | `ops/weighted_sum`              | `2`, `8`, `32`                                                                                                                                                                | Weighted aggregation                           |
| `score_ops`        | `score_from_distance`           | `cosine_scalar`, `l2_scalar`, `dot_scalar`, `cosine_batch_1000`, `l2_batch_1000`, `dot_batch_1000`                                                                            | Lossy metric conversion                        |
| `score_ops`        | `comparator`                    | `cmp_desc_scalar`, `sort_1000_pairs`, `ranked_heap_1000`                                                                                                                      | Comparison, sorting and heap                   |
| `score_ops`        | `DeterministicScore/arithmetic` | `add`, `sub`, `mul_i64`, `mul_f64`, `div_i64`                                                                                                                                 | Fixed-point arithmetic                         |
| `arithmetic_paths` | `strict_cosine`                 | `valid`, `below_range`, `above_range`, `nan`, `infinity`                                                                                                                      | Strict cosine success and four error inputs    |
| `arithmetic_paths` | `rrf_one_based`                 | `rank_50_k60`, `ranks_1_100_k60`                                                                                                                                              | Typed one-based RRF, scalar and 100-rank batch |
| `arithmetic_paths` | `decay_apply`                   | `exponential_rate/{100,1000}`, `hyperbolic_rate/{100,1000}`, `exponential_half_life/{100,1000}`, `hyperbolic_half_life/{100,1000}`, `power_law/{100,1000}`, `none/{100,1000}` | Six decay variants on elapsed-time batches     |
| `arithmetic_paths` | `min_max_band`                  | `f64/{100,1000}`, `f32/{100,1000}`                                                                                                                                            | Normalization in each precision                |
| `arithmetic_paths` | `weight_validation`             | `ordered_valid/{2,8,32}`, `ordered_invalid/{2,8,32}`, `named_valid/{2,8,32}`, `named_invalid/{2,8,32}`                                                                        | Owned weight construction and validation       |

### Arithmetic workload boundaries

All inputs are generated deterministically before timing. Strict cosine uses
0.25, -0.25, 2.25, NaN and positive infinity; exact scores or error variants are
checked first. RRF uses prebuilt `NonZeroUsize` ranks and k=60. The legacy
`rrf_score` is also one-based; zero-based indexing has a separate public helper.

Decay applies value 0.75 to elapsed seconds 0 through N-1, with rate 1/16 per
second or half-life 16 seconds. Half-life construction is outside timing. No
wall-clock reads or timestamp parsing occur. Bands normalize ascending 0 through
N-1 into [0,1], with tolerance zero and `DegenerateSpan::Ceiling`. Decay formulas,
lengths, endpoints, finite ranges and monotonicity are checked before timing.
RRF batches, decay batches and bands include output allocation and destruction.

Ordered weights and named `HashMap<usize, f64>` inputs contain values 1 through N.
Invalid inputs replace the final ordered/keyed weight with -1. The named map's
hash seed and invalid-entry scan position are unspecified; it has exactly one
invalid entry. Input clones are excluded via `iter_batched`, as is destruction
of returned values. Constructor work, including dropping an owned rejected input,
is included. Valid ordered contents, a named all-key constant weighted mean and
the exact negative-weight error are checked outside timing.

In a baseline run the 35 `arithmetic_paths` scenes use 20 Criterion samples, a
one-second warm-up and a three-second measurement; command-line options override
these. `--quick` ignores the sample count and the warm-up, and the three-second
measurement time still caps each quick scene. Preflight checks
are small and run even when a group is filtered. No database, model, file corpus,
network, allocation counter or native-memory measurement is involved.

## Performance guarantees

| Operation                   | Input                  | Metric         | Bound                                        |
| --------------------------- | ---------------------- | -------------- | -------------------------------------------- |
| `weighted_sum`              | 32 scores + 32 weights | median latency | Advisory trend once its baseline is recorded |
| `score_from_distance_lossy` | cosine, scalar         | median latency | Advisory trend once its baseline is recorded |
| `rrf_score_one_based`       | k=60, rank 50          | median latency | Advisory trend once its baseline is recorded |
| `Decay` apply               | 1000 elapsed seconds   | median latency | Advisory trend once its baseline is recorded |
| `min_max_band`              | 1000-element vector    | median latency | Advisory trend once its baseline is recorded |

Timing is advisory against both a rolling and fixed reference, never a fixed
percentage gate. A historical observation is not a latency promise. Correctness
preflight assertions are separate from performance measurements.

## Current baseline status

The corrected existing-group inventory and all five new groups have a pending
qualified-host baseline. This source change provides no current timings. Preserve
Criterion's median estimates per scene, and record the commit, full toolchain,
machine, features and command for both reference and candidate measurements.
The earlier release ledger remains a historical observation only.

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
