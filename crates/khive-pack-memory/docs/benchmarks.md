# khive-pack-memory Benchmark Ledger

## Benchmark Inventory

| Name | File | Purpose |
|------|------|---------|
| `e2e_recall` | `benches/e2e_recall.rs` | End-to-end recall latency across FTS-gather and fusion strategies using a stripped real-corpus DB fixture |
| `fts_gather` | `benches/fts_gather.rs` | Latency and quality (recall@10, candidate-pool recall) of the FTS candidate-gather leg across term-selection and gather-mode configurations |

## Run Commands

```bash
# End-to-end recall benchmark (requires tests/fixtures/bench.db)
cargo bench -p khive-pack-memory --bench e2e_recall

# FTS gather benchmark (requires tests/fixtures/memory_corpus_local.jsonl)
# Extract fixture first (read-only, never mutates the source DB):
sqlite3 "file:$HOME/.khive/khive-graph.db?mode=ro" \
  "PRAGMA query_only=1; SELECT json_object('id',id,'kind',kind,'title',COALESCE(name,''),'body',content) \
   FROM notes WHERE namespace='local' AND deleted_at IS NULL ORDER BY created_at;" \
  > crates/khive-pack-memory/tests/fixtures/memory_corpus_local.jsonl

cargo test -p khive-pack-memory --release bench_fts_gather_real_corpus -- --ignored --nocapture
```

## Dataset / Fixture Shape

- `tests/fixtures/bench.db`: Stripped `khive-graph.db` with notes, entities, and embeddings intact; knowledge tables removed. Read-only; git-ignored.
- `tests/fixtures/memory_corpus_local.jsonl`: ~12k local memory notes extracted from `khive-graph.db`. One JSON object per line with fields `id`, `kind`, `title`, `body`. Git-ignored.
- Candidate pool: `CANDIDATE_LIMIT = 150` per retrieval leg (matches `RecallConfig::default().candidate_limit`).
- Query sample: `N_QUERIES = 150` distinct queries; `REPEATS = 5` timed runs per (strategy, query).

## Environment Notes

- Benchmarks require a release build (`--release`) for representative latency numbers.
- Set `KHIVE_RECALL_PROFILE=1` to emit per-stage JSON timing to stderr during recall.
- FTS-gather strategies are controlled via env vars: `KHIVE_RECALL_FTS_GATHER`, `KHIVE_RECALL_FTS_TERM_K`, `KHIVE_RECALL_FTS_SELECTION`, `KHIVE_RECALL_FTS_GATHER_LIMIT`, `KHIVE_RECALL_FTS_GATHER_MULTIPLIER`, `KHIVE_RECALL_FTS_CJK_BYPASS`.
- Env mutation in benchmark setup is single-threaded (outside timed loops); see SAFETY comments in `benches/e2e_recall.rs`.

## Key Finding (fts_gather)

The FTS OR-match set is dominated by near-zero-IDF terms (English stopwords such as "for", "and", "with" match 40–57% of the corpus). Dropping them is both faster and coverage-safe. Fixed-k term selection (lowest_df / highest_idf) drops meaningful terms and loses recall, and the per-term `term_stats` round-trips cost more than the gather saves. Default remains `fts_gather.enabled = false`.

## Baseline Results

| Scenario | Baseline | Date | Commit | Machine |
|----------|----------|------|--------|---------|
| (not yet recorded) | — | — | — | — |

Results should be recorded here after each performance-relevant PR that touches the recall pipeline.
