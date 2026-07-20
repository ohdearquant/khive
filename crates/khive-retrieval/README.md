# khive-retrieval

Hybrid retrieval composer combining HNSW vector search, BM25 keyword search,
rank fusion, and optional graph/cross-encoder reranking, with deterministic
scoring throughout.

## Features

- **`HybridSearcher` trait** — combines `VectorSearch` + `KeywordSearch` (same
  `Id` type) behind one `hybrid_search(query, config)` call
- **`fuse_search_results`/`fuse_search_results_checked`** — fuse pre-computed
  vector and keyword result lists per a `HybridConfig`'s `FusionStrategy`, with
  min-score filtering and top-k truncation applied after fusion
- **Re-exports the retrieval stack** — `khive-hnsw`, `khive-bm25`,
  `khive-fusion`, and `lattice-embed` types are re-exported so a caller depends
  on this one crate for the full hybrid path
- **`DualIndexRouter`** — routes queries between a primary and legacy vector
  index during a migration, with a configurable auto-switch threshold
- **Timeout/cancellation wrappers** — `search_with_timeout`,
  `search_with_deadline`, `search_with_cancellation` around any search future
- **Feature-gated extensions** — see Configuration below

## Usage

```rust
use khive_retrieval::{fuse_search_results, HybridConfig, Query};

let query = Query::hybrid("rust async runtime", vec![0.1_f32; 384]);
let config = HybridConfig::new(10);

// vector_hits / keyword_hits come from your VectorSearch / KeywordSearch impls
// (or khive-hnsw::HnswIndex::search / khive-bm25::Bm25Index::search directly).
let fused = fuse_search_results(vec![vector_hits, keyword_hits], &config);
for (id, score) in &fused {
    println!("{id}: {}", score.to_f64());
}
```

`HybridConfig::new(top_k)` defaults to RRF fusion (k=60); chain
`.with_fusion_strategy(..)`, `.with_pool_size(..)`, `.with_min_score(..)`, or
`.with_weights(vector, keyword)` to customize. `fuse_search_results` falls back
to RRF if a `Weighted` strategy's weight count doesn't match the source count;
`fuse_search_results_checked` returns `Err` in that case instead.

## Configuration (Cargo features)

| Feature            | Adds                                                                                                |
| ------------------ | --------------------------------------------------------------------------------------------------- |
| `policy`           | `khive-gate`-backed `ClearanceLevel`/`SearchPolicy` result filtering                                |
| `checkpoint`       | `HnswCheckpoint`/`HnswCheckpointStore` re-exports (khive-hnsw snapshots via `khive-fold`)           |
| `persist`          | SQLite-based persistence for HNSW and BM25 indexes (`rusqlite`)                                     |
| `storage-adapters` | `StorageVectorSearch`/`StorageKeywordSearch` bridging sqlite-vec/FTS5 backends to the search traits |
| `embed`            | Native `lattice-embed` embedding service re-exports                                                 |
| `native-rerank`    | Cross-encoder reranking — deferred pending `khive-inference` port                                   |

None of these features are enabled by default. The base crate is not
dependency-free, though: it already depends on `khive-db` (which pulls in
`khive-storage` and `rusqlite`) and on `lattice-embed` for native embedding. The
features above gate additional surface — policy filtering, HNSW/BM25 checkpoint
and persistence, storage-backed search adapters, and cross-encoder reranking —
not the core storage or embedding stack.

`SearchConfig` (vector-only/keyword-only/hybrid-balanced presets) and
`SearchPolicy`/`ClearanceLevel` (with `filter_by_policy`/`filter_by_predicate`)
are also re-exported for callers that need per-result access control on top of
fusion.

## Where this sits

`khive-retrieval` sits above `khive-hnsw`, `khive-bm25`, `khive-fusion`,
`khive-score`, and `khive-types` in the storage stack, and below the runtime's
ADR-012 composition layer and the `kg`/`memory` packs that call into it for
hybrid FTS5+vector search.

Governing ADRs:
[ADR-030](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-030-retrieval-stack-port.md)
(this crate's charter — engine/adapter ownership split from ADR-012),
[ADR-012](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-012-retrieval-composition.md)
(the still-live high-level composition contract), and
[ADR-031](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-031-multi-engine-retrieval.md)
(multi-engine embedder registry and pack fan-out this crate composes with).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
