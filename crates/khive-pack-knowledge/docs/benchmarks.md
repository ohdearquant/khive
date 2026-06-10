# khive-pack-knowledge — Benchmark Ledger

## Benchmark Suite

| Benchmark               | File                         | Description                                                                                   |
| ----------------------- | ---------------------------- | --------------------------------------------------------------------------------------------- |
| `knowledge_search_warm` | `tests/bench.rs`             | Warm p50/p95 for `knowledge.search` across three rerank variants                              |
| `knowledge_bench`       | `benches/knowledge_bench.rs` | Criterion suite — learn, upsert_atoms (1/10/50), list (10/100 corpus), search FTS, stats, get |
| `search_latency`        | `benches/search_latency.rs`  | Custom harness — warm p50/p95 for rerank variants over 100-atom corpus                        |

## Run Commands

```bash
# Criterion benchmark suite (statistical, with HTML reports):
cd crates && cargo bench -p khive-pack-knowledge --bench knowledge_bench

# Criterion test mode (compile + single iteration, no timing):
cd crates && cargo bench -p khive-pack-knowledge --bench knowledge_bench -- --test

# Custom search-latency harness (prints JSON to /tmp/issue_595_latencies.json):
cd crates && cargo bench -p khive-pack-knowledge --bench search_latency

# Warm-latency smoke test (uses cargo test with --ignored):
cargo test -p khive-pack-knowledge --test bench \
  benchmark_knowledge_search_warm_latency -- --ignored --nocapture
```

## Environment

- Toolchain: stable (as specified in workspace `rust-toolchain.toml`)
- Profile: release (`--release` recommended for benchmark runs)
- Platform: Apple M-series (primary dev), Linux x86-64 (CI)
- Embedder: `nomic-embed-text-v1.5` via lattice-embed (required for rerank variants)

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default (FTS-only, no embedder for Criterion suite)
- Command: `cd crates && cargo bench -p khive-pack-knowledge --bench knowledge_bench`
- Dataset: in-memory SQLite; atom corpus seeded at bench setup (10 / 100 atoms); atom content
  satisfies MIN_ATOM_CONTENT_WORDS=20 requirement; fixture version: inline bench source
- vs prior: first formal release ledger entry — no prior comparable baseline

#### `knowledge_bench` (Criterion, FTS-only, no embedder, in-memory SQLite)

| Scenario                          | Low      | Median   | High     | Outliers   |
| --------------------------------- | -------- | -------- | -------- | ---------- |
| knowledge_learn/concept_create    | 1.030 ms | 1.368 ms | 1.774 ms | 4/50 (8%)  |
| knowledge_upsert_atoms/atoms/1    | 546.9 µs | 1.196 ms | 1.842 ms | 3/30 (10%) |
| knowledge_upsert_atoms/atoms/10   | 2.371 ms | 3.517 ms | 4.832 ms | 3/30 (10%) |
| knowledge_upsert_atoms/atoms/50   | 9.488 ms | 11.89 ms | 14.80 ms | 2/30 (7%)  |
| knowledge_list/corpus/10          | 251.6 µs | 293.9 µs | 340.3 µs | —          |
| knowledge_list/corpus/100         | 191.5 µs | 213.2 µs | 244.6 µs | 3/50 (6%)  |
| knowledge_search_fts/rerank_false | 378.1 µs | 432.5 µs | 499.6 µs | 5/50 (10%) |
| knowledge_stats/stats_query       | 41.13 µs | 46.28 µs | 53.59 µs | 8/50 (16%) |
| knowledge_get/by_slug             | 28.47 µs | 29.35 µs | 30.65 µs | 3/50 (6%)  |

#### `search_latency` (custom harness, warm p50/p95)

Baselines from the custom harness are populated separately via:

```bash
cd crates && cargo bench -p khive-pack-knowledge --bench search_latency
```

Numbers not yet recorded in this ledger entry; will be added on next full bench run.

- Notes: none

## Accepted Regressions

A p50 regression gate of +20% applies to the Criterion `knowledge_bench` scenarios.

Last reviewed: v0.2.8 (2026-06-08)
