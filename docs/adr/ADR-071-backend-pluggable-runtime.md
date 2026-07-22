# ADR-071: Backend-Pluggable Runtime and Polystore Boundary

**Status**: Accepted
**Date**: 2026-06-25
**Ratified**: 2026-06-28
**Authors**: khive maintainers
**Depends on**:

- [ADR-005](./ADR-005-storage-capability-traits.md): Storage Capability Traits
- [ADR-009](./ADR-009-backend-architecture.md): Backend Architecture
- [ADR-015](./ADR-015-schema-migrations.md): Schema Migrations
- [ADR-028](./ADR-028-pack-scoped-backends.md): Pack-Scoped Backends
- [ADR-043](./ADR-043-embedding-model-migration.md): Embedding Model Migration
- [ADR-044](./ADR-044-vector-store-extensions.md): Vector Store Extensions

---

## Context

ADR-005 specifies the polystore design: `khive-storage` defines eight capability traits
(`SqlAccess`, `EntityStore`, `NoteStore`, `GraphStore`, `EventStore`, `VectorStore`,
`SparseStore`, `TextSearch`) and every crate above depends only on those traits, never
on the concrete `khive-db` SQLite backend. ADR-005 §consequences states:

> "Runtime, packs, and coordinator compile without `rusqlite` on the dependency tree."

ADR-009 §architecture states:

> "The runtime and packs depend on traits, not on any specific backend crate."

This ADR makes that boundary concrete through seven requirements:

| ID | Accepted boundary requirement                                                                   |
| -- | ----------------------------------------------------------------------------------------------- |
| G1 | `KhiveRuntime` owns backend-neutral capability handles rather than a concrete database wrapper. |
| G2 | Migration dispatch uses a backend-neutral migrator contract.                                    |
| G3 | Transactions and backend-specific operations remain behind storage traits.                      |
| G4 | Runtime errors expose `StorageError`, not a concrete database error type.                       |
| G5 | Runtime APIs return runtime-owned domain types.                                                 |
| G6 | Optional database adapters in retrieval are feature-gated.                                      |
| G7 | Capability defaults contain no SQLite-specific limits or index kinds.                           |

Together these requirements allow an alternate backend to be connected without changing
the runtime's public API or pack implementations.

---

## Decision

### 1. `BackendHandle`: the single runtime seam

`KhiveRuntime` will hold a `BackendHandle` instead of `Arc<StorageBackend>`:

```rust
// crates/khive-runtime/src/backend_handle.rs
pub struct BackendHandle {
    // Required core: every backend stores entities and notes, exposes the graph and
    // event surfaces the runtime's core write/projection paths depend on, provides raw
    // SQL access, and carries a migrator.
    entity:   Arc<dyn EntityStore>,
    note:     Arc<dyn NoteStore>,
    graph:    Arc<dyn GraphStore>,
    event:    Arc<dyn EventStore>,
    sql:      Arc<dyn SqlAccess>,
    /// Required on every handle. An in-memory or ephemeral backend provides a migrator
    /// whose `migrate()` applies all migrations automatically and whose
    /// `current_version()` returns the latest schema version.
    migrator: Arc<dyn BackendMigrator>,
    // Optional retrieval tier: a backend that does not provide semantic/lexical search
    // leaves these `None`. The matching accessors return
    // `Option`; the runtime validates presence per operation and fails with a clear
    // diagnostic if an op needs a tier the bound backend does not provide.
    vector:   Option<Arc<dyn VectorStore>>,
    sparse:   Option<Arc<dyn SparseStore>>,
    text:     Option<Arc<dyn TextSearch>>,
}

impl BackendHandle {
    /// Construct from a concrete StorageBackend (the shipped path).
    pub fn from_sqlite(backend: Arc<StorageBackend>) -> Self { ... }

    /// Construct from explicit trait objects (alternate backends, tests).
    /// Pass `None` for any retrieval-tier slot (`vector`, `sparse`, `text`) the backend
    /// does not provide; the required-core slots are mandatory.
    pub fn from_parts(
        entity:   Arc<dyn EntityStore>,
        note:     Arc<dyn NoteStore>,
        graph:    Arc<dyn GraphStore>,
        event:    Arc<dyn EventStore>,
        sql:      Arc<dyn SqlAccess>,
        migrator: Arc<dyn BackendMigrator>,
        vector:   Option<Arc<dyn VectorStore>>,
        sparse:   Option<Arc<dyn SparseStore>>,
        text:     Option<Arc<dyn TextSearch>>,
    ) -> Self { ... }
}
```

`BackendHandle` lives in `crates/khive-runtime` and carries `Arc<dyn Trait>` handles only.
It is produced by the boot path and is the only way to construct a `KhiveRuntime`.

`KhiveRuntime` becomes:

```rust
pub struct KhiveRuntime {
    handle:      BackendHandle,
    /// `None` when this runtime is bound to the main (shared-graph) backend.
    /// `Some(main_handle)` when bound to a secondary backend; `core()` returns
    /// a runtime backed by the main handle. See ADR-073 §2 for the full contract.
    core_handle: Option<BackendHandle>,
    embedders:   Arc<EmbedderRegistry>,
}
```

`backend: Arc<StorageBackend>` is removed. `KhiveRuntime::backend()` is removed. Code that
reached through `backend()` must use the specific trait accessor on `BackendHandle` instead.
The required-core accessors (`entity()`, `note()`, `graph()`, `event()`, `sql()`,
`migrator()`) return the handle directly; the retrieval-tier accessors (`vector()`,
`sparse()`, `text()`) return `Option<&Arc<dyn _>>`. A runtime operation that needs a
retrieval tier the bound backend does not provide fails with a clear diagnostic naming the
missing capability rather than panicking. The SQLite backend fills every slot, so this is
the common path; the `Option` exists for partial backends.

`core_handle` preserves the ADR-073 accessor contract. `KhiveRuntime::core()` returns a
handle bound to the main backend; `with_core_handle(BackendHandle)` is the boot-path
wiring call for secondary-backend runtimes. The backend-handle transition replaces
`Option<Arc<StorageBackend>>` with `Option<BackendHandle>` without changing `core()` semantics.

### 2. Migration dispatch: `BackendMigrator` trait

The runtime boot path must not depend on a concrete database connection type. Migration
dispatch therefore uses a storage-level contract.

A `BackendMigrator` trait in `khive-storage` replaces the direct function call:

```rust
// crates/khive-storage/src/migrations.rs
pub trait BackendMigrator: Send + Sync {
    /// Apply all pending migrations idempotently.
    /// Returns the schema version after applying.
    fn migrate(&self) -> StorageResult<u32>;

    /// Return the current persisted schema version without migrating.
    fn current_version(&self) -> StorageResult<u32>;
}
```

`khive-db` provides a `SqliteMigrator` that wraps `run_migrations` and implements this trait.
The `BackendHandle` includes an `Arc<dyn BackendMigrator>` in its `migrator` slot (see §1).

Migration dispatch at boot follows ADR-015 §Decision: the MCP binary does not apply
migrations at startup:

- **File-backed runtimes**: `KhiveRuntime::boot()` calls
  `handle.migrator().current_version()` and fails fast with a diagnostic pointing at
  `kkernel db migrate` if the schema version is behind the codebase expectation.
  `migrate()` is not called at MCP startup; it is reserved for `kkernel db migrate`.
- **In-memory and ephemeral backends**: `boot()` calls `handle.migrator().migrate()` to
  apply all migrations automatically. There is no operator to invoke `kkernel db migrate`
  for an ephemeral database, and the migration cost is negligible against an empty store.

The runtime crate's dependency on `rusqlite` is removed when this gap closes. Only
`khive-db` depends on `rusqlite`.

### 3. Curation and operations: eliminate raw `rusqlite::Connection` use

Runtime curation and operation code must not reach through a storage pool to raw database
connections. Three categories of operation belong behind traits:

**Pattern A** (merge operations in `curation.rs`): `fn read_merge_entity(conn: &rusqlite::Connection, ...)`.
These must move to `khive-db` as methods on `SqliteEntityStore` (or similar), exposed
through the `EntityStore` trait under a new `merge_read` capability or through `SqlAccess`.

**Pattern B** (upsert in `operations.rs`): Multi-step entity/note upsert using
`rusqlite::params!`. These must be rewritten to use the `EntityStore::upsert` and
`NoteStore::upsert` trait methods, which the `SqliteEntityStore` implements with the same
underlying transaction semantics.

**Pattern C** (graph operations): Batch graph write operations that open a writer
transaction and execute several raw SQL statements. These must be pushed into
`GraphStore::batch_write` or equivalent methods on `GraphStore`, implemented in
`khive-db`'s concrete store.

None of these changes alter observable behavior. They relocate `rusqlite`-specific code
from `khive-runtime` to `khive-db`, where it belongs.

### 4. `RuntimeError`: remove the `Sqlite` variant

```rust
// Before:
pub enum RuntimeError {
    Sqlite(#[from] khive_db::SqliteError),
    // ...
}

// After:
pub enum RuntimeError {
    Storage(#[from] khive_storage::StorageError),
    // ...
}
```

`StorageError` already exists in `khive-storage` and is the correct abstraction. All
`SqliteError` instances produced by storage operations are wrapped by the trait
implementations in `khive-db` and surface as `StorageError` at the trait boundary.
Meeting G1 and G3 ensures that concrete database errors are converted before they enter
the runtime crate.

The `#[from] khive_db::SqliteError` attribute is the dependency injection point that
brought `khive-db` into the runtime's error type. Replacing it with `#[from] StorageError`
removes the last `khive-db` dependency from the runtime's error handling.

### 5. `list_embedding_models`: return a runtime-owned type

`KhiveRuntime::list_embedding_models` returns a runtime-owned type so the public API does
not expose a concrete database record:

```rust
// crates/khive-runtime/src/embedding.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelRecord {
    pub id:            Uuid,
    pub engine_name:   String,
    pub model_id:      String,
    pub key_version:   String,
    pub dim:           u32,
    pub output_dim:    Option<u32>,
    pub status:        EmbeddingModelStatus,
    pub activated_at:  Option<i64>,
    pub superseded_at: Option<i64>,
    pub superseded_by: Option<Uuid>,
    pub canonical_key: Vec<u8>,
    pub created_at:    i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModelStatus {
    Pending,
    Active,
    Superseded,
    Archived,
}
```

`khive_db::EmbeddingModelRegistryRecord` maps to `EmbeddingModelRecord` at the boundary
in `khive-db`'s implementation of the query method. The runtime public API surface does
not expose any `khive-db` types after this change.

### 6. `khive-retrieval`: feature-gate the `khive-db` dependency

`khive-db` is a feature-gated dependency in `khive-retrieval`:

```toml
[dependencies]
# ...

[features]
default = []
sqlite-backend = ["khive-db", "khive-db/vectors"]
storage-adapters = ["khive-storage", "sqlite-backend"]
```

All code paths in `khive-retrieval` that depend on `khive-db` move behind `#[cfg(feature = "sqlite-backend")]`.
The default build of `khive-retrieval` carries no `khive-db` dependency.

### 7. `VectorStore::capabilities()` default: remove SQLite assumptions

The backend-neutral `capabilities()` default must not declare concrete SQLite limits:

```rust
VectorStoreCapabilities {
    max_dimensions: Some(8192),          // sqlite-vec 0.1.9 limit
    index_kinds:    vec![VectorIndexKind::SqliteVec],
    // ...
}
```

Both of these are SQLite-specific values. A backend-neutral default must not encode them:

```rust
fn capabilities(&self) -> &'static VectorStoreCapabilities {
    static BASELINE: OnceLock<VectorStoreCapabilities> = OnceLock::new();
    BASELINE.get_or_init(|| VectorStoreCapabilities {
        supports_filter:        false,
        supports_batch_search:  false,
        supports_quantization:  false,
        supports_update:        false,
        supports_orphan_sweep:  false,
        supports_multi_field:   false,
        max_dimensions:         None,      // no assumption; backend declares
        index_kinds:            vec![],    // no assumption; backend declares
    })
}
```

`khive-db`'s `SqliteVecStore` overrides `capabilities()` and returns the SQLite-specific
values (`SqliteVec`, `max_dimensions: Some(8192)`). The trait default stays neutral.

`VectorIndexKind::SqliteVec` is NOT removed from the enum. It is the correct discriminant
for the sqlite-vec backend; it just must not appear in the trait default.

---

## Rationale

### Why `BackendHandle` over a single `Arc<dyn Backend>` supertrait

A supertrait that extends all eight capability traits forces every backend implementation
to implement all eight. An alternate backend that provides the relational core but no
semantic or lexical search would be forced to provide stub implementations of
`VectorStore`, `SparseStore`, and `TextSearch`. `BackendHandle` holds individual handles
and splits them into a required core (`entity`, `note`, `graph`, `event`, `sql`, `migrator`
: the slots every khive backend must supply because the runtime's core write, projection,
and migration paths depend on them) and an optional retrieval tier (`vector`, `sparse`,
`text`: `Option` slots). A backend that omits the retrieval tier leaves those slots `None`
rather than stubbing them; the matching accessors return `Option`, and a runtime operation
that requires an absent tier fails with a diagnostic naming the missing capability.

### Why not just feature-gate `khive-db` out of `khive-runtime`

Feature-gating alone leaves the same `Arc<StorageBackend>` field in `KhiveRuntime` and
all the raw `rusqlite::Connection` bypasses in `curation.rs` and `operations.rs`. Those
bypasses exist because the trait layer does not yet expose the operations they perform.
The trait-level requirements G3 and G7 are prerequisites for removing the crate-level
dependency; feature gates do not replace the abstraction boundary.

### Why `BackendMigrator` trait and not versioned migration files per backend

Each backend has fundamentally different migration mechanics: SQLite runs DDL statements
in `rusqlite::Connection` transactions; a future RocksDB backend would run column-family
schema declarations; an in-memory backend might be a no-op. A `BackendMigrator` trait
surfaces those differences at the correct abstraction level without coupling the runtime
boot path to any specific mechanism. The ADR-015 `VersionedMigration` / `run_migrations`
mechanism remains the correct implementation inside `khive-db`: the trait is a thin
wrapper around it.

### Why replace `RuntimeError::Sqlite` with `RuntimeError::Storage` rather than keeping both

Keeping both variants means every error consumer that matches on `RuntimeError` must handle
both. As more backend implementations arrive, this list grows (`RuntimeError::RocksDb`,
`RuntimeError::Redis`, ...). `StorageError` already exists in `khive-storage` and is the
backend-neutral error type. All concrete error types (`SqliteError`, future backends) map
into `StorageError` at the trait implementation layer. One `RuntimeError::Storage` variant
is sufficient.

### Why `EmbeddingModelRecord` in `khive-runtime` rather than moving the type to `khive-storage`

`EmbeddingModelRecord` describes a domain concept (an embedding model known to a
deployment) that belongs to the runtime's embedding management subsystem, not to the
storage trait layer. `khive-storage` is a capability-contract crate; it should not carry
domain types. The record lives in `khive-runtime`; `khive-db` produces it from a SQL row
and returns it via a conversion function.

### Relationship to ADR-028

ADR-028's deferred `[[backends]]` boot path already anticipates `KhiveRuntime::from_backend(Arc<StorageBackend>, ...)`.
ADR-071 amends that to `KhiveRuntime::from_handle(BackendHandle, ...)`. The `BackendHandle::from_sqlite(Arc<StorageBackend>)` constructor preserves the SQLite fast path. The `[[backends]]` declarative boot sequence ADR-028 §8 describes constructs a `BackendHandle` per pack rather than an `Arc<StorageBackend>`.

ADR-028 is amended (see below) to reference ADR-071 as the seam that `[[backends]]` plugs into.

### Relationship to ADR-073

ADR-073 (accepted) adds `core_handle: Option<BackendHandle>` to `KhiveRuntime` and the
`core()` / `with_core_handle` accessor API that lets a secondary-backend pack write
shared-graph notes to the main backend. This ADR preserves that contract:
the earlier `core_backend: Option<Arc<StorageBackend>>` form becomes
`core_handle: Option<BackendHandle>` at the `BackendHandle` seam. The
`BackendHandle::from_sqlite(Arc<StorageBackend>)` shim provides the mechanical upgrade
path at the boot-path call site. ADR-073 §6 ("Relationship to ADR-071") records this
as the accepted sequencing constraint.

### Relationship to ADR-005 and ADR-009

ADR-005 and ADR-009 do not require amendment. They correctly specify the intended design.
The code must be brought to the ADRs, not the reverse.

---

## Alternatives Considered

### A. Declare the breach acceptable; do not repair

The shipped SQLite backend is sufficient, the tests pass, and backend-neutral traits add
internal complexity without an immediate user-visible effect.

Rejected. `curation.rs` and `operations.rs` bypass the trait
layer via raw `rusqlite::Connection` closures. This means correctness of those code paths
can only be verified by reading `rusqlite` internals, not by the trait contracts. As the
codebase grows, this becomes a maintenance liability. More concretely, ADR-028's planned
multi-backend topology cannot be realized without G1: a `KhiveRuntime` that holds
`Arc<StorageBackend>` cannot route to a second backend regardless of what the TOML config
says.

### B. Introduce a `StorageBackend` trait (not a struct)

Convert `khive_db::StorageBackend` from a concrete struct into a trait, with
`SqliteStorageBackend` implementing it. Keep `KhiveRuntime { backend: Arc<dyn StorageBackend> }`.

Rejected. A `StorageBackend` trait that returns all eight `Arc<dyn CapabilityTrait>` handles
is functionally equivalent to `BackendHandle`. The difference is that a named trait imposes
a single-implementor contract on backends while `BackendHandle::from_parts` allows the
flexible per-slot construction described in §1 rationale. `BackendHandle` is simpler and
directly maps to what the runtime actually needs.

### C. Separate `khive-runtime` into `khive-runtime-core` (trait-only) and `khive-runtime-sqlite` (concrete)

Split the runtime into two crates: a trait-only core that pack handlers depend on, and a
concrete implementation that depends on `khive-db`. Packs depend on the core; only the MCP
binary depends on the concrete crate.

Considered and deferred. This is the correct long-term shape for multi-binary deployments
(separate `khive-mcp` and a hypothetical `khive-http` binary sharing the same core). For
v1 there is one binary. The repair described in §1-7 achieves the same trait isolation with
a single crate reorganization rather than a crate split. The crate split is tracked as a
follow-up; it requires ADR-003 amendment.

### D. Accept `rusqlite` in `khive-runtime` but gate it behind a Cargo feature

Feature `sqlite` controls the `rusqlite` dependency and all direct-connection bypasses.
Other builds compile without it.

Partially adopted (see §6 for `khive-retrieval`). Insufficient for `khive-runtime` because
the `Arc<StorageBackend>` field (G1) and the `RuntimeError::Sqlite` variant (G4) affect all
callers regardless of feature flags. Feature-gating does not change the API surface; a
clean API requires the structural changes in §1-5.

---

## Implementation sequencing constraints

The requirements have a dependency order, but file-level work planning is not part of this ADR.

1. Establish neutral capability defaults and the `BackendMigrator` and `StorageError` contracts.
2. Move backend-specific transactions behind the storage traits while preserving observable
   operation semantics.
3. Change `KhiveRuntime` to `BackendHandle` and update the ADR-073 core accessor at the same
   boundary.
4. Remove concrete database dependencies from the runtime after no public type or operation path
   exposes them.
5. Feature-gate optional retrieval adapters and verify a trait-only build.

Each step must leave the SQLite-backed test suite passing. The final acceptance gate is that
`khive-runtime` and the default `khive-retrieval` build do not depend on `rusqlite` or `khive-db`,
while the SQLite implementation continues to provide all required capabilities.

---

## Consequences

### Positive

- `khive-runtime` compiles without `rusqlite`. The polystore
  boundary specified in ADR-005 and ADR-009 is restored.
- ADR-028's `[[backends]]` multi-backend boot path can be implemented: it constructs
  a `BackendHandle` per pack assignment rather than an `Arc<StorageBackend>`.
- An alternate storage backend (e.g., an in-memory backend for tests, a future
  RocksDB backend for the archive tier) can be connected without modifying
  `khive-runtime`.
- The `curation.rs` and `operations.rs` bypasses are replaced by trait-level
  operations. Correctness can be audited at the trait boundary.
- `RuntimeError::Storage` unifies error handling across all backends.

### Negative

- Relocating raw SQL operations is a non-trivial refactor. It must not change
  behavior and requires comprehensive test coverage on the affected paths.
- `BackendHandle::from_sqlite` is a convenience shim that will be removed when the
  multi-backend boot path (ADR-028) lands. It adds a layer that is temporary.
- All callers of `KhiveRuntime::backend()` must be updated to use the specific
  `BackendHandle` accessor. The number of such callers is bounded (they are all within
  `khive-runtime`) but must be audited.

### Neutral

- The SQLite backend's behavior is unchanged. All current tests pass without
  modification to test code.
- The MCP wire protocol is unchanged.
- Pack verb handlers are unchanged: they depend on `KhiveRuntime`'s public API,
  not on the backend field directly.
- ADR-015 migration mechanics are unchanged inside `khive-db`; only the call site in
  the runtime boot path changes.

---

## References

- [ADR-005](./ADR-005-storage-capability-traits.md): the polystore specification this ADR restores
- [ADR-009](./ADR-009-backend-architecture.md): backend architecture; reaffirmed, not amended
- [ADR-015](./ADR-015-schema-migrations.md): migration system; amended to reference `BackendMigrator`
- [ADR-028](./ADR-028-pack-scoped-backends.md): pack-scoped backends; amended to reference `BackendHandle`
- [ADR-043](./ADR-043-embedding-model-migration.md): amended to note `EmbeddingModelRecord` type change
- [ADR-044](./ADR-044-vector-store-extensions.md): amended to note `capabilities()` default correction
