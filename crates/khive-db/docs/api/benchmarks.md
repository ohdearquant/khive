# khive-db Benchmarks

Performance benchmarks for the khive-db storage layer, covering FTS5 text
search, sqlite-vec vector operations, and backend creation overhead.

## Suite

Defined in `benches/db_hot_path.rs`. Three benchmark groups:

### `fts_benches` — FTS5 text search

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

### `vec_benches` — sqlite-vec vector search

| Function                              | Description                             |
| ------------------------------------- | --------------------------------------- |
| `sqlite_vec_search/top_k/{N}`         | KNN search, N in {10, 50, 100}, 384-dim |
| `sqlite_vec_insert_batch/records/{N}` | Batch insert, N in {100, 500, 1000}     |

### `backend_benches` — StorageBackend creation

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

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: `vectors`
- Command: `cd crates && cargo bench -p khive-db --bench db_hot_path --features khive-db/vectors`
- Dataset: 10,000 documents / vectors, deterministic seeded RNG; vector dimensions 384 (matches
  all-MiniLM-L6-v2); file-backed SQLite (tempdir); sample size 50 (200 for backend creation)
- vs prior: first formal release ledger entry — no prior comparable baseline

#### FTS5 Search (10K corpus, top-20)

| Benchmark                            | Low      | Median   | High     |
| ------------------------------------ | -------- | -------- | -------- |
| `fts5_search/anyterm_1term`          | 7.60 ms  | 7.67 ms  | 7.74 ms  |
| `fts5_search/anyterm_3terms`         | 14.76 ms | 14.87 ms | 15.05 ms |
| `fts5_search/anyterm_5terms`         | 20.91 ms | 21.07 ms | 21.28 ms |
| `fts5_search/plain_no_snippet`       | 11.84 ms | 11.99 ms | 12.19 ms |
| `fts5_search/plain_with_snippet`     | 12.10 ms | 12.15 ms | 12.20 ms |
| `fts5_search_unranked/anyterm_top20` | 296.7 µs | 300.1 µs | 304.0 µs |
| `fts5_rank_within_cap/cap/50`        | 23.38 ms | 23.71 ms | 24.10 ms |
| `fts5_rank_within_cap/cap/200`       | 21.13 ms | 21.32 ms | 21.54 ms |
| `fts5_rank_within_cap/cap/500`       | 21.05 ms | 21.20 ms | 21.35 ms |
| `fts5_term_stats/single_term`        | 6.27 ms  | 6.34 ms  | 6.41 ms  |
| `fts5_term_stats/five_terms`         | 21.37 ms | 21.58 ms | 21.80 ms |

#### FTS5 Upsert

| Benchmark                     | Low      | Median   | High     |
| ----------------------------- | -------- | -------- | -------- |
| `fts5_upsert_batch/docs/100`  | 7.15 ms  | 7.19 ms  | 7.25 ms  |
| `fts5_upsert_batch/docs/500`  | 51.42 ms | 51.55 ms | 51.69 ms |
| `fts5_upsert_batch/docs/1000` | 150.5 ms | 153.7 ms | 158.1 ms |

#### sqlite-vec Vector Search (10K corpus, 384-dim)

| Benchmark                     | Low      | Median   | High     |
| ----------------------------- | -------- | -------- | -------- |
| `sqlite_vec_search/top_k/10`  | 9.11 ms  | 9.22 ms  | 9.38 ms  |
| `sqlite_vec_search/top_k/50`  | 9.52 ms  | 9.58 ms  | 9.64 ms  |
| `sqlite_vec_search/top_k/100` | 10.39 ms | 10.60 ms | 10.83 ms |

#### sqlite-vec Batch Insert

| Benchmark                              | Low      | Median   | High     |
| -------------------------------------- | -------- | -------- | -------- |
| `sqlite_vec_insert_batch/records/100`  | 5.63 ms  | 5.94 ms  | 6.28 ms  |
| `sqlite_vec_insert_batch/records/500`  | 12.25 ms | 12.58 ms | 12.93 ms |
| `sqlite_vec_insert_batch/records/1000` | 25.13 ms | 27.24 ms | 29.50 ms |

#### Backend Creation

| Benchmark                         | Low      | Median   | High     |
| --------------------------------- | -------- | -------- | -------- |
| `storage_backend_creation/memory` | 20.22 µs | 20.89 µs | 21.82 µs |
| `storage_backend_creation/file`   | 1.076 ms | 1.084 ms | 1.093 ms |

- Notes: none

## Regression policy

- Any hot-path benchmark regressing >5% vs baseline requires investigation before merge.
- Run `cargo bench -p khive-db --features vectors` in CI or locally before
  performance-sensitive PRs.
- Update the baseline table after hardware changes or significant optimizations.

Last reviewed: v0.2.8 (2026-06-08)
