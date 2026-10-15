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

## Inline ANN build runtime measurement

The ignored `measure_inline_ann_build_tick_gap` test measures the real
`load_and_build_from_vector_store` path on a Tokio runtime with two workers,
while a separate task ticks every 10 ms. It uses an isolated temporary database,
the existing local test embedder, and deterministic normalized 384-dimensional
vectors. It does not download an embedding model. Database creation and corpus
seeding finish before the timing window.

Run from the repository root on the machine where the result will be evaluated:

```bash
KHIVE_ANN_MEASUREMENT_ROWS=4096 cargo test --manifest-path crates/Cargo.toml \
  -p khive-pack-knowledge --lib --release \
  knowledge::vamana::tests::runtime_measurement::measure_inline_ann_build_tick_gap \
  -- --exact --ignored --nocapture --test-threads=1
```

Capture the `ANN_RUNTIME_MEASUREMENT` JSON line with the commit SHA, toolchain,
CPU, operating system, available memory, exact command, and concurrent machine
load. The test reports a 20-tick baseline, the elapsed scan-and-build time, the
number of observed ticks, and the largest gap between ticks. The measured build
is spawned onto a runtime worker because the async test body itself executes on
Tokio's `block_on` thread. The ticker records its first tick after completion
before stopping, so a stall at the end cannot disappear from the report. Its
window includes dispatch and completion notification overhead. Missed ticks are
skipped rather than replayed as a burst.

The default corpus has 4,096 rows. `KHIVE_ANN_MEASUREMENT_ROWS` accepts 1 through
32,768 rows; the upper limit represents 48 MiB of raw vector data before SQLite,
scan buffers, and index overhead. Each invocation seeds one corpus and performs
one build. There is no automatic growth or retry loop. Start at the default and,
if `exceeds_one_second` is false, increase the row count within the limit and run
again. A run that does not exceed one second is insufficient for the requested
measurement. If even the limit is too fast, record that limitation instead of
claiming the requirement was met. Resource limits bound the corpus, not runtime;
the build remains synchronous CPU work on a worker during this measurement.

The instrument makes no timing pass/fail assertion and no automatic stall
classification. Compare the recorded maximum gap with the 10 ms target and the
baseline, and repeat qualifying runs to distinguish machine noise. A small gap
only shows that this ticker stayed responsive: the second worker may service it
while the build occupies the first. The elapsed time includes the vector-store
scan, so it does not isolate graph construction time. Record those limitations
with the observed result in issue #4491 before choosing a production scheduling
change. No native result is recorded here yet.

To run the small deterministic checks for corpus bounds, vector generation, and
delayed tick accounting separately:

```bash
cargo test --manifest-path crates/Cargo.toml -p khive-pack-knowledge --lib \
  knowledge::vamana::tests::runtime_measurement:: -- --test-threads=1
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
