# khive-retrieval Design

## ADR Compliance

### Graph Traversal Algorithms (ADR-004)

- The legacy `graph` module (the `graph-legacy` feature) was removed (#58). It predated the
  `GraphStore` trait and duplicated traversal that now lives canonically in `khive-runtime`
  (`graph_traversal`) over `khive-storage`'s `GraphStore` and unified `TraversalOptions`.
- Relationship-aware retrieval composes with that runtime traversal rather than a
  retrieval-local `LinkStore` implementation.

### ADR-006: Deterministic Scoring

- All scores in this crate use `DeterministicScore` from `khive-score` (i64 fixed-point).
- `DeterministicScore::from_f64` is the only entry point for converting f64 similarity
  scores; callers must not bypass this.
- This guarantees cross-platform ranking identity (x86_64, ARM64, WASM) and enables
  `Ord` + `Hash` on ranked results.

### Retrieval as Composition of Storage-Capability Signals (ADR-012)

- `khive-retrieval` materialises the composition layer described in ADR-012.
- `VectorSearch`, `KeywordSearch`, `HybridSearcher`, and `Reranker` are independent traits.
- `HybridSearcher` is blanket-implemented for types that provide both `VectorSearch` and
  `KeywordSearch`.
- Namespace enforcement is the responsibility of the runtime layer, not this crate.

### Feature Flag Policy (ADR-030)

- `Cargo.toml` uses `default = []`, as specified by ADR-030. Consumers opt into the
  additional engine, checkpoint, adapter, policy and embedding implementation surfaces
  they need.

## Consistency Notes

- **Graph module removed**: The legacy `graph-legacy` `LinkStore`-based traversal module was
  deleted (#58). Traversal lives in `khive-runtime` over `khive-storage`'s `GraphStore`.
