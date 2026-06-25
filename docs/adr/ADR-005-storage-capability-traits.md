# ADR-005: Storage Capability Traits

**Status**: accepted\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive

## Context

khive separates storage contracts from storage implementations. Backend code (SQLite,
sqlite-vec, FTS5) lives in `khive-db`. Every other crate that touches persistence — runtime,
packs, coordinator — depends on traits defined in `khive-storage`, never on `khive-db`
directly.

The trait crate exists for dependency inversion. If the runtime called SQLite directly,
swapping or testing backends would require recompiling the runtime. Traits let the runtime
compile against contracts; the binary wires in concrete backends at startup.

The constraints on `khive-storage`:

1. **Zero implementations.** The crate defines what a single backend can do. It does not
   decide which backend to call, when to call it, or what is valid input.
2. **Placement-blind.** Hot/cold/frozen tiers, backend IDs, ATTACH schema aliases, and
   federation topology are runtime/coordinator concerns. Storage traits define capabilities,
   not topology.
3. **Kind-agnostic.** Entity kinds, note kinds, entity_type validation, and edge endpoint
   legality are runtime concerns (ADR-001, ADR-002, ADR-003). Backends persist and filter
   fields; they do not govern vocabulary.
4. **Single-backend scoped.** Each trait instance talks to one backend. Cross-backend
   orchestration lives in the SubstrateCoordinator (ADR-003, ADR-029 (Substrate Coordinator)).

## Decision

### Eight capability traits

```rust
pub trait SqlAccess: Send + Sync + 'static {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>>;
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>>;
    async fn begin_tx(&self, options: SqlTxOptions) -> StorageResult<Box<dyn SqlTransaction>>;
}

pub trait NoteStore: Send + Sync + 'static {
    async fn upsert_note(&self, note: Note) -> StorageResult<()>;
    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary>;
    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>>;
    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    async fn query_notes(&self, namespace: &str, kind: Option<&str>,
                         filter: Page) -> StorageResult<Vec<Note>>;
    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64>;
}

pub trait EntityStore: Send + Sync + 'static {
    async fn upsert_entity(&self, entity: Entity) -> StorageResult<()>;
    async fn upsert_entities(&self, entities: Vec<Entity>) -> StorageResult<BatchWriteSummary>;
    async fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>>;
    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    async fn query_entities(&self, namespace: &str, filter: EntityFilter,
                            page: Page) -> StorageResult<Vec<Entity>>;
    async fn count_entities(&self, namespace: &str, filter: EntityFilter) -> StorageResult<u64>;
}

pub trait GraphStore: Send + Sync + 'static {
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()>;
    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary>;
    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    async fn delete_edge(&self, id: LinkId) -> StorageResult<bool>;
    async fn query_edges(&self, filter: EdgeFilter, page: Page) -> StorageResult<Vec<Edge>>;
    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64>;
    async fn neighbors(&self, query: NeighborQuery) -> StorageResult<Vec<NeighborHit>>;
    /// Batched form of `get_edge`: fetch multiple edges in one round-trip via
    /// `WHERE id IN (...)`. Default impl loops `get_edge`; SQLite backend overrides.
    async fn get_edges(&self, ids: &[LinkId]) -> StorageResult<Vec<Edge>> { ... }
    /// Batched form of `neighbors`: expand multiple source nodes in one round-trip,
    /// returning `(source_id, hit)` pairs. Default impl loops `neighbors`; SQLite overrides.
    async fn batch_neighbors(
        &self,
        sources: &[Uuid],
        query: NeighborQuery,
    ) -> StorageResult<Vec<(Uuid, NeighborHit)>> { ... }
    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>>;
}

pub trait EventStore: Send + Sync + 'static {
    async fn append_event(&self, event: Event) -> StorageResult<()>;
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary>;
    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>>;
    async fn query_events(&self, filter: EventFilter, page: Page) -> StorageResult<Vec<Event>>;
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64>;
}

pub trait VectorStore: Send + Sync + 'static {
    async fn insert(&self, namespace: &str, subject_id: Uuid,
                    field: &str, vectors: Vec<Vec<f32>>) -> StorageResult<()>;
    async fn insert_batch(&self, records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary>;
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;
    async fn count(&self) -> StorageResult<u64>;
    async fn search(&self, request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>>;
    async fn info(&self) -> StorageResult<VectorStoreInfo>;
    async fn rebuild(&self, scope: IndexRebuildScope) -> StorageResult<VectorStoreInfo>;
}

pub trait SparseStore: Send + Sync + 'static {
    async fn insert(&self, namespace: &str, subject_id: Uuid,
                    field: &str, indices: Vec<u32>, values: Vec<f32>) -> StorageResult<()>;
    async fn insert_batch(&self, records: Vec<SparseRecord>) -> StorageResult<BatchWriteSummary>;
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;
    async fn search(&self, request: SparseSearchRequest) -> StorageResult<Vec<SparseSearchHit>>;
    async fn count(&self) -> StorageResult<u64>;
}

pub trait TextSearch: Send + Sync + 'static {
    async fn upsert_document(&self, document: TextDocument) -> StorageResult<()>;
    async fn upsert_documents(&self, docs: Vec<TextDocument>) -> StorageResult<BatchWriteSummary>;
    async fn delete_document(&self, namespace: &str, subject_id: Uuid) -> StorageResult<bool>;
    async fn get_document(&self, namespace: &str, subject_id: Uuid)
        -> StorageResult<Option<TextDocument>>;
    async fn search(&self, request: TextSearchRequest) -> StorageResult<Vec<TextSearchHit>>;
    async fn count(&self, filter: TextFilter) -> StorageResult<u64>;
    async fn stats(&self) -> StorageResult<TextIndexStats>;
    async fn rebuild(&self, scope: IndexRebuildScope) -> StorageResult<TextIndexStats>;
}
```

`SqlAccess` decomposes into `SqlReader`, `SqlWriter`, and `SqlTransaction` subtypes to
enforce read/write/transactional boundaries at the type level.

`EntityStore` is separate from `NoteStore` because Entity and Note are different substrates
(ADR-004) with different field sets, lifecycle rules, and validation paths.

`EntityStore` is separate from `GraphStore` because graph nodes (entities) and graph links
(edges) have different operation shapes. `EntityStore` persists graph nodes; `GraphStore`
persists graph links and supports traversal. This keeps Edge inside the Entity substrate
without creating an `EdgeStore`. When Link gains namespace and timestamps (ADR-004),
`GraphStore` will need namespace-filtered edge queries — the trait surface will evolve
accordingly.

`SparseStore` is separate from `VectorStore` because sparse vectors (BM25 term weights,
SPLADE activations) have different storage layouts, index structures, and query patterns
from dense vectors. Combining them forces backends to implement both or stub one.

`VectorStore` is inherently multi-capable. A record may carry one vector (the common
dense-embedding case) or many vectors (per-token embeddings, per-section embeddings,
per-modality embeddings, per-time-slice embeddings, any aggregation khive or a pack
chooses to store). The capability is "access to vector representations" — record
cardinality is a record-shape concern, not a capability boundary. The trait signature
reflects this: `insert` accepts `Vec<Vec<f32>>`, where the common case is a
single-element outer vec.

The trait does not enumerate retrieval methods. Different backends (and different
khive subsystems) compute relevance from multi-vector records in different ways —
ColBERT-style late interaction is one option, but salience-weighted aggregation,
decay-weighted token mixing, per-section reranking, learned aggregations, and
hand-rolled khive-native recall methods are all valid. The `VectorSearchRequest`
carries query vectors and opaque backend-specific parameters; aggregation strategy
is the backend's responsibility, not the trait's contract. Backends that support
only single-vector records reject multi-vector inserts with `StorageError::Unsupported`.

`EventStore` is append-only by construction. There is no `update_event` or `delete_event`.

### Dispatch: `Arc<dyn Trait>`

All runtime-facing storage dependencies use `Arc<dyn Trait + Send + Sync>`.

The coordinator holds multiple backends of the same trait type (hot and cold `NoteStore`
instances) and selects at runtime based on namespace, kind, or placement policy. This
cannot be monomorphized — the choice happens at runtime. `Arc<dyn Trait>` is the natural
representation.

Generic storage parameters are allowed inside backend implementation crates (`khive-db`),
but not at the runtime/coordinator boundary.

Virtual dispatch overhead is negligible relative to storage I/O.

### Default method policy

Default trait methods are allowed only if they are semantically equivalent to a composition
of required primitive methods and introduce no policy.

**Allowed**: serial batch wrappers (loop-over-get), pagination helpers, idempotent
convenience delegation.

```rust
// Allowed: mechanical delegation with no policy
async fn get_notes_batch(&self, ids: &[Uuid]) -> StorageResult<Vec<Note>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(self.get_note(*id).await?);
    }
    Ok(out)
}
```

**Forbidden in `khive-storage`**: quota enforcement, retention policy, lifecycle
transitions, semantic validation, ranking policy, namespace authorization, backend
placement.

```rust
// FORBIDDEN: quota is namespace policy, not a storage primitive
async fn upsert_note_if_below_quota(&self, note: Note, max: u64) -> StorageResult<bool> {
    // ... this belongs in khive-runtime
}
```

### StorageError: single-backend scope

`StorageError` describes failures from a single backend call. It carries a
`StorageCapability` discriminant identifying which subsystem failed.

```rust
pub enum StorageError {
    NotFound { capability, resource, key },
    AlreadyExists { capability, resource, key },
    Conflict { capability, operation, message },
    InvalidInput { capability, operation, message },
    Unsupported { capability, operation, message },
    Pool { operation, message },
    Timeout { operation },
    Transaction { operation, message },
    Serialization { capability, message },
    IndexMaintenance { capability, message },
    Driver { capability, operation, source },
}
```

`StorageError` does NOT contain federation variants. Cross-backend partial failure,
backend routing failure, and federation commit failure are coordinator errors that wrap
per-backend `StorageError` values:

```rust
// In kkernel or khive-runtime, NOT in khive-storage
pub enum CoordinatorError {
    BackendUnavailable { backend_id, operation, source: StorageError },
    PartialFailure { operation, succeeded, failed: Vec<BackendFailure> },
    Storage { backend_id, source: StorageError },
}
```

### StorageCapability: one variant per trait

`StorageCapability` maps exactly to the eight capability traits. Each variant corresponds
to a real trait. No placeholders.

```rust
pub enum StorageCapability {
    Sql,
    Notes,
    Entities,
    Graph,
    Events,
    Vectors,
    Sparse,
    Text,
}
```

### VectorStore and RuVector

`VectorStore` is the khive-facing contract. RuVector is an implementation detail inside
`khive-db`. Callers depend on `Arc<dyn VectorStore>`, not on RuVector's concrete API.

`khive-storage` does not depend on RuVector types. `VectorSearchHit` carries scores as
`i64` (the raw `DeterministicScore` representation) — the storage layer does not import
the `DeterministicScore` newtype from `ruvector-core`. The runtime converts between the
raw i64 and `DeterministicScore` at the boundary.

`VectorSearchRequest` includes a `filter: Option<FilterExpression>` field as the extension
point for filter pushdown into vector indexes. The `FilterExpression` type is defined in
`khive-storage` as a simple expression tree (`Eq`, `Ne`, `Gt`, `Lt`, `Range`, `In`, `And`,
`Or`, `Not`). When a backend with filter pushdown replaces or supplements sqlite-vec, the backend maps
`FilterExpression` to ruvector-filter's expression tree.

`VectorSearchRequest` carries query vectors (`Vec<Vec<f32>>` — one or many) and an
opaque `backend_hints: Option<serde_json::Value>` field for backend-specific tuning
(aggregation strategy name, decay coefficients, salience weights, learned parameters,
etc.). The trait does not standardize what hints mean across backends — that is each
backend's contract with its callers, documented in the backend crate, not in the
trait surface. ruvector-core HNSW + ColBERT, khive-db's brute-force vector index,
khive's hand-rolled recall pipelines, and future backends each define their own
hint vocabularies.

A future RuVector integration ADR may replace `VectorStore` with a thinner `VectorIndex`
trait if RuVector's public API stabilizes. The hint mechanism is the seam that makes
this possible without locking khive into ruvector's aggregation taxonomy.

### Index choice is a backend concern, not a trait

Different vector index data structures — Flat (brute force), HNSW, IVF, IVF-PQ, ScaNN,
DiskANN, GNN-based learned indexes — are different `VectorStore` implementations, not
different traits. The capability surface is one: "store vectors, return top-k similar
results." Each backend picks its index strategy.

```text
khive-db current: Flat / brute-force via sqlite-vec
retrieval crates shipped: HNSW via khive-hnsw and Vamana via khive-vamana; backend integration remains explicit, not implied by khive-db
hypothetical future backends: IVF-PQ, DiskANN, GNN-based, learned
```

Index-specific tuning (HNSW's `ef_search`, IVF's `nprobe`, multi-vector's aggregation)
flows through `VectorSearchRequest.backend_hints`. Index selection itself is deployment
configuration — `khive.toml` declares which backend a pack uses (ADR-003); each backend
binds to one index implementation.

Adding `HnswVectorStore` / `IvfVectorStore` traits would force every retrieval pipeline
to branch on index type. The retrieval composition layer (ADR-012) is index-agnostic by
design: it consumes `Arc<dyn VectorStore>` and composes hits, indifferent to what
algorithm produced them.

The same principle applies to sparse and text indexes. `SparseStore` abstracts over
SPLADE, learned BM25, inverted index variants. `TextSearch` abstracts over FTS5,
Tantivy, Lucene-style implementations. The capability is the abstraction; the index
is the backend's choice.

## Crate Boundary

| Belongs in `khive-storage`                                               | Does NOT belong in `khive-storage`              |
| ------------------------------------------------------------------------ | ----------------------------------------------- |
| Eight capability traits                                                  | `StorageProfile` / `PlacementRole`              |
| Storage-facing record types (`Note`, `Entity`, `Edge`, `Event`)          | `BackendId` / `BackendHandle`                   |
| Pagination / result types (`Page`, `PageRequest`, `BatchWriteSummary`)   | ATTACH schema aliases (`SqlScope`)              |
| Single-backend `StorageError`                                            | `CoordinatorError` / federation errors          |
| `StorageCapability` enum (8 variants)                                    | `CoordinatorStore` / `StorageCoordinator` trait |
| Mechanical default methods                                               | Policy default methods (quota, retention)       |
| Filter types (`EntityFilter`, `EdgeFilter`, `EventFilter`, `TextFilter`) | `registered_kinds()` / kind discovery           |
| Shared types (`SqlValue`, `SqlRow`, `VectorSearchHit`, `TextSearchHit`)  | Placement / routing logic                       |

### What `SqlAccess` does not own

`SqlAccess` executes complete SQL statements. It does not expose backend topology, ATTACH
aliases, or placement metadata.

When SQLite ATTACH is used for multi-backend queries, the SubstrateCoordinator owns the
schema alias through its backend metadata and constructs schema-qualified table names before
calling `SqlAccess`. The coordinator builds `SELECT * FROM hot.entities WHERE ...`; `SqlAccess`
runs it. `SqlAccess` never knows it is part of a multi-backend deployment.

### What stores do not validate

Storage traits are kind-agnostic. A backend stores and filters `kind` / `entity_type` columns,
but does not decide whether a kind is valid. It does not advertise which kinds it serves.

Kind validation, entity_type normalization, edge endpoint validation, and namespace
enforcement live in `khive-runtime`. The `EntityTypeRegistry` (ADR-001) and endpoint
validator (ADR-002) are runtime constructs. Backends persist what the runtime has already
validated.

If future placement routes by note kind or entity type, the coordinator uses boot-time
registry + placement metadata from `khive.toml`, not store-trait discovery.

## Rationale

### Why trait-only?

If `khive-storage` contained SQLite code, every crate depending on it would transitively
depend on `rusqlite`, `sqlite-vec`, and FTS5 headers. The trait crate has zero heavy
dependencies — it is pure contracts. Backend crates bring their own dependencies.

### Why eight traits (not fewer)?

Each trait maps to a substrate or index capability with distinct operation shapes:

| Trait         | Record type    | Key operations                    | Why separate                                           |
| ------------- | -------------- | --------------------------------- | ------------------------------------------------------ |
| `NoteStore`   | `Note`         | CRUD, kind filter, temporal query | Temporal/cognitive substrate                           |
| `EntityStore` | `Entity`       | CRUD, kind+type filter            | Graph node substrate                                   |
| `GraphStore`  | `Edge`         | Link CRUD, neighbors, traversal   | Graph link operations                                  |
| `EventStore`  | `Event`        | Append-only, no update/delete     | Audit substrate                                        |
| `VectorStore` | `VectorRecord` | Insert, search, rebuild           | Dense vector index (single or multi-vector per record) |
| `SparseStore` | `SparseRecord` | Insert, search                    | Sparse vector index                                    |
| `TextSearch`  | `TextDocument` | Upsert, search, stats             | Full-text index                                        |
| `SqlAccess`   | `SqlRow`       | Raw SQL, transactions             | Escape hatch                                           |

Collapsing EntityStore into NoteStore would merge two substrates with different field sets
(`entity_type` vs `salience`/`decay_factor`), different validation paths
(`EntityTypeRegistry` vs `NoteKindSpec`), and different lifecycle rules.

Collapsing EntityStore into GraphStore would merge node CRUD (key-value with typed fields)
with edge CRUD (source/target resolution, endpoint validation, traversal) — semantically
unrelated operations on the same trait.

### Why placement-blind?

If placement logic (hot/cold/frozen) leaks into the trait crate, backends must declare tier
awareness. This couples the abstraction boundary to an operational model that varies per
deployment. A test backend and a production backend should implement the same trait surface.

### Why single-backend errors?

`StorageError` and `CoordinatorError` have different recovery semantics. A single-backend
`NotFound` means the record doesn't exist. A federation `PartialFailure` means one backend
succeeded and another failed — the correct response might be rollback, retry, or degraded
read. Mixing them into one error type forces callers to handle federation semantics even
when talking to a single backend.

### Why `Arc<dyn>` and not generics?

Multi-backend requires runtime selection. A coordinator holding two `NoteStore` instances
(hot and cold) and selecting based on namespace cannot be expressed as
`Coordinator<HotNote: NoteStore, ColdNote: NoteStore, ...>` — the type parameter list
compounds with each backend. `Arc<dyn NoteStore>` in a `Vec<BackendBinding<dyn NoteStore>>`
is the natural representation.

## Consequences

### Positive

- Runtime, packs, and coordinator compile without `rusqlite` on the dependency tree.
- Swapping or mocking backends requires no recompilation of upstream crates.
- `StorageCapability` diagnostics are precise — every variant maps to a real trait.
- Default method policy is explicit — future contributors know the line.
- Federation complexity stays above the trait layer.

### Negative

- Eight traits is more implementation surface per backend. Each backend crate implements
  eight traits plus `SqlReader`/`SqlWriter`/`SqlTransaction`.
- `Arc<dyn Trait>` precludes compile-time specialization inside the runtime. Backends
  that could benefit from monomorphization (e.g., a hot-path HNSW search) must absorb
  the virtual dispatch cost.

### Neutral

- `StorageError` variant set is unchanged by this ADR.
- No new dependencies added to `khive-storage`.

## Implementation

- `crates/khive-storage/src/lib.rs`: re-exports all eight traits plus shared types.
- One source file per trait: `sql.rs`, `note.rs`, `entity.rs`, `graph.rs`, `event.rs`,
  `vectors.rs`, `sparse.rs`, `text.rs`.
- `capability.rs`: `StorageCapability` enum (8 variants).
- `error.rs`: `StorageError` with `StorageCapability` discriminant.
- `types.rs`: shared types (`SqlRow`, `SqlValue`, `Page`, `BatchWriteSummary`, filter
  types, hit types).
