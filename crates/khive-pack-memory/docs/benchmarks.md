# khive-pack-memory Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-pack-memory`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                              | Reference median | A/A ratio | Baseline commit                            | Commit date |
| -------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `recall/n_memories/10`                 | 1.130 ms         | 1.004     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `recall/n_memories/100`                | 1.461 ms         | 1.003     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `recall/n_memories/500`                | 2.609 ms         | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `recall_with_min_score/min_score_0_3`  | 1.246 ms         | 1.005     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `remember/baseline`                    | 48.779 ms        | 1.002     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `remember_with_source/with_annotation` | 1.625 ms         | 1.006     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

## Benchmark Inventory

| Name           | File                      | Purpose                                                                                                                                                                    |
| -------------- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `e2e_recall`   | `benches/e2e_recall.rs`   | End-to-end recall latency across FTS-gather and fusion strategies using a stripped real-corpus DB fixture                                                                  |
| `fts_gather`   | `benches/fts_gather.rs`   | Latency and quality (recall@10, candidate-pool recall) of the FTS candidate-gather leg across term-selection and gather-mode configurations                                |
| `memory_bench` | `benches/memory_bench.rs` | Criterion suite — `remember` baseline write, `remember_with_source` annotation path, `recall` scaling over 10/100/500 seeded memories, `recall_with_min_score` filter path |

## Run Commands

```bash
# End-to-end recall benchmark (requires tests/fixtures/bench.db)
cd crates && cargo bench -p khive-pack-memory --bench e2e_recall

# FTS gather benchmark (requires tests/fixtures/memory_corpus_local.jsonl)
# Extract fixture first (read-only, never mutates the source DB):
sqlite3 "file:$HOME/.khive/khive-graph.db?mode=ro" \
  "PRAGMA query_only=1; SELECT json_object('id',id,'kind',kind,'title',COALESCE(name,''),'body',content) \
   FROM notes WHERE namespace='local' AND deleted_at IS NULL ORDER BY created_at;" \
  > crates/khive-pack-memory/tests/fixtures/memory_corpus_local.jsonl

cargo test -p khive-pack-memory --release bench_fts_gather_real_corpus -- --ignored --nocapture

# Criterion verb-level benchmarks (no external fixture required)
cd crates && cargo bench -p khive-pack-memory --bench memory_bench
# Smoke-test mode (compile + single iteration, no timing):
cd crates && cargo bench -p khive-pack-memory --bench memory_bench -- --test
```

## Dataset / Fixture Shape

- `tests/fixtures/bench.db`: Stripped `khive-graph.db` with notes, entities, and embeddings intact;
  knowledge tables removed. Read-only; git-ignored.
- `tests/fixtures/memory_corpus_local.jsonl`: ~12k local memory notes extracted from
  `khive-graph.db`. One JSON object per line with fields `id`, `kind`, `title`, `body`. Git-ignored.
- Candidate pool: `CANDIDATE_LIMIT = 150` per retrieval leg (matches `RecallConfig::default().candidate_limit`).
- Query sample: `N_QUERIES = 150` distinct queries; `REPEATS = 5` timed runs per (strategy, query).

## Environment Notes

- Benchmarks require a release build (`--release`) for representative latency numbers.
- Set `KHIVE_RECALL_PROFILE=1` to emit per-stage JSON timing to stderr during recall.
  The `c` field identifies the recall across async worker changes and concurrent calls.
  Each profiled handler emits one `total` record on completion, including empty or
  verbose responses, errors and cancellation. Its duration covers the handler;
  stages that were never reached are omitted. A `total` record alone does not imply
  success. The environment setting is cached for the lifetime of the process.
- FTS-gather strategies are controlled via env vars: `KHIVE_RECALL_FTS_GATHER`,
  `KHIVE_RECALL_FTS_TERM_K`, `KHIVE_RECALL_FTS_SELECTION`, `KHIVE_RECALL_FTS_GATHER_LIMIT`,
  `KHIVE_RECALL_FTS_GATHER_MULTIPLIER`, `KHIVE_RECALL_FTS_CJK_BYPASS`.
- Env mutation in benchmark setup is single-threaded (outside timed loops); see SAFETY comments
  in `benches/e2e_recall.rs`.

## Key Finding (fts_gather)

The FTS OR-match set is dominated by near-zero-IDF terms (English stopwords such as "for", "and",
"with" match 40–57% of the corpus). Dropping them is both faster and coverage-safe. Fixed-k term
selection (lowest_df / highest_idf) drops meaningful terms and loses recall, and the per-term
`term_stats` round-trips cost more than the gather saves. Default remains `fts_gather.enabled = false`.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default (FTS-only, no embedder for memory_bench)
- Command: `cd crates && cargo bench -p khive-pack-memory --bench memory_bench`
- Dataset: in-memory SQLite; memory corpus seeded with 10 / 100 / 500 notes; sample size 20
- vs prior: first formal release ledger entry — no prior comparable baseline

#### `memory_bench` (Criterion, FTS-only, no embedder, in-memory SQLite)

| Scenario                             | Low      | Median   | High     | Outliers   |
| ------------------------------------ | -------- | -------- | -------- | ---------- |
| remember/baseline                    | 6.932 ms | 7.545 ms | 8.248 ms | 1/20 (5%)  |
| remember_with_source/with_annotation | 2.944 ms | 3.339 ms | 3.782 ms | —          |
| recall/n_memories/10                 | 905.1 µs | 1.014 ms | 1.129 ms | 1/20 (5%)  |
| recall/n_memories/100                | 1.013 ms | 1.122 ms | 1.270 ms | 3/20 (15%) |
| recall/n_memories/500                | 2.467 ms | 2.786 ms | 3.058 ms | —          |
| recall_with_min_score/min_score_0_3  | 780.8 µs | 866.9 µs | 995.0 µs | 1/20 (5%)  |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

## Generated lifecycle and diagnostic suite

The new Criterion targets use generated private fixtures. All new measurements below are
pending a qualified-host run of the `packs` bench-track component; the older ledger above
remains a historical observation.

| Group                | Scenarios                                                 | Measured work                                                                                                                       | Baseline                   |
| -------------------- | --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | -------------------------- |
| `memory_lifecycle`   | `feedback_fresh`                                          | One validated full-UUID feedback dispatch after untimed recall on 10 fresh memories; no brain profile, global tuning prior          | pending p50/p95            |
| `memory_lifecycle`   | `prune_500_half_low`                                      | Coherent soft deletion of 250 low-salience memories from 500; rows-pruned/sec                                                       | pending                    |
| `memory_vacuum`      | `vacuum_500_half_deleted`                                 | Top-level VACUUM over a private WAL database with 500 memories, 250 soft-deleted; wall time, page accounting and settled file bytes | pending                    |
| `memory_diagnostics` | `embed_deterministic`                                     | Warm public embedder dispatch, 384-dimensional deterministic output and full-vector JSON                                            | pending p50/p95            |
| `memory_diagnostics` | `candidates_fts_500`, `fuse_regathers_fts_500`            | FTS candidate gathering/hydration; independent gathering plus fusion, 150 candidates from 500 memories                              | pending p50/p95            |
| `memory_diagnostics` | `rerank_150_fused_candidates`                             | Weighted rerank of 150 actual fused candidates; untimed event counts verify warm and timed appends                                  | pending p50/p95            |
| `memory_diagnostics` | `score_scalar`                                            | Scalar score/breakdown dispatch with fixed finite inputs                                                                            | pending p50/p95            |
| `memory_diagnostics` | `recall_weights_off_fts_500`, `recall_weights_on_fts_500` | End-to-end FTS recall with default scoring versus supported relevance/salience weights; ratio of separately measured medians        | pending p50/p95 and factor |
| `memory_diagnostics` | `recall_full_content_fts_500`, `recall_preview_fts_500`   | Return 10 complete generated bodies versus 200-character previews plus ellipsis                                                     | pending p50/p95            |

### Measurement boundaries

Every dispatch gets its own fixture. Corpus generation, migrations, registry construction,
credential installation, query/parameter construction, setup recall and diagnostic warm call
are untimed. Normal cleanup is also untimed: registry/runtime owners drop, the file writer's
join completes, settled database bytes are read, and its temp directory is explicitly removed.
One current-thread executor (at most two blocking threads) and one working fixture at a time
bound resources, including Criterion warmup. File fixtures use WAL, two readers, an explicit
private lock directory and forced queued writes; memory fixtures require the ordinary memory
writer route. Run with `KHIVE_WRITE_QUEUE` unset/disabled so memory fixtures cannot silently
fall back after an incompatible override.

Notes use deterministic UUIDs with seed 5649, fixed generated bodies and alternating 0.1/0.8
salience. Their current creation timestamp is captured once per fixture and decay is zero.
Both typed-note and FTS batch results must confirm every intended row. Feedback resets the
pack's posterior/cache state per sample and uses a UUID returned by actual recall. Prune
skips the expiry selector (`before=0`) and requires exactly 250 deleted/250 live records.
Vacuum requires those same 500 physical note records afterwards: soft deletion does not
remove them, so neither shrinkage nor reclaimed bytes is promised. It reports logical page
accounting before/after and the physical main-file size after the writer closes; WAL bytes
are not included in that settled main-file size.

Feedback and each diagnostic print 40 independent individual-call samples before their
Criterion measurements. p50 is the mean of sorted samples 20/21; p95 is nearest rank
`ceil(0.95*40)=38`. These are actual dispatch distributions, not Criterion batch percentiles.
Criterion uses sample size 10 and sequential custom intervals excluding fixture work. Its
quick flag controls Criterion sampling, not the fixed 40-sample reports. The weighted/default
median ratio is informational and sensitive to host load; it is not a latency gate.

### Supported semantics and coverage boundaries

- `memory.feedback` accepts `target_id` with a full UUID, not `id`. This suite measures the
  no-profile fallback prior; it does not claim profile-backed brain feedback coverage.
- The five subhandlers are reached through the public Rust registry; they are not callable
  through MCP. `recall_fuse` gathers again, `recall_rerank` consumes prepared candidates,
  and `recall_score` consumes scalar features. Their independent times cannot be added into
  an exclusive pipeline total or converted into synthetic stage shares.
- One separate real `memory.recall(include_timings=true)` report prints measured retrieval
  stages divided by that same handler's `total_us`. Unentered stages stay null. Stages can
  overlap and omit other handler work, so the ratios are neither summed nor a full partition.
- `rerank:true/false` is rejected by the strict recall parameter contract. The supported
  `config.reranker_weights` comparison uses `{}` versus relevance 0.7/salience 0.3. It measures
  FTS weighted scoring, not warm-vector/hybrid overhead or a model cross-encoder. That literal
  requested warm-vector comparison remains outside these new fixtures.
- All corpus-backed new scenes are intentionally FTS-only, with no registered embedding
  models and no ANN background builds. Only the isolated embed diagnostic installs a
  deterministic public provider; it measures dispatch/provider/JSON overhead, not model
  inference quality or latency. The existing real-model `p95_gate` remains distinct.
- New targets require no ignored corpus/model fixtures and fail on unexpected inputs/results.
  Existing `e2e_recall` and `fts_gather` retain their external fixture requirements and skip
  behavior; they have not been silently counted as exercised by this generated suite. Existing
  workflow wiring and guarantee-schema changes remain separate work.

### Execution

```sh
cd crates
cargo bench -p khive-pack-memory --bench memory_lifecycle --bench memory_recall_diagnostics -- --quick --noplot
```

The old `--benches` command also runs custom harnesses with their own fixture/model setup and
argument behavior. It is not an isolated generated-suite command. Existing targets and their
inputs remain unchanged. Record the measured commit, toolchain, profile, machine, load and
command with each baseline; all those new measurement fields are currently pending.

## Historical warm p95 gate ledger

Source: [historical gate results](../benches/p95_gate_results.md). Commit `7b55beea3`, branch
`p95-bench-harness`; Apple M2 Max, macOS 27.0, arm64. The toolchain version was not recorded
there. Each configuration used file-backed WAL SQLite, 200 memories per registered model,
real embedding models, a warm-response/event-count check and 200 timed calls. Concurrent
compiler activity was present; the source records substantial variability and residual
route-observability gaps. These copied observations are not new measurements.

| Historical configuration   | p50 ms | p95 ms | p99 ms | Samples |
| -------------------------- | -----: | -----: | -----: | ------: |
| M=1 queried                |  3.964 |  4.343 |  4.903 |     200 |
| M=3 queried                |  5.961 |  6.809 |  6.940 |     200 |
| M=4 queried, informational |  6.379 | 21.801 | 33.440 |     200 |

| Current qualified-host measurement | p50 ms  | p95 ms  | p99 ms  | Measured commit/toolchain/machine |
| ---------------------------------- | ------- | ------- | ------- | --------------------------------- |
| M=1, real `BgeSmallEnV15`          | pending | pending | pending | pending                           |
| M=3, real model fan-out            | pending | pending | pending | pending                           |

Run the unchanged producer separately:

```sh
cd crates
cargo bench -p khive-pack-memory --bench p95_gate
```

`p95_gate` has 200 timed samples per configuration, rather than an unbounded timed loop, and
retains its real-model warmup, route assertions, controls and existing numeric regression
threshold. Neither that executable nor its historical result file is modified here. The
advisory table below does not disable its existing gate.

## Performance guarantees

| Operation                      | Input                             | Metric         | Bound                                                                           |
| ------------------------------ | --------------------------------- | -------------- | ------------------------------------------------------------------------------- |
| `memory.recall` warm M=1       | single `BgeSmallEnV15`, 200 iters | p95 latency    | Advisory trend; last recorded value 4.343 ms (`benches/p95_gate_results.md:95`) |
| `memory.recall` warm M=3       | three-model fan-out, 200 iters    | p95 latency    | Advisory trend; last recorded value 6.809 ms (`benches/p95_gate_results.md:96`) |
| `memory.recall` FTS-only n=500 | 500 seeded memories, limit 10     | median latency | Advisory trend against the recorded baseline                                    |
| `memory.remember` baseline     | single write                      | median latency | Advisory trend against the recorded baseline                                    |
| `bench_feedback` (new)         | 1 feedback dispatch               | median latency | Advisory trend against the recorded baseline                                    |
| `bench_vacuum` (new)           | DB with soft-deleted rows         | wall time      | Advisory trend against the recorded baseline                                    |

The requested rows are advisory trends against rolling and fixed references. Historical
numbers are observations, not promises. No new deterministic allocation/count/quality gate
is introduced by this suite. Fixture assertions establish meaningful measurements; they do
not turn throughput or latency into a fixed threshold. The earlier FTS-gather corpus finding
is retained as historical and is not revalidated by the new generated workloads.
