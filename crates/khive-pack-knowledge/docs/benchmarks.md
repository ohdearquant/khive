# khive-pack-knowledge — Benchmark Ledger

## Benchmark Suite

| Benchmark | File | Description |
|-----------|------|-------------|
| `knowledge_search_warm` | `tests/bench.rs` | Warm p50/p95 for `knowledge.search` across three rerank variants |

## Run Command

```bash
# Warm-latency smoke test (uses cargo test with --ignored):
cd crates
cargo test -p khive-pack-knowledge --test bench \
  benchmark_knowledge_search_warm_latency -- --ignored --nocapture
```

A Criterion benchmark target is planned in `benches/` once a stable synthetic fixture
dataset is established (see ADR-048 §Phase 3). The current test in `tests/bench.rs`
serves as an early smoke harness and produces JSON output at `/tmp/khive_bench_*.json`.

## Environment

- Toolchain: stable (as specified in workspace `rust-toolchain.toml`)
- Profile: release (`--release` recommended for benchmark runs)
- Platform: Apple M-series (primary dev), Linux x86-64 (CI)
- Embedder: `nomic-embed-text-v1.5` via lattice-embed (required for rerank variants)

## Baseline Table

| Scenario | Baseline | Date | Commit | Machine |
|----------|----------|------|--------|---------|
| `rerank=false` warm p50 | — | — | — | — |
| `rerank=false` warm p95 | — | — | — | — |
| `rerank=true` warm p50 | — | — | — | — |
| `rerank=true` warm p95 | — | — | — | — |
| default rerank warm p50 | — | — | — | — |
| default rerank warm p95 | — | — | — | — |

Baselines are populated after the first formal Criterion benchmark run on stable hardware.
Until then, the smoke test in `tests/bench.rs` serves as latency evidence.

## Accepted Regressions

None defined yet. A p50 regression gate of +20% will be introduced alongside the Criterion
benchmark in a follow-up PR.
