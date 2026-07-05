# ADR-071: Backend-Pluggable Runtime — Polystore Restoration

**Status**: Accepted
**Date**: 2026-06-25
**Ratified**: 2026-06-28
**Authors**: khive maintainers
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — Storage Capability Traits
- [ADR-009](ADR-009-backend-architecture.md) — Backend Architecture
- [ADR-015](ADR-015-schema-migrations.md) — Schema Migrations
- [ADR-028](ADR-028-pack-scoped-backends.md) — Pack-Scoped Backends
- [ADR-043](ADR-043-embedding-model-migration.md) — Embedding Model Migration
- [ADR-044](ADR-044-vector-store-extensions.md) — Vector Store Extensions

---

## Context

ADR-005 specifies the polystore design: `khive-storage` defines eight capability traits
(`SqlAccess`, `EntityStore`, `NoteStore`, `GraphStore`, `EventStore`, `VectorStore`,
`SparseStore`, `TextSearch`) and every crate above depends only on those traits, never
on the concrete `khive-db` SQLite backend. ADR-005 §consequences states:

> "Runtime, packs, and coordinator compile without `rusqlite` on the dependency tree."

ADR-009 §architecture states:

> "The runtime and packs depend on traits, not on any specific backend crate."

A storage-layer drift audit conducted 2026-06-25 (`docs/audits/20260625/storage-backend-drift.md`)
found that the current `khive-runtime` crate violates both specifications. The breach is
not incremental erosion — it has been present since the initial commit (`16d75d9a`). Seven
gaps were identified, ordered by severity:

| Gap | Description                                                                                                                                                              |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| G1  | `KhiveRuntime` holds `Arc<StorageBackend>` (the concrete SQLite wrapper), not a trait handle                                                                             |
| G2  | `run_migrations` takes `rusqlite::Connection` directly; no backend-neutral migration path exists                                                                         |
| G3  | `curation.rs` and `operations.rs` call `backend.pool_arc().writer().transaction(conn: &rusqlite::Connection)` — bypassing every trait                                    |
| G4  | `RuntimeError::Sqlite` is typed as `#[from] khive_db::SqliteError`, coupling the public error enum to the SQLite backend                                                 |
| G5  | `list_embedding_models` returns `Vec<khive_db::EmbeddingModelRegistryRecord>` — a concrete `khive-db` type in the public API                                             |
| G6  | `khive-retrieval` takes `khive-db` as an unconditional production dependency                                                                                             |
| G7  | `VectorStore::capabilities()` default returns `VectorIndexKind::SqliteVec` and `max_dimensions: Some(8192)`, embedding SQLite constraints into the backend-neutral trait |

The consequence of G1 is that the entire trait layer in `khive-storage` is structurally
correct but architecturally isolated: all actual data paths go through the concrete backend
rather than through the traits it implements. This defeats the purpose of the trait layer.

No alternate storage backend can be connected today without modifying `khive-runtime`. The
runtime cannot be compiled without `rusqlite`. The ADR design contract specifies otherwise.

This ADR decides how to close all seven gaps and restore the polystore boundary.

---

## Decision

### 1. `BackendHandle` — the single runtime seam

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
    // (e.g. a session-only or secondary backend that delegates shared-graph reads to the
    // main backend via `core()`) leaves these `None`. The matching accessors return
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
wiring call for secondary-backend runtimes. Phase 4 updates the field type from
`Option<Arc<StorageBackend>>` (the ADR-073 pre-Phase-4 form) to `Option<BackendHandle>`;
the `core()` accessor semantics are unchanged.

### 2. Migration dispatch — `BackendMigrator` trait

The current `run_migrations(conn: &mut rusqlite::Connection)` function in `khive-db` is
called directly from the runtime boot path and takes a `rusqlite::Connection`. This
hardcodes the migration contract to the SQLite backend.

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

Migration dispatch at boot follows ADR-015 §Decision — the MCP binary does not apply
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

### 3. Curation and operations — eliminate raw `rusqlite::Connection` use

`crates/khive-runtime/src/curation.rs` and `operations.rs` contain calls that reach
through the `ConnectionPool` to raw `rusqlite::Connection` closures. These are the G3
bypasses. Three patterns exist:

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

### 4. `RuntimeError` — remove the `Sqlite` variant

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
`RuntimeError::Sqlite` receives them only because the runtime currently holds the
concrete backend and calls it directly. Removing G1 and G3 eliminates all sites that
produce `SqliteError` inside the runtime crate.

The `#[from] khive_db::SqliteError` attribute is the dependency injection point that
brought `khive-db` into the runtime's error type. Replacing it with `#[from] StorageError`
removes the last `khive-db` dependency from the runtime's error handling.

### 5. `list_embedding_models` — return a runtime-owned type

`KhiveRuntime::list_embedding_models` currently returns
`RuntimeResult<Vec<khive_db::EmbeddingModelRegistryRecord>>`. This leaks the concrete
`khive_db` struct into the runtime's public API surface.

A runtime-owned type replaces it:

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

### 6. `khive-retrieval` — feature-gate the `khive-db` dependency

`khive-retrieval` currently takes `khive-db` as an unconditional production dependency in
`Cargo.toml`. The feature `storage-adapters` exists but gates only the `khive-storage`
and `khive-db/vectors` sub-dep, not the base `khive-db` dependency.

The repair: `khive-db` becomes a feature-gated dependency in `khive-retrieval`:

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

### 7. `VectorStore::capabilities()` default — remove SQLite assumptions

The current default `capabilities()` implementation in `khive-storage/src/vectors.rs` returns:

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
semantic or lexical search (e.g., a secondary session backend that delegates shared-graph
reads to the main backend via `core()`) would be forced to provide stub implementations of
`VectorStore`, `SparseStore`, and `TextSearch`. `BackendHandle` holds individual handles
and splits them into a required core (`entity`, `note`, `graph`, `event`, `sql`, `migrator`
— the slots every khive backend must supply because the runtime's core write, projection,
and migration paths depend on them) and an optional retrieval tier (`vector`, `sparse`,
`text` — `Option` slots). A backend that omits the retrieval tier leaves those slots `None`
rather than stubbing them; the matching accessors return `Option`, and a runtime operation
that requires an absent tier fails with a diagnostic naming the missing capability. This is
the seam the cloud session-store decision relies on: cloud plugs in its own backend without
modifying `runtime.core()` and without implementing search traits it does not use.

### Why not just feature-gate `khive-db` out of `khive-runtime`

Feature-gating alone leaves the same `Arc<StorageBackend>` field in `KhiveRuntime` and
all the raw `rusqlite::Connection` bypasses in `curation.rs` and `operations.rs`. Those
bypasses exist because the trait layer does not yet expose the operations they perform.
The trait-level repair (G3, G7) must precede the crate-level dependency removal, not
replace it.

### Why `BackendMigrator` trait and not versioned migration files per backend

Each backend has fundamentally different migration mechanics: SQLite runs DDL statements
in `rusqlite::Connection` transactions; a future RocksDB backend would run column-family
schema declarations; an in-memory backend might be a no-op. A `BackendMigrator` trait
surfaces those differences at the correct abstraction level without coupling the runtime
boot path to any specific mechanism. The ADR-015 `VersionedMigration` / `run_migrations`
mechanism remains the correct implementation inside `khive-db` — the trait is a thin
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
shared-graph notes to the main backend. Phase 4 of this ADR must preserve that contract:
the `core_backend: Option<Arc<StorageBackend>>` field introduced by ADR-073 becomes
`core_handle: Option<BackendHandle>` when the `BackendHandle` seam lands. The
`BackendHandle::from_sqlite(Arc<StorageBackend>)` shim provides the mechanical upgrade
path at the boot-path call site. ADR-073 §6 ("Relationship to ADR-071") records this
as the accepted sequencing constraint.

### Relationship to ADR-005 and ADR-009

ADR-005 and ADR-009 do not require amendment. They correctly specify the intended design.
The code must be brought to the ADRs, not the reverse.

---

## Alternatives Considered

### A. Declare the breach acceptable; do not repair

The sqlite-vec backend is the only backend in production; all tests pass; the traits in
`khive-storage` are correct. The repair is internal churn with no user-visible effect.

Rejected. The drift audit found that `curation.rs` and `operations.rs` bypass the trait
layer via raw `rusqlite::Connection` closures. This means correctness of those code paths
can only be verified by reading `rusqlite` internals, not by the trait contracts. As the
codebase grows, this becomes a maintenance liability. More concretely, ADR-028's planned
multi-backend topology cannot be realized without closing G1 — a `KhiveRuntime` that holds
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

## Boundary-Repair Plan

The seven gaps close in dependency order. G7 is independent and merges first. G2 and G4
are preconditions for G1. G3 is the largest single unit of work and must land before G1.

### Phase 1 — Trait neutral defaults (G7)

Change `VectorStore::capabilities()` default in `crates/khive-storage/src/vectors.rs` to
return `max_dimensions: None` and `index_kinds: vec![]`. Override `capabilities()` in
`crates/khive-db/src/stores/vectors.rs` to return the SQLite-specific values. Compile and
test. No behavioral change.

Estimated scope: 2 files, ~15 LOC change.

### Phase 2 — `BackendMigrator` trait and `RuntimeError` swap (G2, G4)

Add `BackendMigrator` to `crates/khive-storage/src/migrations.rs`. Implement
`SqliteMigrator` in `crates/khive-db`. Replace `RuntimeError::Sqlite` with
`RuntimeError::Storage(#[from] StorageError)` in `crates/khive-runtime/src/error.rs`.
Update all `RuntimeError::Sqlite` match arms in the codebase to `RuntimeError::Storage`.

Estimated scope: 4 files, ~60 LOC change.

### Phase 3 — Relocate raw SQL bypasses (G3)

Move `read_merge_entity` and similar functions from `curation.rs` into `khive-db`'s store
implementations, exposed through existing or new trait methods. Rewrite `operations.rs`
upsert paths to use `EntityStore::upsert` / `NoteStore::upsert`. Move batch graph write
operations into `GraphStore`.

This is the largest phase. It does not change observable behavior; it moves `rusqlite`-
typed code from the runtime crate into the db crate where it belongs. Full test coverage
on the affected paths must pass before merging.

Estimated scope: 3-5 files, ~200-400 LOC relocation.

### Phase 4 — `BackendHandle` and `KhiveRuntime` field change (G1)

Add `BackendHandle` type in `crates/khive-runtime/src/backend_handle.rs`. Replace
`backend: Arc<StorageBackend>` with `handle: BackendHandle` in `KhiveRuntime`. Add
`BackendHandle::from_sqlite(backend: Arc<StorageBackend>)` for the boot path. Remove
`KhiveRuntime::backend()` accessor. Update the few callers that used it.

Phases 2 and 3 must land before Phase 4. After Phase 4, `khive-runtime/Cargo.toml`
drops `rusqlite` and `khive-db` as production dependencies.

Phase 4 also updates the `core_backend: Option<Arc<StorageBackend>>` field (ADR-073) to
`core_handle: Option<BackendHandle>`, with a corresponding mechanical update to
`with_core_handle` and the boot-path wiring in `build_registry_for_multi_backend`.
The `core()` accessor semantics (ADR-073 §2) are preserved unchanged.

Estimated scope: 3 files, ~100 LOC change.

### Phase 5 — `EmbeddingModelRecord` type (G5)

Add `EmbeddingModelRecord` and `EmbeddingModelStatus` to
`crates/khive-runtime/src/embedding.rs`. Update `list_embedding_models` to return
`Vec<EmbeddingModelRecord>`. Update `khive-db`'s query method to convert
`EmbeddingModelRegistryRecord` to `EmbeddingModelRecord`. Update callers (the `kkernel
engine list` / `status` subcommands).

Estimated scope: 3 files, ~50 LOC change.

### Phase 6 — `khive-retrieval` feature gate (G6)

Change `khive-db` to a feature-gated dependency in `crates/khive-retrieval/Cargo.toml`
per §6. Move affected code behind `#[cfg(feature = "sqlite-backend")]`. Verify compilation
without the feature.

Estimated scope: 2 files, ~30 LOC change.

---

## Consequences

### Positive

- `khive-runtime` compiles without `rusqlite` after Phase 4 lands. The polystore
  boundary specified in ADR-005 and ADR-009 is restored.
- ADR-028's `[[backends]]` multi-backend boot path can be implemented: it constructs
  a `BackendHandle` per pack assignment rather than an `Arc<StorageBackend>`.
- An alternate storage backend (e.g., an in-memory backend for tests, a future
  RocksDB backend for the archive tier) can be connected without modifying
  `khive-runtime`.
- The `curation.rs` and `operations.rs` bypasses are replaced by trait-level
  operations. Correctness can be audited at the trait boundary.
- `RuntimeError::Storage` unifies error handling across all backends.
- The per-pack-backend seam is already exercised by a second consumer (see
  Downstream adoption), so the de-weld serves an existing routing need rather than
  a hypothetical one.

### Negative

- Phase 3 (relocating raw SQL bypasses) is a non-trivial refactor. It must not change
  behavior; it requires comprehensive test coverage on the affected paths before merging.
- `BackendHandle::from_sqlite` is a convenience shim that will be removed when the
  multi-backend boot path (ADR-028) lands. It adds a layer that is temporary.
- All callers of `KhiveRuntime::backend()` must be updated to use the specific
  `BackendHandle` accessor. The number of such callers is bounded (they are all within
  `khive-runtime`) but must be audited.

### Neutral

- The SQLite backend's behavior is unchanged. All current tests pass without
  modification to test code.
- The MCP wire protocol is unchanged.
- Pack verb handlers are unchanged — they depend on `KhiveRuntime`'s public API,
  not on the backend field directly.
- ADR-015 migration mechanics are unchanged inside `khive-db`; only the call site in
  the runtime boot path changes.

---

## Downstream adoption

A second runtime consumer already routes packs to distinct backends through the
mechanism this ADR formalizes. That consumer constructs two runtimes (a canonical
runtime and a secondary runtime over a separate database file) and routes a subset
of packs to the secondary backend via a `HashMap<&str, &StorageBackend>` keyed by
pack name, per the ADR-028 §7 collision policy. It is an apps-layer consumer: it
adopts reactively when the seam changes and makes no upstream request.

The adoption surface for the de-weld is small and bounded. When `StorageBackend`
(the concrete struct) is replaced by the runtime-owned `BackendHandle`, the
consumer's change is the value type of that routing map plus the two
`runtime.backend()` return sites that populate it. The `BackendHandle::from_sqlite`
shim (Negative, above) exists precisely so this construction keeps compiling across
the transition; `from_parts` is adopted later for per-slot routing.

### Sequencing constraint for shared-backend comm

A pack may be pointed at a shared networked backend by adding one entry to the
routing map (for example `("comm", shared_handle)`). This requires no new runtime
plumbing. It does, however, couple to the actor-attribution model (ADR-018): a
runtime that records episodic memory under a hardcoded actor id while recall queries
the `"local"` namespace will silently drop recalled rows. Attributed,
per-sender comm over a shared backend therefore requires the actor to flow from the
authenticated key at the Gate, not from a hardcoded runtime actor id. The
consequence is an ordering rule, not a code change in this ADR: shared-backend comm
attribution lands together with the Gate-key auth model, never before it.

---

## References

- [ADR-005](ADR-005-storage-capability-traits.md) — the polystore specification this ADR restores
- [ADR-009](ADR-009-backend-architecture.md) — backend architecture; reaffirmed, not amended
- [ADR-015](ADR-015-schema-migrations.md) — migration system; amended to reference `BackendMigrator`
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends; amended to reference `BackendHandle`
- [ADR-043](ADR-043-embedding-model-migration.md) — amended to note `EmbeddingModelRecord` type change
- [ADR-044](ADR-044-vector-store-extensions.md) — amended to note `capabilities()` default correction
- Drift audit: `docs/audits/20260625/storage-backend-drift.md`
