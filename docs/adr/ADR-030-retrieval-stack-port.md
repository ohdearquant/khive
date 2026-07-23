# ADR-030: Retrieval Stack and Crate Boundaries

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

ADR-005 defines storage capabilities and ADR-012 defines high-level retrieval
composition. Dense vector search, lexical ranking, fusion, index persistence, and
quantization are substantial algorithmic concerns that should not live directly in
`khive-runtime` or a pack handler.

The retrieval layer must provide:

- approximate-nearest-neighbor and exact compatibility paths;
- lexical BM25 search;
- deterministic fusion of ranked lists;
- optional persistence and checkpoint support;
- storage adapters that preserve the ADR-005 capability boundaries; and
- a stable building-block API for multi-engine composition in ADR-031.

This ADR describes public crates and their responsibilities. It does not depend on
private source history, unpublished implementation inventories, or delivery schedules.

## Decision

### Public crate map

```text
khive-types
  └── khive-score
        └── khive-fold

khive-storage
  ├── khive-db
  ├── khive-bm25
  ├── khive-hnsw
  ├── khive-vamana
  ├── khive-fusion
  └── khive-retrieval

khive-runtime
  └── pack handlers
```

The crate responsibilities are:

| Crate             | Responsibility                                                                    |
| ----------------- | --------------------------------------------------------------------------------- |
| `khive-retrieval` | Common search configuration, hybrid orchestration, query IR, and storage adapters |
| `khive-hnsw`      | HNSW vector index, search, checkpointing, and persistence                         |
| `khive-vamana`    | Vamana graph index implementation                                                 |
| `khive-bm25`      | Lexical indexing and BM25 ranking                                                 |
| `khive-fusion`    | RRF, weighted, union, vector-only, and keyword-only fusion                        |
| `khive-score`     | Deterministic comparison and score representation                                 |
| `khive-fold`      | Generic objective and selector composition from ADR-024                           |
| `khive-storage`   | Capability traits consumed by storage adapters                                    |
| `khive-db`        | SQLite storage, including the shipped sqlite-vec compatibility path               |

`khive-runtime` composes these crates but does not own their algorithms.

### Ownership boundaries

ADR-030 owns:

- dense and lexical retrieval building blocks;
- low-level ranked-list fusion;
- vector and keyword storage adapters;
- persistence contracts for retrieval indexes; and
- the hybrid-search entry point.

The following are outside this ADR:

- storage capability definitions, owned by ADR-005;
- request-level composition, owned by ADR-012;
- multi-engine fan-out and per-engine calibration, owned by ADR-031;
- reranker traits and rerank-stage configuration, owned by ADR-042; and
- pack-specific scoring, owned by the relevant pack.

### Public API shape

Consumers use the retrieval stack through types equivalent to:

```rust
pub use khive_retrieval::{
    HybridSearcher,
    SearchConfig,
    StorageKeywordSearch,
    StorageVectorSearch,
};

pub use khive_fusion::FusionStrategy;
pub use khive_hnsw::{HnswCheckpoint, HnswConfig, HnswIndex};
pub use khive_bm25::{Bm25Config, Bm25Index};
```

`StorageVectorSearch` adapts an ADR-005 `VectorStore` to the retrieval vector-search
interface. `StorageKeywordSearch` performs the equivalent adaptation for `TextSearch`.
Adapters translate capabilities; they do not bypass namespace or authorization tokens.

`HybridSearcher` coordinates dense and lexical candidate generation and delegates
ranked-list combination to `khive-fusion`. It remains independent of pack-specific
post-processing.

### Deterministic ranking

All fusion strategies define stable tie-breaking through `khive-score`. A strategy may
accept floating-point input scores, but the externally observable ordering must be stable
for equal logical inputs. Ranked outputs use an explicit secondary key rather than relying
on hash-map or task completion order.

RRF operates on ranks and is the default cross-signal combination when raw score scales
are not directly comparable. Weighted fusion is permitted only when its weights have a
defined configuration source.

### Index and quantization boundaries

`khive-hnsw` owns the graph index, candidate search, checkpoint format, and persistence
behavior for HNSW. Quantized search may use a coarse candidate phase followed by exact
reranking. The public result contract is independent of the selected distance kernel.

`khive-vamana` supplies an alternative graph index. It is not selected implicitly by the
runtime; a caller or configuration chooses an index implementation through the documented
construction path.

`khive-bm25` owns token statistics and lexical scoring. It does not access vector indexes.
`khive-fusion` consumes ranked results from either source without owning their storage.

### sqlite-vec compatibility

The public `khive-db` crate still ships a sqlite-vec `VectorStore` compatibility path.
The retrieval crates coexist with that path. This ADR does not claim that sqlite-vec has
been removed or that every runtime call is routed through HNSW.

Replacing the compatibility path requires a separate migration decision covering vector
rebuild, index selection, rollback, and storage-version handling. Until that decision is
implemented, both paths must preserve the same `VectorStore` contract.

### Features and optional dependencies

Retrieval consumers opt into persistence, checkpointing, and storage adapters only when
needed. Feature flags must not alter the semantic meaning of a result. Disabling a feature
may remove a constructor or implementation, but cannot silently select a different ranking
policy for the same configuration.

Native embedding providers remain outside the retrieval algorithm crates. The retrieval
layer accepts vectors or an embedding interface supplied by its consumer.

### Persistence and recovery

Persisted indexes are derived state. A persisted index records enough identity metadata to
determine whether it matches the configured model, dimensions, metric, and source snapshot.
If compatibility validation fails, the index is rejected and rebuilt from authoritative
storage.

Checkpoint publication is atomic from the reader's perspective. Readers observe either the
preceding complete checkpoint or the new complete checkpoint, never a partially written
file.

## Rationale

### Why separate retrieval crates

ANN indexing, lexical ranking, fusion, and storage adaptation evolve independently and have
different dependency profiles. Separate crates keep the capability graph explicit and let a
consumer use BM25 or fusion without linking an unrelated index implementation.

### Why keep high-level orchestration in `khive-retrieval`

The component crates expose algorithms. `khive-retrieval` supplies the common query and
adapter vocabulary needed to combine them. This avoids placing algorithm orchestration in
`khive-runtime` while preserving one entry point for typical hybrid search.

### Why retain the sqlite-vec path

Existing databases and `VectorStore` implementations require a migration contract before
their active index can change. Coexistence is explicit and testable; claiming an early
retirement would make the ADR disagree with the shipped public code.

### Why fusion is a distinct crate

Fusion depends on ranked lists, not on index internals. Keeping it separate allows the same
deterministic implementation to combine vector and lexical results, multiple embedding
engines, or backend-local result sets.

## Alternatives Considered

| Alternative                                | Reason rejected                                                                        |
| ------------------------------------------ | -------------------------------------------------------------------------------------- |
| Keep all retrieval code in `khive-runtime` | Couples algorithms to runtime orchestration and expands the runtime dependency surface |
| Use only sqlite-vec                        | Preserves compatibility but does not provide the public graph-index implementations    |
| One crate for every retrieval concern      | Forces BM25, ANN, fusion, and persistence dependencies on every consumer               |
| Hide fusion inside each searcher           | Duplicates ranking rules and risks inconsistent tie-breaking                           |
| Remove sqlite-vec immediately              | Lacks the required migration and rollback contract                                     |

## Consequences

### Positive

- Public crate boundaries match algorithmic responsibilities.
- Consumers can select only the retrieval components they need.
- Deterministic fusion is shared across retrieval paths.
- Storage adapters preserve ADR-005 capability contracts.
- Persisted indexes remain safely rebuildable derived state.

### Negative

- The workspace contains several retrieval crates rather than one monolith.
- Compatibility and graph-index paths coexist until a separate migration is complete.
- Cross-crate integration requires contract tests for features and result ordering.

### Neutral

- The request and MCP verb surfaces are unchanged.
- `khive-query` continues to compile GQL/SPARQL independently of retrieval algorithms.
- ADR-031 composes multiple engines above this layer.

## Validation

The public implementation is expected to test:

- stable ordering for equal scores;
- HNSW and Vamana construction and query contracts;
- BM25 index and query round trips;
- each fusion strategy against fixed ranked inputs;
- checkpoint compatibility rejection and atomic publication;
- storage-adapter namespace propagation; and
- sqlite-vec compatibility while the legacy path remains shipped.

## References

- [ADR-005](./ADR-005-storage-capability-traits.md): storage capabilities
- [ADR-006](./ADR-006-deterministic-scoring.md): deterministic scoring
- [ADR-012](./ADR-012-retrieval-composition.md): request-level retrieval composition
- [ADR-024](./ADR-024-fold-cognitive-primitives.md): deterministic fold primitives
- [ADR-029](./ADR-029-substrate-coordinator.md): backend-level coordination
- [ADR-031](./ADR-031-multi-engine-retrieval.md): multi-engine retrieval
