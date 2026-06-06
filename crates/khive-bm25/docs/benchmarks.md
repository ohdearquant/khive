# khive-bm25 Benchmarks

## Suite

All benchmarks live in `benches/bm25_bench.rs` (Criterion harness).

| Group                   | Scenarios                       | What it measures                          |
| ----------------------- | ------------------------------- | ----------------------------------------- |
| `index_document`        | 100 / 1K / 5K docs              | Bulk indexing throughput                  |
| `index_document_single` | 50 / 200 / 500 words            | Single-document insert into a 1K corpus   |
| `search_1k`             | 1-term through 5-term queries   | Search latency vs query length on 1K docs |
| `search_corpus_scale`   | 100 / 500 / 1K docs             | Search latency vs corpus size             |
| `search_context`        | fresh vs reused `SearchContext` | Allocation overhead of context reuse      |
| `search_topk`           | k = 1, 10, 50                   | Top-k variation on 1K corpus              |
| `memory_usage`          | 100 / 500 / 1K docs             | `memory_usage()` call cost                |
| `remove_document`       | 100 removals from 1K corpus     | Document removal throughput               |

## Running

```bash
cargo bench -p khive-bm25
```

To run a single group:

```bash
cargo bench -p khive-bm25 -- search_1k
```

HTML reports are written to `target/criterion/`.

## Environment

Results depend on:

- CPU architecture and SIMD support (AVX2, NEON)
- OS and kernel scheduler
- Available memory and cache hierarchy
- Background load during the run

Pin the CPU governor and minimize background processes for reproducible
numbers.

## Baseline

| Scenario                   | Baseline | Date       | Commit  | Machine              |
| -------------------------- | -------- | ---------- | ------- | -------------------- |
| `index_document/100`       | 5.48 ms  | 2026-06-06 | fb780c9 | Apple M-series, NEON |
| `index_document/1000`      | 51.5 ms  | 2026-06-06 | fb780c9 | Apple M-series, NEON |
| `index_document/5000`      | 282 ms   | 2026-06-06 | fb780c9 | Apple M-series, NEON |
| `index_document_single/50` | 4.24 ms  | 2026-06-06 | fb780c9 | Apple M-series, NEON |

Run `cargo bench -p khive-bm25 -- --save-baseline <name>` to capture a
baseline, then compare with `--baseline <name>`.

## Regression Policy

- **>10% regression**: requires investigation and explanation before merge.
- **>20% regression**: blocks merge until resolved or explicitly accepted.
