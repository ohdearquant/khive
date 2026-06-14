# khive-retrieval: Architecture and Design

**Scope**: Hybrid search and ranking primitives for the khive knowledge graph runtime.
Combines HNSW vector search, BM25 keyword search, and RRF/weighted/union fusion into a
single retrieval layer with deterministic scoring throughout. Designed to compose with
`khive-runtime` via storage-capability traits and the pack-dispatch surface.

**Last reviewed**: 2026-06-06

## Module layout

| Module                 | Purpose                                                                       |
| ---------------------- | ----------------------------------------------------------------------------- |
| `src/adapters/`        | `StorageVectorSearch` and `StorageKeywordSearch` (feature `storage-adapters`) |
| `src/error.rs`         | `RetrievalError` enum and `Result` alias                                      |
| `src/eval/`            | Precision/recall/nDCG/Jaccard retrieval evaluation metrics                    |
| `src/hybrid/`          | `HybridSearcher`, `DualIndexRouter`, `HybridConfig`, `Query`                  |
| `src/metrics/`         | `MetricEvent`, `MetricsSink`, `RecordingSink`, `NoopSink`                     |
| `src/persist/`         | SQLite persistence for HNSW/BM25 indexes (feature `persist`)                  |
| `src/policy/`          | `SearchPolicy`, `ClearanceLevel`, `filter_by_policy`                          |
| `src/query_ir.rs`      | `QueryNode` IR tree; composable, serialisable query plans                     |
| `src/replay/`          | Temporal replay and drift metrics (feature `persist`)                         |
| `src/search_config.rs` | Per-call `SearchConfig` for recall/compose search phase                       |
| `src/timeout.rs`       | `search_with_timeout`, `search_with_cancellation`, `search_with_deadline`     |
| `src/weights/`         | Per-atom weight loading for replay (feature `persist`)                        |

## Tests and benchmarks

- Unit and integration tests: `src/**` inline (`#[cfg(test)]`) + `tests/fusion_surface.rs`
- Benchmarks: `benches/fusion_bench.rs` (Criterion, `harness = false`)
- Benchmark ledger: `docs/benchmarks.md`

## Design principles

### Deterministic scoring (ADR-006)

All scores use `DeterministicScore` from `khive-score` (i64 fixed-point). This gives:

- Cross-platform identical rankings (x86_64, ARM64, WASM)
- `Ord` implementation (sortable, usable in `BTreeSet`)
- `Hash` implementation (cacheable)

ADR-006 is the normative contract for score representation. Do not introduce raw `f32`/`f64`
scores in the ranked-result layer without conversion through `DeterministicScore`.

### Index composition (ADR-012)

ADR-012 established retrieval as composition of storage-capability signals. `khive-retrieval`
is the crate that materialises that composition. The `VectorSearch`, `KeywordSearch`,
`HybridSearcher`, and `Reranker` traits compose independently:

- Implementors provide only the traits they support.
- `HybridSearcher` is blanket-implemented for types that provide both `VectorSearch` and
  `KeywordSearch`.

### Feature flag policy (ADR-030)

| Feature            | Default | Notes                                               |
| ------------------ | ------- | --------------------------------------------------- |
| `checkpoint`       | off     | HNSW snapshot save/restore; requires `khive-fold`   |
| `persist`          | off     | SQLite persistence for indexes; requires `rusqlite` |
| `embed`            | off     | Native `lattice-embed` model implementations        |
| `storage-adapters` | off     | Bridge `khive-storage` backends to retrieval traits |
| `policy`           | off     | Gate integration (`khive-gate`); opt-in             |

The current `default = []` deviates from the ADR-030 table which marks `checkpoint`,
`persist`, `embed`, and `storage-adapters` as default-on. This deviation is tracked as a
known gap pending an ADR-030 amendment.

### Namespace isolation

Namespace enforcement is the responsibility of the runtime layer (ADR-012). Storage stores
are ID-only. The retrieval crate provides per-namespace filtering helpers
(`filter_atoms_by_namespace` in `replay/engine_replay.rs`) for use by callers that operate
below the runtime trust boundary.

## Invariants

1. `fuse_search_results` never returns more than `config.candidate_pool_size` items before
   top-k truncation.
2. `DeterministicScore::from_f64` is the only entry point for f64→score conversion; callers
   must not produce scores by direct arithmetic on `DeterministicScore` internals.
3. All public timeout functions propagate `RetrievalError::QueryTimeout` on elapsed deadline;
   they never silently swallow the timeout error.

## Failure modes

- `RetrievalError::QueryTimeout`: search future exceeded the configured duration.
- `RetrievalError::QueryCancelled`: cancellation token was triggered before search completed.
- `PersistError::Sqlite`: SQLite operation failed during index persistence/load.
- `RetrievalError::GraphTraversal`: BFS/DFS exceeded `MAX_TRAVERSAL_DEPTH` or `MAX_TRAVERSAL_RESULTS`.

## Quick start

```rust,ignore
use khive_retrieval::{VectorSearch, KeywordSearch, HybridSearcher, Query, HybridConfig};

// Keyword-only search
let results = searcher.keyword_search("distributed systems", 10).await?;

// Hybrid search (vector + keyword with RRF fusion)
let query = Query::hybrid("distributed systems", embedding_vec);
let config = HybridConfig::new(10);
let results = searcher.hybrid_search(&query, &config).await?;

for (id, score) in results {
    println!("{}: {}", id, score);
}
```
