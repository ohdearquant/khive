# khive-retrieval: Benchmark Ledger

## Suite inventory

| Benchmark file            | Group                | Targets                                            | Measures                                       |
| ------------------------- | -------------------- | -------------------------------------------------- | ---------------------------------------------- |
| `benches/fusion_bench.rs` | `fuse/rrf`           | input sizes 50 / 100 / 250 / 500                   | `fuse_search_results` with RRF strategy        |
| `benches/fusion_bench.rs` | `fuse/weighted`      | input sizes 50 / 100 / 250 / 500                   | `fuse_search_results` with weighted strategy   |
| `benches/fusion_bench.rs` | `fuse/union`         | input sizes 50 / 100 / 250 / 500                   | `fuse_search_results` with union strategy      |
| `benches/fusion_bench.rs` | `fuse/three_sources` | input sizes 50 / 200 / 500                         | three-source RRF fusion                        |
| `benches/fusion_bench.rs` | `hybrid_config`      | new / builder_rrf / builder_weighted               | `HybridConfig` construction and builder chains |
| `benches/fusion_bench.rs` | `config/search`      | default / preset_vector_only / preset_keyword_only | `SearchConfig` construction                    |
| `benches/fusion_bench.rs` | `policy`             | by_policy / by_predicate                           | policy filtering over 1000-item result sets    |
| `benches/fusion_bench.rs` | `eval`               | compute_all_100 / compute_all_1000                 | retrieval eval metric computation              |

## Run command

```sh
cd crates && cargo bench -p khive-retrieval --bench fusion_bench
```

HTML reports are written to `target/criterion/`.

## Environment notes

- Benchmarks use Criterion 0.5 with `html_reports` feature.
- `fuse/rrf`, `fuse/weighted`, `fuse/union`, and `hybrid_config` use `sample_size(200)`.
- `fuse/three_sources` uses `sample_size(100)`.
- `fuse/*` groups use `iter_batched` to exclude per-iteration Vec clone cost from the timed path.
- Results depend on CPU micro-architecture (branch predictor, cache sizes). Record machine for
  cross-run comparisons.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-retrieval --bench fusion_bench`
- Dataset: synthetic scored result sets, LCG-generated; input sizes 50 / 100 / 250 / 500;
  `fuse/*` groups use `iter_batched` (clone excluded from timed path)
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Fusion (2-source)

| Scenario          | Low      | Median   | High      | Outliers     |
| ----------------- | -------- | -------- | --------- | ------------ |
| fuse/rrf/50       | 7.577 µs | 7.818 µs | 8.127 µs  | 37/200 (19%) |
| fuse/rrf/100      | 15.14 µs | 15.35 µs | 15.60 µs  | 30/200 (15%) |
| fuse/rrf/250      | 38.38 µs | 39.08 µs | 39.87 µs  | 17/200 (9%)  |
| fuse/rrf/500      | 91.68 µs | 95.74 µs | 100.75 µs | 18/200 (9%)  |
| fuse/weighted/50  | 7.806 µs | 8.313 µs | 8.902 µs  | 6/200 (3%)   |
| fuse/weighted/100 | 11.72 µs | 11.99 µs | 12.38 µs  | 26/200 (13%) |
| fuse/weighted/250 | 29.02 µs | 29.10 µs | 29.17 µs  | 9/200 (5%)   |
| fuse/weighted/500 | 60.90 µs | 63.63 µs | 66.76 µs  | 16/200 (8%)  |
| fuse/union/50     | 3.517 µs | 3.644 µs | 3.793 µs  | 25/200 (13%) |
| fuse/union/100    | 7.218 µs | 7.243 µs | 7.272 µs  | 18/200 (9%)  |
| fuse/union/250    | 18.93 µs | 19.11 µs | 19.34 µs  | 21/200 (11%) |
| fuse/union/500    | 39.11 µs | 39.62 µs | 40.28 µs  | 26/200 (13%) |

#### Fusion (3-source)

| Scenario               | Low      | Median   | High     | Outliers     |
| ---------------------- | -------- | -------- | -------- | ------------ |
| fuse/three_sources/50  | 10.78 µs | 10.90 µs | 11.07 µs | 13/100 (13%) |
| fuse/three_sources/200 | 42.84 µs | 43.54 µs | 44.48 µs | 8/100 (8%)   |
| fuse/three_sources/500 | 107.8 µs | 110.7 µs | 115.2 µs | 13/100 (13%) |

#### Config Construction

| Scenario                         | Low      | Median   | High     | Outliers    |
| -------------------------------- | -------- | -------- | -------- | ----------- |
| hybrid_config/new                | 3.685 ns | 3.750 ns | 3.827 ns | 15/200 (8%) |
| hybrid_config/builder_rrf        | 6.151 ns | 6.342 ns | 6.586 ns | 15/200 (8%) |
| hybrid_config/builder_weighted   | 21.87 ns | 22.30 ns | 22.86 ns | 9/200 (5%)  |
| hybrid_config/normalized_weights | 843.4 ps | 877.9 ps | 915.4 ps | 12/200 (6%) |
| search_config/default            | 3.631 ns | 3.750 ns | 3.896 ns | 12/200 (6%) |

- Notes: none

## Regression notes

- A regression is flagged when Criterion reports a statistically significant slowdown (>5% at p=0.05).
- Noise floor: record machine load and CPU governor state when seeding baselines.

Last reviewed: v0.2.8 (2026-06-08)
