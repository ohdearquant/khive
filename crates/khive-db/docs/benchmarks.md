# khive-db Benchmarks

Performance benchmarks for the khive-db storage layer, covering FTS5 text
search, sqlite-vec vector operations, and backend creation overhead.

## Suite

Defined in `benches/db_hot_path.rs`. Three benchmark groups:

### `fts_benches` -- FTS5 text search

| Function                             | Description                                   |
| ------------------------------------ | --------------------------------------------- |
| `fts5_search/anyterm_1term`          | Single-term AnyTerm query, 10K corpus, top-20 |
| `fts5_search/anyterm_3terms`         | Three-term AnyTerm query                      |
| `fts5_search/anyterm_5terms`         | Five-term AnyTerm query                       |
| `fts5_search/plain_no_snippet`       | Plain mode, no snippet extraction             |
| `fts5_search/plain_with_snippet`     | Plain mode, 64-char snippets                  |
| `fts5_search_unranked/anyterm_top20` | Unranked gather mode                          |
| `fts5_rank_within_cap/cap/{N}`       | RankWithinCap mode, N in {50, 200, 500}       |
| `fts5_term_stats/single_term`        | Term frequency stats, 1 term                  |
| `fts5_term_stats/five_terms`         | Term frequency stats, 5 terms                 |
| `fts5_upsert_batch/docs/{N}`         | Batch upsert, N in {100, 500, 1000}           |

### `vec_benches` -- sqlite-vec vector search

| Function                              | Description                             |
| ------------------------------------- | --------------------------------------- |
| `sqlite_vec_search/top_k/{N}`         | KNN search, N in {10, 50, 100}, 384-dim |
| `sqlite_vec_insert_batch/records/{N}` | Batch insert, N in {100, 500, 1000}     |

### `backend_benches` -- StorageBackend creation

| Function                          | Description                       |
| --------------------------------- | --------------------------------- |
| `storage_backend_creation/memory` | In-memory backend instantiation   |
| `storage_backend_creation/file`   | File-backed backend instantiation |

## Running

```bash
# Full suite (requires sqlite-vec feature)
cargo bench -p khive-db --features vectors

# Single group
cargo bench -p khive-db --features vectors -- fts5_search
cargo bench -p khive-db --features vectors -- sqlite_vec
cargo bench -p khive-db --features vectors -- storage_backend
```

## Environment notes

- Corpus size: 10,000 documents / vectors (deterministic, seeded RNG)
- Vector dimensions: 384 (matches all-MiniLM-L6-v2)
- Sample size: 50 iterations (200 for backend creation)
- Benchmarks use file-backed SQLite (tempdir), not in-memory

## Baseline (2026-06-06, post-sweep)

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Machine:** arm64 (Apple Silicon), macOS Darwin 25.5.0
**Command:** `cargo bench -p khive-db --bench db_hot_path --features khive-db/vectors`

### FTS5 Search (10K corpus, top-20)

| Benchmark                              | Median    |
| -------------------------------------- | --------- |
| `fts5_search/anyterm_1term`            | 7.67 ms   |
| `fts5_search/anyterm_3terms`           | 14.87 ms  |
| `fts5_search/anyterm_5terms`           | 21.07 ms  |
| `fts5_search/plain_no_snippet`         | 11.99 ms  |
| `fts5_search/plain_with_snippet`       | 12.15 ms  |
| `fts5_search_unranked/anyterm_top20`   | 300.1 µs  |
| `fts5_rank_within_cap/cap/50`          | 23.71 ms  |
| `fts5_rank_within_cap/cap/200`         | 21.32 ms  |
| `fts5_rank_within_cap/cap/500`         | 21.20 ms  |
| `fts5_term_stats/single_term`          | 6.34 ms   |
| `fts5_term_stats/five_terms`           | 21.58 ms  |

### FTS5 Upsert

| Benchmark                     | Median    |
| ----------------------------- | --------- |
| `fts5_upsert_batch/docs/100`  | 7.19 ms   |
| `fts5_upsert_batch/docs/500`  | 51.55 ms  |
| `fts5_upsert_batch/docs/1000` | 153.69 ms |

### sqlite-vec Vector Search (10K corpus, 384-dim)

| Benchmark                              | Median   |
| -------------------------------------- | -------- |
| `sqlite_vec_search/top_k/10`           | 9.22 ms  |
| `sqlite_vec_search/top_k/50`           | 9.58 ms  |
| `sqlite_vec_search/top_k/100`          | 10.60 ms |

### sqlite-vec Batch Insert

| Benchmark                              | Median   |
| -------------------------------------- | -------- |
| `sqlite_vec_insert_batch/records/100`  | 5.94 ms  |
| `sqlite_vec_insert_batch/records/500`  | 12.58 ms |
| `sqlite_vec_insert_batch/records/1000` | 27.24 ms |

### Backend Creation

| Benchmark                         | Median    |
| --------------------------------- | --------- |
| `storage_backend_creation/memory` | 20.89 µs  |
| `storage_backend_creation/file`   | 1.08 ms   |

## Regression policy

- Any hot-path benchmark regressing >5% vs baseline requires investigation
  before merge.
- Run `cargo bench -p khive-db --features vectors` in CI or locally before
  performance-sensitive PRs.
- Update the baseline table after hardware changes or significant
  optimizations.
