# khive-retrieval: Benchmark Ledger

## Suite inventory

| Benchmark file | Group | Targets | Measures |
| --- | --- | --- | --- |
| `benches/fusion_bench.rs` | `fuse/rrf` | input sizes 50 / 100 / 250 / 500 | `fuse_search_results` with RRF strategy |
| `benches/fusion_bench.rs` | `fuse/weighted` | input sizes 50 / 100 / 250 / 500 | `fuse_search_results` with weighted strategy |
| `benches/fusion_bench.rs` | `fuse/union` | input sizes 50 / 100 / 250 / 500 | `fuse_search_results` with union strategy |
| `benches/fusion_bench.rs` | `fuse/three_sources` | input sizes 50 / 200 / 500 | three-source RRF fusion |
| `benches/fusion_bench.rs` | `hybrid_config` | new / builder_rrf / builder_weighted | `HybridConfig` construction and builder chains |
| `benches/fusion_bench.rs` | `config/search` | default / preset_vector_only / preset_keyword_only | `SearchConfig` construction |
| `benches/fusion_bench.rs` | `policy` | by_policy / by_predicate | policy filtering over 1000-item result sets |
| `benches/fusion_bench.rs` | `eval` | compute_all_100 / compute_all_1000 | retrieval eval metric computation |

## Run command

```sh
cargo bench --manifest-path crates/khive-retrieval/Cargo.toml --bench fusion_bench
```

HTML reports are written to `target/criterion/`.

## Environment notes

- Benchmarks use Criterion 0.5 with `html_reports` feature.
- `fuse/rrf`, `fuse/weighted`, `fuse/union`, and `hybrid_config` use `sample_size(200)`.
- `fuse/three_sources` uses `sample_size(100)`.
- Results depend on CPU micro-architecture (branch predictor, cache sizes). Record machine for cross-run comparisons.
- Clone / allocation setup inside `b.iter` is a known methodology issue (see AUD-005 in the 2026-06-06 audit). Results for `fuse/*` groups currently include vector-clone cost alongside fusion cost. Baseline entries below reflect current methodology until AUD-005 is resolved.

## Baseline table

| Scenario | Baseline (mean) | Date | Commit | Machine |
| --- | --- | --- | --- | --- |
| _(no entries yet — run benchmarks on first tagged release to seed this table)_ | — | — | — | — |

## Regression notes

- A regression is flagged when Criterion reports a statistically significant slowdown (>5% at p=0.05).
- Noise floor: record machine load and CPU governor state when seeding baselines.
