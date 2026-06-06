# khive-hnsw Benchmark Ledger

## Benchmark Suite

Benchmarks live in `benches/hnsw_bench.rs` and are run with Criterion.

### Run Command

```bash
cargo bench -p khive-hnsw --bench hnsw_bench
```

HTML reports are written to `target/criterion/`.

### Benchmark Groups

| Group              | Scenario                      | What Is Measured                                |
| ------------------ | ----------------------------- | ----------------------------------------------- |
| `build/sequential` | Sequential insert (1K, 5K)    | Index construction time only (vectors pre-built) |
| `build/batch`      | `build_batch` (1K, 5K)        | Parallel batch index build time only            |
| `search/n5k_kN`    | Single-query search (k=10,50) | Per-query latency on a 5K-vector index          |
| `search/n5k_kN_with_ctx` | Context-reuse search     | Per-query latency with reused `HnswSearchContext` |
| `search_quantized/n5k_k10_int8` | INT8 two-phase  | Per-query latency with INT8 pre-filter enabled  |
| `distance`         | Cosine, L2, Dot at 384d       | Raw distance kernel throughput                  |
| `search_context`   | Alloc and reuse patterns       | Context allocation overhead vs reuse            |
| `search_metrics`   | Per-metric search (k=10)      | Cosine vs L2 vs Dot search latency at 5K        |

### Dataset Shape

- Dimensions: 384 (BGE-base / MiniLM-L6 profile)
- Corpus sizes: 1K and 5K random unit vectors
- Seed: 42 (reproducible)
- Query pool: 20 vectors, seed 43

### Environment Notes

- Run on a quiet machine (no other CPU-intensive processes)
- Pin to physical cores if possible: `taskset -c 0-3 cargo bench ...` (Linux)
- macOS: close background apps; use Release profile (default for bench)
- Rust toolchain: stable (see `rust-toolchain.toml` at workspace root if present)

### Config at Benchmark Time

Default `HnswConfig` applies unless overridden per group:

| Parameter        | Value |
| ---------------- | ----- |
| `m`              | 20    |
| `m_max0`         | 40    |
| `ef_construction`| 200   |
| `ef_search`      | 80    |
| `dimensions`     | 384   |
| `metric`         | Cosine (default); L2/Dot in `search_metrics` |

## Baseline Table

_Record results here after each release benchmark run._

| Scenario                     | Baseline (ms/iter) | Date | Commit | Machine |
| ---------------------------- | ------------------ | ---- | ------ | ------- |
| build/sequential_1000        | —                  | —    | —      | —       |
| build/sequential_5000        | —                  | —    | —      | —       |
| build/batch_1000             | —                  | —    | —      | —       |
| build/batch_5000             | —                  | —    | —      | —       |
| search/n5k_k10               | —                  | —    | —      | —       |
| search/n5k_k50               | —                  | —    | —      | —       |
| search/n5k_k10_with_ctx      | —                  | —    | —      | —       |
| search_quantized/n5k_k10_int8| —                  | —    | —      | —       |
| distance/cosine_384d         | —                  | —    | —      | —       |
| distance/l2_384d             | —                  | —    | —      | —       |
| distance/dot_384d            | —                  | —    | —      | —       |
