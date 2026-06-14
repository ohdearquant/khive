# khive-retrieval Design

## ADR Compliance

### ADR-004: Graph Traversal Algorithms

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

### ADR-012: Retrieval as Composition of Storage-Capability Signals

- `khive-retrieval` materialises the composition layer described in ADR-012.
- `VectorSearch`, `KeywordSearch`, `HybridSearcher`, and `Reranker` are independent traits.
- `HybridSearcher` is blanket-implemented for types that provide both `VectorSearch` and
  `KeywordSearch`.
- Namespace enforcement is the responsibility of the runtime layer, not this crate.
  Per-namespace filtering helpers (`filter_atoms_by_namespace`) are provided for callers
  operating below the runtime trust boundary.

### ADR-030: Feature Flag Policy

- `replay/engine_replay.rs` is co-located with its SQL schema helpers because all five
  replay primitives share a single SQLite connection type and the same `weight_events`
  schema. Splitting would duplicate schema definitions and connection wiring.
- The `engine` feature flag in `engine_replay.rs` is intentionally undeclared (`#[allow(unexpected_cfgs)]`);
  it guards an `EmbeddedEngine` integration point that is not yet ported (blocked on
  `khive-inference` crate landing).
- Feature flags in this crate deviate from ADR-030 defaults: `checkpoint`, `persist`,
  `embed`, and `storage-adapters` are not default-on. This deviation is tracked pending
  an ADR-030 amendment.

## Consistency Notes

- **Feature default deviation (ADR-030)**: ADR-030 marks `checkpoint`, `persist`, `embed`,
  and `storage-adapters` as default-on, but `Cargo.toml` has `default = []`. An ADR-030
  amendment is needed to reflect the actual shipping defaults.
- **Graph module removed**: The legacy `graph-legacy` `LinkStore`-based traversal module was
  deleted (#58). Traversal lives in `khive-runtime` over `khive-storage`'s `GraphStore`.
- **EmbeddedEngine stub**: `engine_replay.rs` defines `type EmbeddedEngine = ()` as a
  placeholder for when `khive-inference` lands. This is an intentional forward stub, not
  dead code; the `#[allow(dead_code)]` comment explains why.
