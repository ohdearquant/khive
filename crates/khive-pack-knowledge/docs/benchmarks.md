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

The historical +20% comparison for Criterion `knowledge_bench` scenarios is advisory;
no automatic percentage gate is enforced.

Last reviewed: v0.2.8 (2026-06-08)

## Generated workload coverage

These additive targets cover public dispatch with generated data. Existing targets
and historical observations above are retained. The historical +20% statement is
not an enforced gate: timing rows below are advisory trends, with qualified-host
baselines still pending. No native measurement is claimed by this addition.

| Target/group                                    | Scenarios                                                 | Timed work                                                                                                                                                                     | Baseline |
| ----------------------------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------- |
| `knowledge_workloads / knowledge_compose`       | 100 atoms / 10 domains; 1000 / 20; `max_tokens=2000`      | Pinned-atom retrieval, scoring and bounded Markdown assembly; actual-call p50/p95 and estimated rendered tokens/sec                                                            | pending  |
| `knowledge_workloads / knowledge_suggest`       | The same two atom/domain corpora                          | Domain retrieval and actual member-cost aggregation; actual-call p50/p95                                                                                                       | pending  |
| `knowledge_workloads / knowledge_fold`          | 100/1000/10000 candidates × budget 256/2048               | Parse and select scored candidates of size 32; actual-call p50/p95 versus N                                                                                                    | pending  |
| `knowledge_workloads / knowledge_search_scale`  | 100/1000/10000 indexed atoms, limit 20                    | Hybrid lexical/ANN retrieval and stored-vector reranking; actual-call p50/p95                                                                                                  | pending  |
| `knowledge_workloads / knowledge_search_matrix` | 1000 atoms, half draft; alpha 0/0.5/1 × drafts false/true | Actual status filtering and embedding-score blending; actual-call p50/p95                                                                                                      | pending  |
| `knowledge_index_build`                         | 1000/10000 atoms; 100000 explicitly opt-in                | One cold full index dispatch per size, including synthetic embedding, vector writes, corpus scan, ANN build and publication; reducible elapsed time and diagnostic vectors/sec | pending  |

The Criterion target contains 19 scenarios. Each reports 21 actual individual
dispatch durations, with nearest-rank p50/p95 (ranks 11 and 20), then separately
runs Criterion's adaptive measurements. Parameter construction, fixture seeding,
warm-up, result validation and response destruction are outside each returned
duration. The timer includes synchronous `block_on`/dispatch overhead. Baselines
must use the same timer and fixture regime; these are not isolated inner-function
costs. Compose's token rate sums `ceil(markdown.len()/4)` over the 21 calls and
divides by their summed durations. This follows the implementation's UTF-8 byte
budget unit, not an actual tokenizer. Pinned atoms do not exercise auto-compose's
KG blend; domains are seeded and measured separately by suggest.

Every corpus is an isolated private file database with a queued WAL writer. The
registry has KG/knowledge packs and the same optional-audit-sink composition as
the older benchmark; it does not include persistent dispatch-audit overhead.
No model is configured for compose/suggest/fold. Search and index use a registered
deterministic normalized 384-dimensional synthetic provider, with no downloads.
Draft and reviewed vectors occupy distinct neighborhoods; this tests execution
and filtering, not real-model semantic quality. Search setup fully indexes its
corpus before timing. Search checks 20 unique real atoms, returned status and
successful stored-vector reranking without fallback or degraded responses. ANN
provenance is required for the all-reviewed scale/index corpora and the matrix
cells that include drafts. When the mixed corpus excludes drafts, filtering and
final ranking can remove the query-nearest ANN candidates from the returned top
20; a result without ANN provenance must retain lexical provenance on every row.
Suggest rejects missing member costs; fold verifies exact selected
cardinality and summed budget. Writer shutdown is joined before directory removal.

Text, slugs and vectors are deterministic; public upsert assigns fresh UUIDs and
timestamps. ANN subject ordering can therefore vary between fresh corpora. Record
that limitation when comparing builds rather than assuming identical graph layout.

The cold-index target does not use adaptive repetitions or report percentiles.
It creates and seeds a fresh corpus before the timer and validates all indexed
counts, ANN publication and an actual ANN-backed search afterward. “Cold” means
no preexisting corpus vectors or ANN in that fixture, not a flushed OS page
cache. The duration includes synthetic embedding and storage work, so it is not
pure graph-construction time. Publication and post-build read validation are
required; a zero-vector or failed build cannot produce a successful report.

```bash
cd crates
cargo bench -p khive-pack-knowledge --bench knowledge_workloads -- --quick --noplot
cargo bench -p khive-pack-knowledge --bench knowledge_index_build -- --quick --noplot
# Bounded custom-harness smoke: one 1000-atom build, then Success.
cargo test -p khive-pack-knowledge --bench knowledge_index_build
# Explicit reference measurement only; exclude this setting from scheduled CI.
KHIVE_BENCH_KNOWLEDGE_100K=1 cargo bench -p khive-pack-knowledge --bench knowledge_index_build
```

The target lists `knowledge_index_build/atoms1000` and
`knowledge_index_build/atoms10000` with `--list`, including Cargo
`--bench --list`, before executor, database or corpus setup. Each full run
performs one fresh build per size and emits one stdout row per case:

```text
BENCH-JSON {"id":"knowledge_index_build/atoms1000","statistic":"median_ns","value":123,"n":1}
```

The value above is illustrative. Each measured value is the original complete
index dispatch duration in nanoseconds. With `n=1`, the median is that one
observation, not a percentile estimate. Setup, post-build search validation and
joined writer teardown stay outside the timer; diagnostic elapsed milliseconds,
vectors/sec and corpus details go to stderr. Compare two full runs at the same
commit and environment before admitting a reference. This custom target does
not write Criterion estimates or baseline files.

Without `--bench`, or with `--test`, the target runs the 1000-atom case once and
prints `Success` after validation and teardown. Accepted flags such as `--quick`
and `--noplot` do not change the fixed corpus sizes. `KHIVE_BENCH_KNOWLEDGE_100K=1`
adds `knowledge_index_build/atoms100000` only outside test mode; it is excluded
from the default reference cohort. `--regression-case=report-contract` emits
known protocol values without runtime work; those values are not measurements.

The existing inline tick-gap measurement remains an ignored internal test.
Its measurement module is `cfg(test)` and its full-build helper is `pub(crate)`;
an external benchmark cannot call that instrument through the current public
API. No visibility is widened. `knowledge_index_build` measures the public full
index operation and does not claim to reproduce the private ticker measurement.
The historical `search_latency` and tick-gap ledger entries still have no new
measurement. Record commit, toolchain, machine, command, environment and concurrent
load alongside each qualified-host baseline. Workflow selection is separate work.

## Performance guarantees

| Operation                           | Input                         | Metric         | Bound                                                                      |
| ----------------------------------- | ----------------------------- | -------------- | -------------------------------------------------------------------------- |
| `knowledge_search_fts/rerank_false` | 50-atom corpus, limit default | median latency | Advisory trend; last recorded value 432.5 µs (`docs/benchmarks.md` v0.2.8) |
| `knowledge_list/corpus/100`         | 100 atoms                     | median latency | Advisory trend; last recorded value 213.2 µs (`docs/benchmarks.md` v0.2.8) |
| `knowledge_upsert_atoms/atoms/50`   | 50 atoms                      | median latency | Advisory trend; last recorded value 11.89 ms (`docs/benchmarks.md` v0.2.8) |
| `knowledge_get/by_slug`             | 1 atom                        | median latency | Advisory trend; last recorded value 29.35 µs (`docs/benchmarks.md` v0.2.8) |
| `knowledge_search_scale/atoms10000` | 10 000 atoms, limit 20        | median latency | Advisory trend against the recorded baseline                               |
| `knowledge_index_build/atoms10000`  | 10 000 atoms cold build       | build time     | Advisory trend against the recorded baseline                               |

These rows add no deterministic performance bound or automatic percentage gate.
Historical values are observations. New rolling and fixed-reference comparisons
await qualified-host measurements; all new baseline cells remain pending.
