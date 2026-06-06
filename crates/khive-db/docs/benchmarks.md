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

## Baseline

Measured on: TBD

| Benchmark                              | p50 | p95 | Notes |
| -------------------------------------- | --- | --- | ----- |
| `fts5_search/anyterm_1term`            | TBD | TBD |       |
| `fts5_search/anyterm_3terms`           | TBD | TBD |       |
| `fts5_search/plain_no_snippet`         | TBD | TBD |       |
| `fts5_search/plain_with_snippet`       | TBD | TBD |       |
| `fts5_upsert_batch/docs/1000`          | TBD | TBD |       |
| `sqlite_vec_search/top_k/10`           | TBD | TBD |       |
| `sqlite_vec_search/top_k/100`          | TBD | TBD |       |
| `sqlite_vec_insert_batch/records/1000` | TBD | TBD |       |
| `storage_backend_creation/memory`      | TBD | TBD |       |
| `storage_backend_creation/file`        | TBD | TBD |       |

## Regression policy

- Any hot-path benchmark regressing >15% vs baseline requires investigation
  before merge.
- Run `cargo bench -p khive-db --features vectors` in CI or locally before
  performance-sensitive PRs.
- Update the baseline table after hardware changes or significant
  optimizations.
