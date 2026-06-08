# khive-hnsw Benchmark Ledger

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
| `build/batch`                   | `build_batch` (1K, 5K)        | Parallel batch index build time only              |
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
