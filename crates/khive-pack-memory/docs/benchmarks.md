# khive-pack-memory Benchmark Ledger

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
