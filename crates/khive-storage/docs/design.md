# khive-storage Design

Function-specific technical reference docs (error taxonomy, blob store,
transaction registry) live in [`docs/api/`](api/). This document covers
design rationale and ADR compliance.

## Scope

Trait-only storage capability surface. Contains zero backend implementations.
All concrete storage logic lives in `khive-db` and retrieval crates. This crate
defines the contracts that backends must satisfy.

## ADR Compliance

### ADR-004: Substrate Observables

`EventStore` is an append-only operation log. Every verb execution produces one
`Event` record. Events are immutable once appended; projection rows are written
beside the event at append time. The `EventFilter` struct supports querying by
verb, substrate, actor, session, aggregate, and observed/selected referents.

### ADR-005: Storage Capability Traits

The `StorageCapability` enum in `capability.rs` identifies which surface produced
an error (`Sql`, `Notes`, `Entities`, `Graph`, `Events`, `Vectors`, `Sparse`,
`Text`). Each trait file defines one capability surface as a separate module.

### ADR-009: Backend Architecture

`khive-storage` depends on neither `rusqlite` nor `khive-db`, preserving the
trait-only boundary. Backends implement these traits in their own crates.

### ADR-031: Multi-Engine Retrieval — Embedder Trait, Registry, Configuration, and Pack Orchestration

`SparseStore` defines the sparse vector capability surface over the
`SparseVector` type (parallel `indices`/`values` arrays). `TextSearch` defines
the FTS capability surface. The `search_with_options` extension method supports a
two-stage gather + rank strategy via `TextSearchOptions` and `TextGatherMode`.
Non-default gather options return `StorageError::Unsupported` on backends that do
not override the method. Term-level document-frequency statistics are exposed via
`term_stats`, also optional (`Unsupported` by default).

### ADR-041: Event Provenance Projection — Hybrid Log + Graph Edges / ADR-044: Vector Store Extensions — Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep

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
  sqlite-vec backends enforce `vectors.len() == 1` (single vector per primary key
  row).

## Modules

| Module                                      | Purpose                                                                     |
| ------------------------------------------- | --------------------------------------------------------------------------- |
| [`src/capability.rs`](../src/capability.rs) | `StorageCapability` enum                                                    |
| [`src/entity.rs`](../src/entity.rs)         | `Entity`, `EntityFilter`, `EntityStore`                                     |
| [`src/error.rs`](../src/error.rs)           | `StorageError`                                                              |
| [`src/event.rs`](../src/event.rs)           | `Event`, `EventFilter`, `EventStore`                                        |
| [`src/graph.rs`](../src/graph.rs)           | `GraphStore`                                                                |
| [`src/note.rs`](../src/note.rs)             | `Note`, `NoteFilter`, `NoteStore`                                           |
| [`src/sparse.rs`](../src/sparse.rs)         | `SparseStore`                                                               |
| [`src/sql.rs`](../src/sql.rs)               | `SqlAccess`, `SqlReader`, `SqlWriter`, `AtomicUnitOp`                       |
| [`src/text.rs`](../src/text.rs)             | `TextSearch`                                                                |
| [`src/types/`](../src/types/)               | Shared types split by domain (vector, text, graph, sparse, sql, pagination) |
| [`src/vectors.rs`](../src/vectors.rs)       | `VectorStore`                                                               |

## Tests

| Path                                            | Coverage                                                                                                           |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| [`tests/compliance.rs`](../tests/compliance.rs) | Validate() invariant tests for `VectorSearchRequest`, `SparseVector`, `EdgeFilter`; vector filter compliance suite |
| [`tests/vectors.rs`](../tests/vectors.rs)       | `VectorStore` default-impl behavior: capabilities, batch, update, orphan sweep                                     |

## Invariants

- This crate has **zero implementations**. All concrete backends live elsewhere.
- `SparseVector`: indices and values must be equal length, indices strictly
  increasing, all values finite.
- `VectorSearchRequest`: query_vectors non-empty, top_k > 0, all values finite.
- `EdgeFilter`: weight bounds must be finite and min <= max.
- Deserialization of `VectorSearchRequest`, `SparseVector`, and `EdgeFilter`
  enforces invariants via `serde(try_from)`.
- Storage is ID-only. Namespace authorization is enforced at the runtime layer.

## Failure Modes

- `StorageError::InvalidInput` for constraint violations at the trait boundary.
- `StorageError::Unsupported` for optional capabilities a backend has not
  implemented.
- `StorageError::Driver` wraps backend-specific errors.
- `StorageError::NotFound` / `AlreadyExists` for ID-based lookups.
- Pool, Timeout, Transaction errors are retryable (`is_retryable() == true`).

## Consistency Notes

- `VectorStoreCapabilities.supports_multi_field`: sqlite-vec backends use a
  `subject_id PRIMARY KEY` table and therefore only support one vector per subject
  per namespace. Backends that support multiple named fields per subject (e.g.
  `entity.title` and `entity.body`) must set this to `true`.
- `max_dimensions` baseline: 8192 (sqlite-vec 0.1.9 limit
  `SQLITE_VEC_VEC0_MAX_DIMENSIONS`). Backends with a different limit should
  override `capabilities()` and return the correct value.
- `TextTermStats.inverse_document_frequency` uses the Robertson-Walker IDF
  formula.

Last reviewed: 2026-06-06
