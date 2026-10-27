# khive-score Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-score`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                                | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ---------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `DeterministicScore/arithmetic/add`      | 1.130 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/arithmetic/div_i64`  | 1.836 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/arithmetic/mul_f64`  | 4.316 ns         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/arithmetic/mul_i64`  | 0.989 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/arithmetic/sub`      | 1.130 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/from_f32/batch_1000` | 6.678 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/from_f32/scalar`     | 4.505 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/from_f64/batch_1000` | 6.321 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/from_f64/nan`        | 0.638 ns         | 1.020     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/from_f64/pos_inf`    | 0.708 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/from_f64/scalar`     | 4.180 ns         | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/to_f64/batch_1000`   | 841.047 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `DeterministicScore/to_f64/scalar`       | 0.565 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `comparator/cmp_desc_scalar`             | 1.412 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `comparator/ranked_heap_1000`            | 3.209 µs         | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `comparator/sort_1000_pairs`             | 12.903 µs        | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/avg_scores/10`                      | 7.910 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/avg_scores/100`                     | 36.739 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/avg_scores/1000`                    | 349.152 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/max_min/max_1000`                   | 651.839 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/max_min/min_1000`                   | 651.823 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/rrf_score/batch_1000_k60`           | 5.686 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/rrf_score/rank_1_k60`               | 5.754 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/sum_scores/10`                      | 4.144 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/sum_scores/100`                     | 32.548 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/sum_scores/1000`                    | 342.490 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/weighted_sum/2`                     | 12.982 ns        | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/weighted_sum/32`                    | 175.029 ns       | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ops/weighted_sum/8`                     | 45.185 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `score_from_distance/cosine_batch_1000`  | 6.889 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `score_from_distance/cosine_scalar`      | 5.174 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `score_from_distance/dot_batch_1000`     | 6.410 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `score_from_distance/dot_scalar`         | 4.303 ns         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `score_from_distance/l2_batch_1000`      | 8.200 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `score_from_distance/l2_scalar`          | 6.885 ns         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

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

## CI observation, 2026-10-27 (UTC)

The “Current baseline status” section above predates this observation: the rows below supply measured quick-mode trends for the requested families. Qualified-host/A/A calibration for the new workloads remains pending. The existing generated fixed-reference/A/A block and release ledger remain unchanged; these observations imply no cross-machine improvement, scaling result or fixed latency guarantee.

Provenance: [run 40430486230](https://github.com/ohdearquant/khive/actions/runs/40430486230), attempt 1, [scoring-retrieval job 121947482310](https://github.com/ohdearquant/khive/actions/runs/40430486230/job/121947482310), [artifact 12702217093](https://github.com/ohdearquant/khive/actions/runs/40430486230/artifacts/12702217093). Measured checkout: synthetic merge [a72ad862cbfc57b6063fa26a1d7dfcebd0749c91](https://github.com/ohdearquant/khive/commit/a72ad862cbfc57b6063fa26a1d7dfcebd0749c91), ref `6184/merge`, tree `ae5b6ef2b3fde51324783e963302fff4b4d1c8fe` (equal to integration head `a8aaafaf104d76f3dd67a3232246b94900744efe`). The score sources and manifest match dev `f7524e536512c6a57b039933be034bbc5b75c4d3`. Artifact ZIP SHA-256: `2879796e358155b3e1d6e90ae30356da5a93f89d724d820a80f73a5a989a5544`.

Host: Linux x86_64, `ubuntu-24.04` runner image, 4 logical CPUs, runner `GitHub Actions 1000462429`. Actual toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`, `x86_64-unknown-linux-gnu`; optimized `bench` profile, `CARGO_INCREMENTAL=0`. CPU model, exhaustive compiler flags and resolved feature graph were not captured. The checked-in `crates/Cargo.lock` Git blob is `c5544b62827ecac6464b5eaa4a66e799554f4de0`; no separate runtime lockfile SHA-256 was captured.

From `crates`, the component successfully compiled its bench targets, then ran:

```text
cargo bench -p khive-score -p khive-fusion -p khive-retrieval -p khive-text -p khive-bm25 --benches -- --quick --noplot
```

Each value is the archived Criterion `new/estimates.json` median point estimate in nanoseconds per benchmark iteration. All 37 selected `sample.json` files have `sampling_mode: "Linear"` and exactly two `iters`/`times` entries: these are actual two-sample quick observations, not the configured 20-sample arithmetic baseline or eleven individually timed operations. Batch IDs measure the entire batch; the workload boundaries above apply. Digits reproduce the archived point estimates, not a claim of measurement precision.

| Exact Criterion source ID                | Observed median (ns) |
| ---------------------------------------- | -------------------- |
| `ops/weighted_sum/32`                    | 210.68160438537598   |
| `score_from_distance/cosine_scalar`      | 6.183337330818176    |
| `strict_cosine/valid`                    | 7.041262447834015    |
| `strict_cosine/below_range`              | 2.530705213546753    |
| `strict_cosine/above_range`              | 2.7077079713344574   |
| `strict_cosine/nan`                      | 1.992587685585022    |
| `strict_cosine/infinity`                 | 1.9863653630018234   |
| `rrf_one_based/rank_50_k60`              | 5.449764281511307    |
| `rrf_one_based/ranks_1_100_k60`          | 571.7925415039062    |
| `decay_apply/exponential_rate/100`       | 1315.2475509643555   |
| `decay_apply/exponential_rate/1000`      | 10964.87255859375    |
| `decay_apply/hyperbolic_rate/100`        | 851.7629547119141    |
| `decay_apply/hyperbolic_rate/1000`       | 5913.924285888672    |
| `decay_apply/exponential_half_life/100`  | 1210.2587509155273   |
| `decay_apply/exponential_half_life/1000` | 9730.628601074219    |
| `decay_apply/hyperbolic_half_life/100`   | 827.5621643066406    |
| `decay_apply/hyperbolic_half_life/1000`  | 5642.722839355469    |
| `decay_apply/power_law/100`              | 833.8719177246094    |
| `decay_apply/power_law/1000`             | 5916.452850341797    |
| `decay_apply/none/100`                   | 685.5620880126953    |
| `decay_apply/none/1000`                  | 4929.419006347656    |
| `min_max_band/f64/100`                   | 582.7093276977539    |
| `min_max_band/f64/1000`                  | 3961.0382690429688   |
| `min_max_band/f32/100`                   | 555.3947067260742    |
| `min_max_band/f32/1000`                  | 3388.302032470703    |
| `weight_validation/ordered_valid/2`      | 10.482904314994812   |
| `weight_validation/ordered_valid/8`      | 17.973282784223557   |
| `weight_validation/ordered_valid/32`     | 50.37022441625595    |
| `weight_validation/ordered_invalid/2`    | 23.82474994659424    |
| `weight_validation/ordered_invalid/8`    | 30.410592555999756   |
| `weight_validation/ordered_invalid/32`   | 61.69587969779968    |
| `weight_validation/named_valid/2`        | 19.77314519882202    |
| `weight_validation/named_valid/8`        | 54.95233511924744    |
| `weight_validation/named_valid/32`       | 522.7974219322205    |
| `weight_validation/named_invalid/2`      | 26.112743854522705   |
| `weight_validation/named_invalid/8`      | 118.2883026599884    |
| `weight_validation/named_invalid/32`     | 246.348130941391     |

The 35 `arithmetic_paths` scenes appear once each, alongside the existing weighted-sum and lossy-cosine anchors. Directory/metric alias `ops_weighted_sum/32` maps to source ID `ops/weighted_sum/32`; the rows use source IDs, not escaped directory names. All 37 map to distinct metric keys and one benchmark start each in the job log; no score-selection collision was observed.

The archive's `bench-data/components.jsonl` schema-v2 record is `status=ok`, `gate_status=pass`, with the same point estimates. Its `timestamp` (`2026-10-26T21:05:33-04:00`) is the measured commit date, not the execution date; the job ran on 2026-10-27 UTC. This single quick CI observation does not replace a qualified baseline or establish A/A repeatability.
