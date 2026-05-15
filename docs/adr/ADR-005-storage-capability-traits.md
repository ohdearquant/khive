# ADR-005: Storage Capability Traits (Trait-Only Crate, Zero Implementations)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A research KG needs storage for multiple modalities:

- SQL rows (notes, entities, events)
- Vector embeddings (semantic search over content)
- Full-text search (BM25/FTS5 over titles and bodies)
- Graph edges + traversal

Each modality has different backend options:

- SQL: SQLite, Postgres, MySQL
- Vectors: sqlite-vec, HNSW (in-memory), Qdrant, pgvector
- Text: FTS5, Tantivy, Elasticsearch
- Graph: same SQL backend OR Neo4j OR DuckDB

If services depend on concrete backends, swapping a backend requires changing every consumer. If
services depend on traits, the backend is a runtime choice.

## Decision

**`khive-storage` is a trait-only crate. Zero implementations. Six capability traits.**

```rust
// In khive-storage:
pub trait SqlAccess { ... }          // SQL reader/writer/transaction
pub trait VectorStore { ... }        // Vector embedding search
pub trait TextSearch { ... }         // Full-text search
pub trait GraphStore { ... }         // Edge CRUD + traversal
pub trait NoteStore { ... }          // Temporal record CRUD
pub trait EventStore { ... }         // Append-only operation log
```

Implementations live in **separate crates**:

- `khive-db`: SQLite implementations (default for v0.1)
- Future: `khive-pg` (Postgres), `khive-qdrant` (Qdrant vectors), etc.

Services depend on `Arc<dyn SqlAccess>`, `Arc<dyn GraphStore>`, etc. — never on concrete types.

## Rationale

### Why trait-only?

If a trait crate has implementations, it gains dependencies (e.g., rusqlite, sqlite-vec). Every
consumer of the trait crate inherits those deps. By keeping `khive-storage` impl-free:

- The trait crate has zero heavy dependencies (just `serde`, `chrono`, `uuid`, `async-trait`).
- Backends can be swapped without recompilation of consumers.
- Multiple backends can coexist in one binary.

### Why six capability traits, not one big "Storage" trait?

The Interface Segregation Principle in practice. A service that only needs SQL shouldn't have to
implement (or mock) vector search. Examples:

- The entity service needs `GraphStore` + `SqlAccess`. Not vectors.
- The note-retrieval consumer needs `NoteStore` + `VectorStore` + `TextSearch`. Not graph.
- The event service needs only `EventStore`.

Each service depends on exactly what it uses, and only the necessary backends are wired up.

### Why these six (not fewer/more)?

The six match the three substrates + their orthogonal storage capabilities:

| Substrate      | Required Capabilities                               |
| -------------- | --------------------------------------------------- |
| Note           | `NoteStore`, optionally `VectorStore`, `TextSearch` |
| Entity (edges) | `GraphStore`                                        |
| Event          | `EventStore`                                        |

`SqlAccess` is the base capability — backends implementing the higher-level traits typically also
expose raw SQL for ad-hoc queries.

### Why include `BatchWriteSummary`, `Page<T>`, `PageRequest` in the trait crate?

These are part of the trait contract — the shape of return values is part of the API. Putting them
elsewhere would force consumers to depend on a separate "types" crate.

### Why `Arc<dyn Trait>` and not generics?

Generics would propagate through every service signature. `Arc<dyn>` keeps the API surface clean at
the cost of one virtual dispatch per call — negligible overhead for storage operations that already
cross IO boundaries.

## Alternatives Considered

| Alternative                            | Pros                         | Cons                                           | Why rejected            |
| -------------------------------------- | ---------------------------- | ---------------------------------------------- | ----------------------- |
| One big `Storage` trait                | Simpler API                  | Forces every backend to implement everything   | Wrong abstraction       |
| Concrete types throughout              | No virtual dispatch overhead | Swapping backend = rewrite                     | Inflexibility           |
| Generic services `Service<S: Storage>` | Zero-cost abstraction        | Type parameter explosion in service signatures | Too noisy               |
| sqlx-style query traits                | Compile-time checked SQL     | Doesn't extend to vectors/text/graph           | Doesn't fit the breadth |

## Consequences

### Positive

- Backends are swappable without changing service code.
- Trait crate is lightweight — no heavy deps.
- New backends can be added by implementing the traits in a separate crate.
- Services declare exactly which capabilities they need (Interface Segregation).

### Negative

- Trait definitions are upfront design work; getting them wrong means breaking changes later.
  Mitigated: the traits are derived from real services' needs, not speculation.
- `Arc<dyn>` overhead — negligible for storage operations (already IO-bound).
- Implementations are duplicated across backend crates (no shared base impl). Mitigated: helper
  functions in `khive-storage::types` for shared logic.

### Neutral

- Documentation lives with the trait, not the implementation. Implementations link back to the trait
  docs.

## Implementation

### `khive-storage` crate structure:

```
crates/khive-storage/src/
├── lib.rs              // Re-exports
├── capability.rs       // StorageCapability enum (Sql, Notes, Vectors, Text, Graph, Event)
├── error.rs            // StorageError (NotFound, AlreadyExists, Conflict, ...)
├── types.rs            // SqlValue, SqlRow, Edge, Note, etc. — shared types
├── sql.rs              // SqlAccess, SqlReader, SqlWriter, SqlTransaction
├── vectors.rs          // VectorStore
├── text.rs             // TextSearch
├── graph.rs            // GraphStore
├── note.rs             // NoteStore + Note
└── event.rs            // EventStore + Event + EventKind + EventOutcome
```

Dependencies: `serde`, `serde_json`, `chrono`, `uuid`, `async-trait`, `khive-score` (for
DeterministicScore in search hits).

No rusqlite, no sqlite-vec, no Postgres driver. Pure traits.

### `khive-db` crate (the first implementation):

Implements all six traits using SQLite + sqlite-vec + FTS5. Other backends (`khive-pg`,
`khive-duckdb`) can plug in later without touching `khive-storage`.

### Error handling

All operations return `Result<T, StorageError>`. `StorageError` is granular enough for callers to
distinguish retryable (Pool, Timeout, Transaction) from fatal (InvalidInput, NotFound).

## References

- ADR-003: Four-Layer Architecture (this fits in the Crates layer)
- ADR-004: Substrate Observables (the capabilities map to substrates)
- `crates/khive-storage/`: trait definitions
- `crates/khive-db/`: first implementation
