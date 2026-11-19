# khive-db Benchmarks

Performance benchmarks for the khive-db storage layer, covering FTS5 text
search, sqlite-vec vector operations, backend creation overhead, typed stores,
and pooled reader acquisition. See the [scenario and measurement guide](../benchmarks.md)
for the new workloads and their timing boundaries.

## Suite

The historical groups below are defined in `benches/db_hot_path.rs`.
`benches/db_concurrency.rs` adds `concurrent_fts_reads/{1,4,8}` and
`write_under_read_load/{0,4}`. New targets `db_stores` and `db_pool` are listed
in the [scenario guide](../benchmarks.md#scenarios). Existing workload code is unchanged.

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

### Current-main rebaseline — pending

The source base for this benchmark addition is
`d3619dec9e32045f273c61b5a7b2eb70fe6463a3`; this is provenance, not a measured
baseline. Measurement commit, toolchain, machine/OS and results are **pending**.
The owner records these after running on the measurement machine. No old
number is relabeled as a result for the current main revision.

| Target/group                                                                                                         | Coverage                                                            | Current baseline |
| -------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- | ---------------- |
| `db_hot_path`: `fts5_search`, `fts5_search_unranked`, `fts5_rank_within_cap`, `fts5_term_stats`, `fts5_upsert_batch` | Existing FTS workloads, all sizes                                   | pending          |
| `db_hot_path`: `sqlite_vec_search`, `sqlite_vec_insert_batch`                                                        | Existing 10K × 384-dim workloads, all sizes                         | pending          |
| `db_hot_path`: `storage_backend_creation`                                                                            | Existing memory/file workloads                                      | pending          |
| `db_concurrency`: `concurrent_fts_reads/{1,4,8}`                                                                     | 10K documents; 8 searches/task; wave wall-time                      | pending          |
| `db_concurrency`: `write_under_read_load/{0,4}`                                                                      | One document upsert with 0/4 concurrent readers                     | pending          |
| `db_stores`: `entity_upsert_batch`, `entity_page`, `edge_upsert_batch`, `edge_neighbors`, `edge_traverse`            | All generated sizes in the scenario guide                           | pending          |
| `db_stores`: `note_upsert_batch`, `note_filtered_query`, `event_append_batch`, `event_page`                          | All generated sizes in the scenario guide                           | pending          |
| `db_pool`: `pooled_reader_checkout_window`, `pooled_reader_checkout_p99`                                             | 1/4/8/16 threads; separate wave wall-time and empirical acquire p99 | pending          |

### Historical: v0.2.8 - 2026-06-08

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

Timing results are advisory trends against rolling and fixed references, not a
fixed-percentage merge gate. Compare matching toolchain, hardware, flags and
fixture protocols; investigate changes with that context. Keep historical rows
and add a named measurement entry after hardware or workload changes.
Workflow wiring and proposed guarantee-schema changes are outside this addition.

## Performance guarantees

| Operation               | Input                       | Metric         | Bound                                                                     |
| ----------------------- | --------------------------- | -------------- | ------------------------------------------------------------------------- |
| FTS5 AnyTerm search     | 10K docs, 3 terms, top_k=20 | latency median | Advisory trend; last ledger value 14.87 ms (`docs/api/benchmarks.md:71`)  |
| FTS5 batch upsert       | 100 docs                    | latency median | Advisory trend; last ledger value 7.19 ms (`docs/api/benchmarks.md:86`)   |
| sqlite-vec KNN search   | 10K × 384-dim, top_k=100    | latency median | Advisory trend; last ledger value 10.60 ms (`docs/api/benchmarks.md:96`)  |
| sqlite-vec batch insert | 1000 records × 384-dim      | latency median | Advisory trend; last ledger value 27.24 ms (`docs/api/benchmarks.md:104`) |
| Entity batch upsert     | 1000 entities               | latency median | Advisory trend against the recorded baseline                              |
| Edge neighbors          | 10K edges, depth=2          | latency median | Advisory trend against the recorded baseline                              |
| Concurrent FTS reads    | 8 tasks × 8 searches        | wall-time      | Advisory trend against the recorded baseline                              |

These rows copy issue #5644. The neighbor depth=2 premise belongs to
`GraphStore::traverse`; the public neighbor call is one-hop. See the
[measurement guide](../benchmarks.md) for that distinction and the empirical
p99 definition. All baseline values added for current main remain pending.

Historical ledger last measured: v0.2.8 (2026-06-08).
