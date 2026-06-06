# khive-storage Design

This crate is a trait-only surface. It contains zero backend implementations.
All concrete storage logic lives in `khive-db` and retrieval crates.

## ADR Compliance

### ADR-004: Event Store

`EventStore` is an append-only operation log. Every verb execution produces
one `Event` record. Events are immutable once appended; projection rows are
written beside the event at append time. The `EventFilter` struct supports
querying by verb, substrate, actor, session, aggregate, and observed/selected
referents.

### ADR-005: Storage Capability Traits

The `StorageCapability` enum in `capability.rs` identifies which surface
produced an error (`Sql`, `Notes`, `Entities`, `Graph`, `Events`, `Vectors`,
`Sparse`, `Text`). Each trait file defines one capability surface as a
separate module.

### ADR-024: Full-Text Search

`TextSearch` defines the FTS capability surface. The `search_with_options`
extension method supports a two-stage gather + rank strategy via
`TextSearchOptions` and `TextGatherMode`. Non-default gather options return
`StorageError::Unsupported` on backends that do not override the method.
Term-level document-frequency statistics are exposed via `term_stats`, also
optional (`Unsupported` by default).

### ADR-031: Sparse Vector Store

`SparseStore` defines the sparse vector capability surface over the
`SparseVector` type (parallel `indices`/`values` arrays). Invariants enforced
at the trait boundary: arrays must be equal length, indices strictly
increasing, and all values finite. These are validated via
`SparseVector::validate()`.

### ADR-041 / ADR-044: Vector Store Capabilities and Filter Pushdown

`VectorStoreCapabilities` is returned by `VectorStore::capabilities()` and
introspected by the retrieval layer at construction time to select code paths
without error-type matching.

Key design constraints:
- The default `capabilities()` impl returns a conservative baseline with all
  optional features disabled, preserving backward compatibility for existing
  implementations.
- Backends that claim `supports_filter = true` but do not override
  `search_with_filter` will trigger a `debug_assert` at runtime.
- `OrphanSweepConfig.subject_id_allowlist = None` means scan all rows;
  `Some(ids)` restricts the sweep to only those IDs.
- `VectorRecord.vectors` may contain multiple embeddings per subject per field;
  sqlite-vec backends enforce `vectors.len() == 1` (single vector per primary
  key row).

## Consistency Notes

- `VectorStoreCapabilities.supports_multi_field`: sqlite-vec backends use a
  `subject_id PRIMARY KEY` table and therefore only support one vector per
  subject per namespace. Backends that support multiple named fields per
  subject (e.g. `entity.title` and `entity.body`) must set this to `true`.
- `max_dimensions` baseline: 8192 (sqlite-vec 0.1.9 limit
  `SQLITE_VEC_VEC0_MAX_DIMENSIONS`). Backends with a different limit should
  override `capabilities()` and return the correct value.
- `TextTermStats.inverse_document_frequency` uses the Robertson-Walker IDF
  formula: $\ln\!\left(\frac{N - df + 0.5}{df + 0.5} + 1\right)$
