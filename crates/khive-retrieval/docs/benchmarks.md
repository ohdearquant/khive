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

## Persistence and lifecycle coverage

The preceding release ledger is historical. The following suite adds generated workloads;
its measurements and execution metadata are pending. Its timing guarantees are advisory
trends, not the fixed-percentage rule in the historical regression notes.

| Group             | Scenario names                                                                 | Timed work and input                                                                       | Baseline |
| ----------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------ | -------- |
| `materialization` | `limits_500`, `drop_counts_500`, `prefix_500`                                  | Validate limits; inspect precomputed drop counters; materialize all 500 candidates         | pending  |
| `orchestration`   | `dual_index_2x500`, `two_stage_fusion_2x500_plus_text500`                      | Primary/legacy RRF; engine RRF followed by vector/text RRF, k=60 in each stage             | pending  |
| `persistence`     | `bm25/new_save_load_500`, `hnsw/new_save_load_500`                             | Construct the persistence wrapper, serialize/save/load a 500-item index                    | pending  |
| `persistence`     | `bm25/namespace_replace_save_load_500`, `hnsw/namespace_replace_save_load_500` | Replace a different 500-item snapshot in one namespace and reload it                       | pending  |
| `persistence`     | `bm25/namespace_shadow_500`, `hnsw/namespace_shadow_500`                       | Save and fully sampled shadow load/validation of 500 items                                 | pending  |
| `ann_registry`    | `register_raise_compact/100`, `register_raise_compact/500`                     | Execute register + activation for every consumer, then compact a 100/500-entry log         | pending  |
| `ann_registry`    | `recover_raise_compact/100`, `recover_raise_compact/500`                       | Execute recovery fence + recovery-authorized raise for every active consumer, then compact | pending  |
| `ann_registry`    | `compact_only/100`, `compact_only/500`                                         | Execute only compaction with all consumers already active at the midpoint                  | pending  |

### Fixture and timing protocol

In ordinary modes, these three targets construct runtimes and generated corpora only
after Criterion selects a scene. `--list` and filters matching no scene do not build
indexes, materialize results or open databases. Selected correctness runs still execute
their full workload and checks; the limits-only scene needs no generated fixture.
Selected scenes reuse their target's initialized inputs. Initialization remains outside
recorded operation intervals. Profiling (`--bench --profile-time`, including the `=value`
form) preserves eager initialization before each group, including for unmatched filters,
so fixture construction stays outside the external profiler's callback. Criterion rejects
`--list` combined with `--profile-time`; options after `--` are literal filter text.

- HNSW uses 500 vectors of 384 dimensions, an LCG corpus seed of 42 and HNSW level seed 42,
  fixed insertion order and deterministic IDs. BM25 indexes 500 generated documents. The
  old/sentinel corpus uses seed 43 and disjoint IDs, also with 500 items. No files or models
  are downloaded and no optional fixture is silently skipped.
- Each persistence iteration creates one private WAL SQLite database with two readers and
  a private volume-lock directory, using the existing test-support constructor. Opening,
  migrations, starting the writer, generating indexes, prior/sentinel writes and checking
  exact restored content are outside timing. `RetrievalPersistence::new` wraps an existing
  SQL capability; its scenario does not measure database opening. Replacement verifies the
  sentinel namespace and exactly one snapshot row per namespace/index kind.
- Shadow validation uses `sample_rate=1`. An outer successful result alone is insufficient:
  the benchmark also requires a present, passed validation, exact counts and no discrepancies.
  The wrapper performs its load inside timing; an additional exact-content check is untimed.
  No distinct shadow-index or namespace-rotation API exists: replacement is the measured operation.
  Private generic persistence and validator helpers are reached through public wrappers.
- All persistence owners and reader leases are dropped before taking/awaiting the writer task
  exit and deleting the directory. Cleanup is untimed and failures are fatal. Iterations are
  sequential, including Criterion warmup, with one working database at a time. One current-thread
  Tokio runtime with at most two blocking threads is reused for each storage target.
- ANN uses a fresh private in-memory database per iteration, with N log entries and N consumers
  for N=100/500. Each consumer raises to N/2; exactly N/2 rows are deleted and the upper half
  remains. Every mutation must affect its intended row. Recovery starts from active watermark 0.
  Schema/log creation and recovery/compaction-only registration are untimed. The timed region
  includes public statement construction, writer admission and SQL execution, not just SQL builders.
  Compaction-only excludes register/raise. A forced memory write-queue override fails loudly;
  run with `KHIVE_WRITE_QUEUE` unset or disabled. No durable pending-age retirement is measured.
- Materialization has 500 unique ordered candidates, batch size 64, output limit 500 and
  diagnostic cap 64. Missing every fifth row and filtering the next yields 300 accepted rows
  and 100 drops of each reason. `count`/`total` are `DropCounts` methods, not limits methods:
  the counter scene reads already-computed counters (total sums 32 slots), not 500 rows.
- Dual-index fusion uses two ranked 500-item sources with 250 shared IDs, returning 500 unique
  results. Two-stage fusion adds 500 text hits, preserves the full 1,000-ID union and has no
  downstream truncation. It measures fusion, not model reranking. Vec cloning and output
  identity/order checks are untimed; materialization and two-stage timings include `block_on`.
- Small limits/counter scenes use Criterion's ordinary loop. Other scenes use sequential custom
  loops summing individual operation intervals, with preparation, result checks/drop and cleanup
  excluded. Criterion reports statistics over its samples; these are timing trends, not request
  service-level percentiles. Persistence and registry groups use sample size 10 (quick mode may
  choose its own sampling). No timing result is a correctness gate.

### Execution and baseline metadata

```sh
cd crates
cargo bench -p khive-retrieval --features persist,ann --benches -- --quick --noplot
```

`retrieval_lifecycle` uses default features. `retrieval_persistence` requires `persist` and
`retrieval_ann_registry` requires `ann`; Cargo intentionally omits these optional targets
without those features. The default-feature command alone cannot exercise the full suite.
Existing `fusion_bench` remains registered and unchanged. Workflow wiring is handled separately.

| Metadata                | New suite                            |
| ----------------------- | ------------------------------------ |
| Measured commit         | pending                              |
| Toolchain/profile       | pending / Criterion release          |
| Machine, OS, load       | pending                              |
| Feature flags           | `persist,ann`                        |
| Median timing baselines | pending                              |
| Comparison references   | pending rolling and fixed references |

## Performance guarantees

| Operation                        | Input                     | Metric         | Bound                                        |
| -------------------------------- | ------------------------- | -------------- | -------------------------------------------- |
| `fuse_search_results`            | 2 sources × 500, RRF k=60 | median latency | Advisory trend against the recorded baseline |
| `filter_by_policy`               | 500 results               | median latency | Advisory trend against the recorded baseline |
| `RetrievalPersistence` save+load | 500-item snapshot         | median latency | Advisory trend against the recorded baseline |
| `pathless_compact_log`           | 500 entries               | median latency | Advisory trend against the recorded baseline |
| `dual_index` fusion              | 2 sources × 500           | median latency | Advisory trend against the recorded baseline |

These timing rows are advisory trends against recorded rolling and fixed references. Historical
values are observations, not promises. The existing fusion/policy scenes supply the first two
rows; `persistence/*/new_save_load_500`, `ann_registry/compact_only/500` and
`orchestration/dual_index_2x500` supply the remaining rows. No new deterministic allocation,
count or quality guarantee is introduced. The benchmark assertions establish valid fixtures;
ordinary correctness tests and any future guarantee checks remain separate.
