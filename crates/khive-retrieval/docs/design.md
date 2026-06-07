# khive-retrieval Design

## ADR Compliance

### ADR-004: Graph Traversal Algorithms

- This crate's `graph` module (feature `graph-legacy`) implements BFS, DFS, and bidirectional
  BFS shortest-path over the `LinkStore` trait from `khive-db`.
- The traversal algorithms operate on `LinkStore` / `EntityRef` / `StorageContext` types,
  enabling relationship-aware retrieval pipelines.
- Safety limits `MAX_TRAVERSAL_DEPTH = 20` and `MAX_TRAVERSAL_RESULTS = 10_000` prevent
  runaway traversals.
- The `graph` module is gated behind `feature = "graph-legacy"` because the `LinkStore` API
  predates the current `GraphStore` trait in `khive-storage`. It is not yet ported.

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
- **Graph module not ported**: The `graph-legacy` feature exposes the old `LinkStore`-based
  traversal API. This should be ported to the `GraphStore` trait from `khive-storage` and
  the feature flag removed. Tracked as a known gap.
- **EmbeddedEngine stub**: `engine_replay.rs` defines `type EmbeddedEngine = ()` as a
  placeholder for when `khive-inference` lands. This is an intentional forward stub, not
  dead code; the `#[allow(dead_code)]` comment explains why.
