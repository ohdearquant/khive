# ADR-073: Pack Core-Backend Accessor

**Status**: Proposed\
**Date**: 2026-06-25\
**Authors**: Ocean, lambda:khive\
**Depends on**: [ADR-017](ADR-017-pack-standard.md) (Pack Standard), [ADR-028](ADR-028-pack-scoped-backends.md) (Pack-Scoped Backends), [ADR-029](ADR-029-substrate-coordinator.md) (Substrate Coordinator)\
**Extends**: ADR-028 §"Per-pack runtime instances"\
**Sequencing note**: ADR-071 (Backend-Pluggable Runtime, proposed) introduces `BackendHandle` as the future seam. See §"Relationship to ADR-071" for the forward-compatibility constraint.

---

## Context

### Pack-scoped backends (ADR-028)

ADR-028 specifies that each pack receives exactly one `KhiveRuntime` instance, constructed
over its assigned backend. In a multi-backend deployment, the boot path in
`crates/khive-mcp/src/serve.rs` (`build_registry_for_multi_backend`, lines 110-262)
opens each declared backend, then constructs one `KhiveRuntime` per pack via
`KhiveRuntime::from_backend(backend, rt_config)`.

### Single-backend handle (the gap)

`KhiveRuntime` holds exactly one backend handle:

```rust
// crates/khive-runtime/src/runtime.rs:32
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    // ...
}
```

Both note and entity creation (`create_note` → `self.notes(token)` → `self.backend`,
`operations.rs:1887`) and raw SQL access (`self.sql()` → `self.backend.sql()`,
`runtime.rs:223-225`) route through that single handle.

A pack assigned to a secondary backend has its `KhiveRuntime::backend` bound to that
secondary file. Every note and entity it creates lands there.

### The consequence

A note in the main backend participates fully in the shared graph: it can be retrieved
by `memory.recall`, `search`, and `get`; it can be the target of `annotates` edges from
entities in `kg`, `gtd`, and `memory`; it appears in cross-pack traversals.

A note written to a secondary backend is invisible to those operations. Edges cannot
span SQLite files. Any note that must be linkable — via `annotates` or any other
relation — must reside in the main backend regardless of where its pack's auxiliary
tables live.

Today, a pack assigned to a secondary backend is forced to write everything there,
including notes that logically belong in the shared graph. There is no mechanism for
such a pack to write one class of records to main and another class to its own backend.

### Motivating pattern

A pack that manages high-volume append-only records in a dedicated database illustrates
the gap. Such a pack would want its summary note (the entity that receives `annotates`
edges from concepts, tasks, and people) to reside in the main backend so that
`memory.recall` and cross-pack graph traversals can find it. Meanwhile, the bulk auxiliary
rows it indexes (log entries, timing data, large append-only tables) are appropriately
isolated in a dedicated file with its own VACUUM schedule. Under the current design this
split is not expressible without duplicating pack logic across two separate pack instances.

---

## Decision

### 1. Add `core_backend` field to `KhiveRuntime`

`KhiveRuntime` gains one new field:

```rust
// crates/khive-runtime/src/runtime.rs
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    /// When `Some`, holds the main backend so that `core()` can return a
    /// main-bound runtime handle without constructing a new connection.
    /// `None` when this runtime is already bound to the main backend.
    core_backend: Option<Arc<StorageBackend>>,
    config: RuntimeConfig,
    embedder_registry: Arc<RwLock<EmbedderRegistry>>,
    default_embedder_name: Arc<str>,
    edge_rules: Arc<RwLock<Vec<EdgeEndpointRule>>>,
    valid_entity_kinds: Arc<RwLock<Vec<String>>>,
    valid_note_kinds: Arc<RwLock<Vec<String>>>,
}
```

The invariant is: `core_backend` is `None` when `self.config.backend_id == BackendId::main()`,
and `Some(main_arc)` when `self` is bound to a secondary backend. A runtime produced by
any constructor that does not supply a core backend always has `core_backend = None`.

### 2. `core()` accessor

`KhiveRuntime` gains the following public method:

```rust
/// Return a runtime handle bound to the main (shared-graph) backend.
///
/// When `self` is already the main runtime (`core_backend` is `None`),
/// this returns a clone of `self` — no new backend reference is acquired.
///
/// When `self` is a secondary-backend runtime (`core_backend` is `Some`),
/// this returns a new `KhiveRuntime` struct backed by the main
/// `Arc<StorageBackend>` and sharing all registry state (`embedder_registry`,
/// `edge_rules`, `valid_entity_kinds`, `valid_note_kinds`) with `self`.
/// No database I/O occurs; no embedding models are reloaded.
///
/// Use `core()` for notes and entities that must reside in the shared graph
/// so that `memory.recall`, cross-pack search, and `annotates` edges work.
/// Use `self` (or `self.sql()`) for pack-auxiliary bulk tables.
pub fn core(&self) -> KhiveRuntime {
    match &self.core_backend {
        None => self.clone(),
        Some(main_arc) => {
            let mut core_config = self.config.clone();
            core_config.backend_id = BackendId::main();
            KhiveRuntime {
                backend: main_arc.clone(),
                core_backend: None,
                config: core_config,
                embedder_registry: self.embedder_registry.clone(),
                default_embedder_name: self.default_embedder_name.clone(),
                edge_rules: self.edge_rules.clone(),
                valid_entity_kinds: self.valid_entity_kinds.clone(),
                valid_note_kinds: self.valid_note_kinds.clone(),
            }
        }
    }
}
```

The returned `KhiveRuntime` shares the same `Arc`-wrapped registry state as `self`. The
cost is bounded to cloning `RuntimeConfig` (a small struct containing a few short `Vec`
fields) and incrementing several `Arc` reference counts. No new database connection is
opened, no embedding model is loaded, and no I/O occurs.

### 3. Constructor changes

All existing constructors (`new`, `new_readonly`, `from_backend`, `memory`) set
`core_backend: None`. No constructor signature changes.

A builder-style wiring method is added for the boot path:

```rust
/// Wire this runtime as a secondary-backend runtime pointing at `core`.
///
/// After this call, `self.core()` returns a handle to `core` rather than
/// cloning `self`. The caller (the boot path, not pack code) is responsible
/// for passing the correct main backend.
///
/// Panics in debug builds if `self.config.backend_id == BackendId::main()`,
/// because the main runtime does not need a core pointer.
pub fn with_core_backend(mut self, core: Arc<StorageBackend>) -> Self {
    debug_assert_ne!(
        self.config.backend_id.as_str(),
        BackendId::MAIN,
        "with_core_backend must not be called on the main runtime"
    );
    self.core_backend = Some(core);
    self
}
```

### 4. Boot path wiring in `build_registry_for_multi_backend`

After constructing each per-pack runtime, the boot path calls `with_core_backend` for
any pack whose assigned backend is not `main`:

```rust
// crates/khive-mcp/src/serve.rs — inside build_registry_for_multi_backend
for pack_name in pack_names {
    let backend_name = khive_cfg
        .packs
        .get(pack_name.as_str())
        .map(|pc| pc.backend.as_str())
        .unwrap_or(BackendId::MAIN);
    let backend = backends
        .get(backend_name)
        .cloned()
        .unwrap_or_else(|| main_backend.clone());
    let mut rt_config = base_config.clone();
    rt_config.backend_id = BackendId::new(backend_name);
    let mut rt = KhiveRuntime::from_backend(backend, rt_config);
    if backend_name != BackendId::MAIN {
        rt = rt.with_core_backend(main_backend.clone());
    }
    per_pack_runtimes_local.insert(pack_name.clone(), rt);
}
```

The `main_backend` `Arc` is already available in this function (lines 141-149 of the
current code). No new backend opens, and no per-pack runtime that is already on `main`
is touched.

### 5. Semantics and contract

**Core is for the shared graph.** `core()` is intended for notes and entities that must
participate in the shared graph substrate: notes that will receive or originate
`annotates` edges, entities that must appear in `memory.recall` or cross-pack search,
or any record that a different pack's verb handler may need to traverse or link.

**`self` is for pack-auxiliary bulk data.** A pack handler that must write pack-specific
rows to its own backend uses `self` (or `self.sql()`) directly. This is the case for any
auxiliary table declared in the pack's `schema_plan` and applied to the secondary backend.

**Cross-backend edges remain illegal.** The `link` operation requires both the source
and the target to reside in the same backend's `graph_edges` table. A pack that writes
a note via `core()` and a bulk row via `self` cannot create an edge between those two
records. The note should carry any linking information as properties or be linked from
a main-side entity using a main-side edge.

**No cross-backend atomicity.** A handler that writes a note via `core()` (the main
backend) and auxiliary rows via `self.sql()` (the pack's secondary backend) performs two
independent transactions across two SQLite files. If the first commit succeeds and the
second fails — or if the process crashes between them — one side is committed and the
other is not, leaving an orphaned record. Pack handlers must design writes to be
idempotent or provide compensating operations; they must not assume a single transaction
spans both the main and the secondary backend.

**The `Pack::dispatch` signature is unchanged.** Pack handlers receive a `&VerbRegistry`
and a `&KhiveRuntime` (the pack's assigned runtime). The pack's verb handler code calls
`self_runtime.core()` when it needs to write a main-side record. No dispatch-layer
change is needed, and existing packs that do not use `core()` are unaffected.

### 6. Relationship to ADR-071

ADR-071 (proposed, not yet accepted) introduces `BackendHandle` — a struct holding
`Arc<dyn Trait>` handles — to replace `Arc<StorageBackend>` in `KhiveRuntime`. If
ADR-071 Phase 4 lands before this ADR is implemented, the field type changes:

```rust
// ADR-071 world:
core_handle: Option<BackendHandle>,   // replaces core_backend: Option<Arc<StorageBackend>>
```

`BackendHandle::from_sqlite(Arc<StorageBackend>)` (specified in ADR-071 §1) provides
the upgrade path: the boot path passes `BackendHandle::from_sqlite(main_backend)` where
this ADR passes `main_backend` directly.

ADR-073 should not be folded into ADR-071. ADR-071 closes the polystore boundary
violation — a multi-phase structural refactor touching migrations, error types, raw SQL
bypasses, and the runtime field type. ADR-073 is a narrow capability addition: one new
field, one new method, and one boot-path call site. Merging them would couple a large
in-flight refactor to a small usability fix and delay both. They are compatible in
sequence; the implementation of ADR-073 need only adapt the field type when ADR-071
Phase 4 lands.

---

## Rationale

### Why a `core()` accessor over a dual-handle dispatch signature

The most direct alternative is to change the `PackRuntime::dispatch` signature to pass
two runtimes: `self_rt: &KhiveRuntime, core_rt: &KhiveRuntime`. This exposes the
split to every pack implementer and forces every existing pack to accept a second
parameter, even if it never uses it. `PackRuntime` is a trait; adding a required
parameter is a breaking change across all 7 production packs and every downstream
consumer. The accessor keeps the change entirely within `KhiveRuntime` and pack handler
code that opts in. Existing packs compile and function without modification.

### Why `Option<Arc<StorageBackend>>` rather than always storing a core reference

Storing the main `Arc<StorageBackend>` only on secondary-backend runtimes avoids a
reference cycle: the main runtime does not hold a self-reference. The `None` sentinel
signals to `core()` to return `self.clone()`, which is correct and avoids the allocation
of a redundant handle struct. Secondary-backend runtimes hold a reference to the main
backend, which will keep it live as long as any secondary runtime is live — this is
acceptable because the main backend must outlive all packs in a well-ordered shutdown.

### Why `with_core_backend` is a builder method rather than a constructor parameter

`from_backend` is called for all runtimes in the boot path, including the main runtime,
which must not receive a core pointer. A separate wiring step avoids a conditional
parameter (e.g., `Option<Arc<StorageBackend>>` in `from_backend`) that would be `None`
for the main runtime and `Some` for secondary runtimes. The builder method is called only
where it is needed; the condition is explicit in the boot path rather than implicit in
a constructor.

### Why `core()` returns `KhiveRuntime` by value rather than `&KhiveRuntime`

Returning `&KhiveRuntime` would require storing the core runtime as a field of type
`Option<Box<KhiveRuntime>>`, which introduces a recursion in the struct's layout. It
would also force callers to handle lifetime annotations in async contexts where
`KhiveRuntime` must often be moved or cloned for `'static` bounds. Returning by value
uses the existing `Clone` implementation: all fields are `Arc`-wrapped except
`RuntimeConfig`, whose heap content is small (a few short `Vec<String>` fields). The
copy cost is bounded and does not involve I/O or model loading.

---

## Alternatives Considered

### A. Accessor (chosen)

Pack handlers call `self_runtime.core()` to get a main-bound handle for shared-graph
writes. All other writes use `self_runtime`. No dispatch signature change; existing
packs unaffected.

Chosen. Minimum blast radius, backward-compatible, implementable in two phases
(field addition, then boot-path wiring).

### B. Dual-handle dispatch signature

`PackRuntime::dispatch` gains a second parameter: `core_rt: &KhiveRuntime`. Packs that
need to write to main use `core_rt`; others use their existing runtime.

Rejected. This is a breaking change to the `PackRuntime` trait — every existing pack
must be updated to accept and forward the second parameter, including packs that never
use it. The surface area of the change grows with the number of packs, and the
maintenance cost of the extra parameter never decreases.

### C. Status-quo single-backend plus `memory.remember` bridge

Packs that need a main-side note call `memory.remember` via the verb dispatch layer
rather than writing a note directly. The `memory` pack is always assigned to main.

Rejected. This conflates two concerns: creating a semantically distinct note kind with
creating a memory record. It routes around the type system (a pack-specific note
masquerading as a memory), prevents the pack from using its own note kind, and couples the pack to
the `memory` pack's vocabulary. It also requires a round-trip through the verb registry
when a direct storage write would suffice. Finally, it does not generalize: other packs
(such as a future `comm` pack assigned to a dedicated file) face the same note-ownership
problem and cannot all be bridged through `memory.remember`.

### D. Promote secondary-backend notes to main via a post-write copy

After writing a note to the secondary backend, the pack handler duplicates the record
to the main backend so that recall and search can find it. The secondary copy is the
canonical record; the main copy is a projection.

Rejected. This creates two sources of truth for the same record. Soft delete, updates,
and edge consistency become undefined: which copy is the target of an `annotates` edge?
Which version is authoritative on recall? The data-vs-view principle (see `CLAUDE.md`)
prohibits this pattern explicitly: representing the same logical record in two backends
is not a view-layer decision, it is a data-layer duplication with no defined merge
semantics.

---

## Risks

- **Boot-path wiring omission**: a secondary-backend pack whose `core_backend` is not
  wired at boot will silently call `core()` and receive `self.clone()` — a same-backend
  handle, not a main handle. The debug assert in `with_core_backend` catches the inverse
  (wiring a main runtime), but there is no assert for the omission case. A future
  integration test that verifies `rt.core().backend_id() == BackendId::main()` for all
  secondary runtimes closes this gap.

- **ADR-071 sequencing**: if ADR-071 Phase 4 lands in a branch concurrently with the
  implementation of this ADR, the `core_backend` field type will conflict. The
  implementer must rebase or merge after ADR-071 Phase 4 and update the field type
  from `Option<Arc<StorageBackend>>` to `Option<BackendHandle>`. This is a well-defined
  mechanical update; the semantic contract is unchanged.

- **Accidental cross-backend edge creation**: a pack handler that obtains a core handle
  and a self handle and then tries to call `link` between a core-side record and a
  self-side record will receive a `NotFound` or `StorageError` from the graph store,
  because the edge table in main does not contain the secondary-side UUID. This fails
  at runtime, not at compile time. Documenting the constraint in the `core()` docstring
  (done above) and in pack development guidance is the mitigation for v1.

---

## Consequences

### Positive

- Packs assigned to secondary backends can write notes and entities to the shared graph
  without moving to main or splitting into two pack instances.
- The `Pack::dispatch` trait signature is unchanged; all existing packs compile without
  modification.
- The `KhiveRuntime` public API surface grows by one method and one builder. The struct
  grows by one `Option<Arc<StorageBackend>>` field.
- No new database connections, no embedding model reloads, and no I/O are introduced by
  `core()`.
- The boot path change is localized to `build_registry_for_multi_backend`; single-backend
  deployments see no behavioral change (all packs get `core_backend = None`).

### Negative

- Every `KhiveRuntime` instance carries one additional `Option<Arc<StorageBackend>>`
  field (8 bytes for the discriminant, 8 bytes for the pointer). This is negligible.
- Pack authors must understand the core-vs-self distinction when implementing handlers
  for packs assigned to secondary backends. Incorrect use (writing a linkable note to
  `self` instead of `core()`) fails silently at the storage level.

### Neutral

- The MCP wire protocol is unchanged.
- Single-backend deployments (the common case) are unaffected: all runtimes have
  `core_backend = None`, and `core()` returns `self.clone()`.
- ADR-028's multi-backend topology is not altered; this ADR adds a capability within
  that topology.

---

## References

- [ADR-017](ADR-017-pack-standard.md) — `PackRuntime` trait; `dispatch` signature
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends; per-pack runtime instances
- [ADR-029](ADR-029-substrate-coordinator.md) — cross-backend coordination; SubstrateCoordinator
- ADR-071 (draft, not yet authored) — proposed polystore boundary restoration; forward-compatibility constraint documented in §"Relationship to ADR-071"
- `crates/khive-runtime/src/runtime.rs` — `KhiveRuntime` struct definition (line 31)
- `crates/khive-mcp/src/serve.rs` — `build_registry_for_multi_backend` (line 110)
- `crates/khive-runtime/src/operations.rs` — `create_note` path (line 1887)
