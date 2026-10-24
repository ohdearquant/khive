# khive-hnsw Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-hnsw`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                       | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `build/sequential_1000`         | 175.167 ms       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `build/sequential_5000`         | 1.583 s          | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `distance/cosine_384d`          | 642.676 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `distance/dot_384d`             | 25.590 ns        | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `distance/l2_384d`              | 31.782 ns        | 1.009     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n5k_k10`                | 140.218 µs       | 1.012     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n5k_k10_with_ctx`       | 139.139 µs       | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n5k_k50`                | 141.196 µs       | 1.009     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n5k_k50_with_ctx`       | 140.694 µs       | 1.021     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_context/ctx_fresh_k10`  | 140.036 µs       | 1.008     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_context/ctx_reuse_k10`  | 138.791 µs       | 1.005     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_context/new_ef200`      | 202.749 ns       | 1.017     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_context/new_ef80`       | 171.884 ns       | 1.116     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_metrics/n5k_k10_cosine` | 138.271 µs       | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_metrics/n5k_k10_dot`    | 139.282 µs       | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_metrics/n5k_k10_l2`     | 156.273 µs       | 1.026     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_quantized/n5k_k10_int8` | 205.641 µs       | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

## Benchmark Suite

Benchmarks live in `benches/hnsw_bench.rs` and are run with Criterion.

### Run Command

```bash
cd crates && cargo bench -p khive-hnsw --bench hnsw_bench
```

HTML reports are written to `target/criterion/`.

### Benchmark Groups

| Group                           | Scenario                      | What Is Measured                                  |
| ------------------------------- | ----------------------------- | ------------------------------------------------- |
| `build/sequential`              | Sequential insert (1K, 5K)    | Index construction time only (vectors pre-built)  |
| `search/n5k_kN`                 | Single-query search (k=10,50) | Per-query latency on a 5K-vector index            |
| `search/n5k_kN_with_ctx`        | Context-reuse search          | Per-query latency with reused `HnswSearchContext` |
| `search_quantized/n5k_k10_int8` | INT8 two-phase                | Per-query latency with INT8 pre-filter enabled    |
| `distance`                      | Cosine, L2, Dot at 384d       | Raw distance kernel throughput                    |
| `search_context`                | Alloc and reuse patterns      | Context allocation overhead vs reuse              |
| `search_metrics`                | Per-metric search (k=10)      | Cosine vs L2 vs Dot search latency at 5K          |

### Dataset Shape

- Dimensions: 384 (BGE-base / MiniLM-L6 profile)
- Corpus sizes: 1K and 5K random unit vectors
- Seed: 42 (reproducible)
- Query pool: 20 vectors, seed 43

### Config at Benchmark Time

Default `HnswConfig` applies unless overridden per group:

| Parameter         | Value                                        |
| ----------------- | -------------------------------------------- |
| `m`               | 20                                           |
| `m_max0`          | 40                                           |
| `ef_construction` | 200                                          |
| `ef_search`       | 80                                           |
| `dimensions`      | 384                                          |
| `metric`          | Cosine (default); L2/Dot in `search_metrics` |

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-hnsw --bench hnsw_bench`
- Dataset: random unit vectors, seed 42 (corpus), seed 43 (queries); dimensions 384; corpus
  sizes 1K and 5K
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Build

| Scenario              | Low       | Median    | High      |
| --------------------- | --------- | --------- | --------- |
| build/sequential_1000 | 169.80 ms | 170.41 ms | 171.01 ms |
| build/sequential_5000 | 1.472 s   | 1.500 s   | 1.531 s   |
| build/batch_1000      | 54.48 ms  | 54.79 ms  | 55.38 ms  |
| build/batch_5000      | 413.32 ms | 422.97 ms | 435.32 ms |

#### Search (5K corpus, 384-dim)

| Scenario                      | Low       | Median    | High      |
| ----------------------------- | --------- | --------- | --------- |
| search/n5k_k10                | 115.48 µs | 116.33 µs | 117.29 µs |
| search/n5k_k50                | 116.55 µs | 117.21 µs | 117.88 µs |
| search/n5k_k10_with_ctx       | 114.22 µs | 115.34 µs | 116.52 µs |
| search/n5k_k50_with_ctx       | 118.11 µs | 122.01 µs | 126.54 µs |
| search_quantized/n5k_k10_int8 | 155.16 µs | 157.69 µs | 161.47 µs |

#### Distance Kernels (384-dim)

| Scenario             | Low       | Median    | High      |
| -------------------- | --------- | --------- | --------- |
| distance/cosine_384d | 610.89 ns | 612.32 ns | 613.88 ns |
| distance/l2_384d     | 29.71 ns  | 29.78 ns  | 29.85 ns  |
| distance/dot_384d    | 27.93 ns  | 28.05 ns  | 28.20 ns  |

#### Search Context

| Scenario                     | Low       | Median    | High      |
| ---------------------------- | --------- | --------- | --------- |
| search_context/new_ef80      | 110.14 ns | 110.71 ns | 111.65 ns |
| search_context/new_ef200     | 190.74 ns | 193.56 ns | 197.15 ns |
| search_context/ctx_reuse_k10 | 114.74 µs | 115.30 µs | 115.85 µs |
| search_context/ctx_fresh_k10 | 116.15 µs | 116.86 µs | 117.60 µs |

#### Per-Metric Search (5K corpus, k=10)

| Scenario                      | Low       | Median    | High      |
| ----------------------------- | --------- | --------- | --------- |
| search_metrics/n5k_k10_cosine | 118.20 µs | 119.96 µs | 122.27 µs |
| search_metrics/n5k_k10_l2     | 137.90 µs | 138.54 µs | 139.10 µs |
| search_metrics/n5k_k10_dot    | 115.05 µs | 115.71 µs | 116.48 µs |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

## Lifecycle and allocation coverage

`hnsw_lifecycle` measures the public deletion, compaction, clearing and quantized
selection APIs. `hnsw_rebuild_alloc` is a separate executable with a measured
allocator; its timing includes that instrumentation and is kept separate from the
primary rebuild latency.

```bash
cd crates
cargo bench -p khive-hnsw --benches -- --quick --noplot
cargo test -p khive-hnsw --test benchmark_heap
```

The original `hnsw_bench` executable and historical observations above are unchanged
in meaning. The new groups generate their own 5,000 normalized 384-dimensional
vectors with seed 42 and default configuration (including graph seed 42). Fixtures
are initialized only when a matching Criterion scenario executes; listing or
filtering out a group does not build its corpus. No external dataset is required.

| Target / group            | Scenario                      | Measured operation                                        | New baseline |
| ------------------------- | ----------------------------- | --------------------------------------------------------- | ------------ |
| `hnsw_lifecycle`          | `delete100_n5000`             | 100 successful deletes, external IDs 0–99, as one batch   | pending      |
| `hnsw_lifecycle`          | `rebuild_n5000_tombstones500` | One rebuild after IDs 0–499 were tombstoned               | pending      |
| `hnsw_lifecycle`          | `clear_n5000`                 | One clear on a populated index                            | pending      |
| `hnsw_lifecycle`          | `set_quantized_true_n5000`    | Repeated `set_quantized(true)` calls on an existing arena | pending      |
| `hnsw_lifecycle`          | `is_quantized_n5000`          | Read the enabled state on the same populated index        | pending      |
| `hnsw_rebuild_allocation` | `n5000_tombstones500`         | Rebuild with requested-byte allocation accounting         | pending      |

For delete, rebuild and clear, each iteration clones its pre-built fixture before
starting the timer. Only the requested operation is timed. Result checks and index
destruction follow the timer; no mutated index is reused. The pre-tombstoned
fixture has exactly 500 deleted and 4,500 live nodes. Rebuild must report
5,000 before / 500 removed / 4,500 after, remove sampled deleted IDs, retain sampled
survivors and reset tombstone count. Delete must return 100 successes and leave
4,900 live nodes. Clear must remove all logical entries; it is not claimed to
release every backing allocation. Delete includes entry-point repair if one of
the selected IDs is the current entry point.

The setter is reset to false before each Criterion callback, then repeatedly set
to true in the timed loop. Thus it measures the setter call, including idempotent
repeats, rather than rebuilding an arena or a false/true toggle pair. The state
read has its own scenario. Opaque inputs/index references prevent the optimizer
from deleting these small operations. Fixture creation and reset are excluded.

Criterion's latency summaries and `estimates.json` median estimator describe
per-iteration-normalized samples. They are not individual-request p99 values.
The original search scenarios retain their existing timing method; this change
does not claim a new search p99 measurement.

### Rebuild allocation metric

The allocation target additionally prints a 32-sample report containing the median
and maximum **peak additional requested live bytes** for one rebuild, named
`hnsw_rebuild_allocation/n5000_tombstones500`. Every probe gets an independent
pre-tombstoned clone. The report runs once per selected scenario, including
`--quick`; it is not generated when that scenario is filtered out. The median is
the arithmetic mean of the two central sorted observations.

A signed thread-local ledger observes successful `System` allocations,
deallocations and resize deltas. The baseline is taken after fixture cloning;
freeing pre-existing buffers reduces the live total. The peak is the largest
live requested-byte increase above this baseline, not cumulative bytes requested,
RSS, allocator metadata, retained capacity, other threads' allocations, or the
allocator's private temporary storage during `realloc`. HNSW rebuild is synchronous
on the calling thread and this fixture has no metrics sink or background worker.
The scope excludes clone setup, output, assertions and final index destruction.

The ordinary `benchmark_heap` tests exercise the actual allocator with zeroed
allocations, growth, shrinkage, freeing an older allocation and a fresh baseline.
They check measurement integrity, not a fixed HNSW allocation or timing bound.

### New measurement provenance

| Item                                          | Value   |
| --------------------------------------------- | ------- |
| Commit                                        | pending |
| Toolchain                                     | pending |
| Machine / allocator / OS                      | pending |
| Feature flags                                 | pending |
| Per-scenario latency and allocation baselines | pending |

Record provenance with the measured values. Keep a rolling baseline and a fixed
reference for trends. Historical release values are observations, not current
results or promises. Timing and peak-allocation trends are advisory; do not turn
run-to-run noise into a fixed-percentage failure gate.

## Performance guarantees

| Operation          | Input                         | Metric                      | Bound                                        |
| ------------------ | ----------------------------- | --------------------------- | -------------------------------------------- |
| `search`           | 5K vectors, 384d, k=10        | median + p99 latency        | Advisory trend against the recorded baseline |
| `delete`           | 100 nodes from 5K index       | median latency              | Advisory trend against the recorded baseline |
| `rebuild`          | 5K index after 500 tombstones | median latency + peak alloc | Advisory trend against the recorded baseline |
| `search_quantized` | 5K, k=10, INT8                | median latency              | Advisory trend against the recorded baseline |

These are the issue's declared targets. Current baselines for new scenarios are
pending measurement. Every row is advisory; no deterministic allocation-count
bound is introduced. The existing INT8 two-phase search scenario remains the
measurement of that published claim.
