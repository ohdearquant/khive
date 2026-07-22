# ADR-073: Pack Core-Backend Accessor

**Status**: Accepted\
**Date**: 2026-06-25\
**Authors**: khive maintainers\
**Depends on**: [ADR-017](./ADR-017-pack-standard.md),
[ADR-028](./ADR-028-pack-scoped-backends.md),
[ADR-029](./ADR-029-substrate-coordinator.md)\
**Extends**: ADR-028, per-pack runtime instances

---

## Context

ADR-028 assigns each loaded pack one `KhiveRuntime` bound to that pack's configured backend. A pack
may keep auxiliary rows in a secondary database while also needing to create a note or entity in
the main graph. Edges cannot span database files, so linkable graph records must be written through
the main backend.

Changing `PackRuntime::dispatch` to pass two runtimes would affect all loaded packs and every
downstream implementation of the trait. The runtime instead exposes an opt-in accessor for the
small number of handlers that need the main graph.

## Decision

### 1. Store an optional main-backend reference

The shipped runtime representation is:

```rust
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    core_backend: Option<Arc<StorageBackend>>,
    // shared runtime configuration and registries
}
```

`core_backend` is `None` when the runtime is already bound to the main backend. A runtime bound to a
secondary backend receives `Some(main_backend)` during boot.

All ordinary constructors leave the field unset. Only the multi-backend boot path wires the main
reference, using:

```rust
pub fn with_core_backend(mut self, core: Arc<StorageBackend>) -> Self {
    debug_assert_ne!(self.config.backend_id.as_str(), BackendId::MAIN);
    self.core_backend = Some(core);
    self
}
```

### 2. Expose `core()`

```rust
pub fn core(&self) -> KhiveRuntime {
    match &self.core_backend {
        None => self.clone(),
        Some(main_backend) => self.clone_bound_to(main_backend.clone(), BackendId::main()),
    }
}
```

The actual implementation constructs the returned runtime while cloning its shared registries and
configuration. It opens no database connection, loads no embedding model, and performs no I/O.

For a main-bound runtime, `core()` returns a clone of `self`. For a secondary-bound runtime, it
returns a runtime backed by the main database with `core_backend = None`, so repeated `core()` calls
remain idempotent.

### 3. Preserve shared runtime state

The returned runtime shares the same `Arc`-wrapped embedder registry, endpoint rules, and valid-kind
registries as the originating runtime. Its backend identifier is changed to `main`; unrelated
configuration values are preserved.

This makes `core()` a storage-target change, not a new logical runtime configuration.

### 4. Wire every secondary runtime at boot

The multi-backend boot path holds the main backend handle before it constructs per-pack runtimes.
For each pack assigned to a secondary backend, it calls `with_core_backend(main_backend.clone())`
before registering handlers. A runtime assigned to `main` is left unchanged.

Boot validation must verify that every secondary runtime reports `main` from
`rt.core().backend_id()`. Missing wiring is a startup error, not a silent fallback to the secondary
database.

### 5. Define handler semantics

- `self` is the pack's assigned backend. Handlers use it for auxiliary tables and records that do
  not participate in the shared graph.
- `self.core()` is the main graph backend. Handlers use it for entities, notes, and edges that must
  be visible through shared graph queries.
- Both endpoints of an edge must reside in the same backend.
- Writes through `self` and `self.core()` are separate transactions. No cross-backend atomicity is
  implied.
- A handler that writes to both backends must be idempotent or define a compensating operation.

The `PackRuntime::dispatch` signature remains unchanged. Packs that do not call `core()` require no
source change.

### 6. Compatibility with ADR-071

ADR-071 accepts `BackendHandle` as the future backend-neutral runtime representation. The shipped
code still uses `Arc<StorageBackend>`, so this ADR documents both the current public symbol and the
required compatible endpoint.

When the ADR-071 handle transition is implemented, these fields and methods change together:

```rust
core_handle: Option<BackendHandle>

pub fn with_core_handle(self, core: BackendHandle) -> Self
```

The semantics of `core()` do not change. Both the assigned and core backends use `BackendHandle`,
and the main-bound `None` case still returns `self.clone()`. No intermediate representation is part
of the public contract.

## Invariants

1. A secondary runtime is registered only after its core reference is wired.
2. `core()` always returns a runtime whose backend identifier is `main`.
3. Calling `core()` on a main runtime is idempotent and performs no I/O.
4. Runtime registries are shared, not reconstructed.
5. Cross-backend edges and transactions are unsupported.
6. Pack handlers that never call `core()` are unaffected.

## Rejected alternatives

### Pass two runtimes to every dispatch

Rejected because it changes the trait for all loaded packs even though most handlers need only one
backend.

### Copy secondary records into the main database after writes

Rejected because it creates two sources of truth and leaves update, deletion, and edge ownership
undefined.

### Let handlers open the main database directly

Rejected because it bypasses runtime configuration, connection ownership, capability checks, and
shared registries.

## Consequences

### Positive

- Secondary-backend packs can create linkable records in the main graph.
- Existing dispatch implementations remain source-compatible.
- The accessor performs no additional connection or model initialization.
- ADR-071 has a defined mechanical representation change with unchanged semantics.

### Tradeoffs

- Handlers that use both backends must account for partial failure.
- The distinction between assigned and core storage is a pack-author responsibility.
- The shipped concrete field remains until the ADR-071 handle transition is implemented.

## Testing requirements

- Main-bound `core()` returns the main backend and shares registries.
- Every secondary runtime is wired to the main backend before registration.
- A missing secondary core reference fails boot validation.
- `core()` performs no database open or embedder load.
- A record written through `core()` is visible to main-graph search.
- Cross-backend edge attempts fail without partial edge creation.
- The ADR-071 representation change preserves all tests above.

## References

- [ADR-017](./ADR-017-pack-standard.md): pack dispatch contract
- [ADR-028](./ADR-028-pack-scoped-backends.md): per-pack backend assignment
- [ADR-029](./ADR-029-substrate-coordinator.md): cross-backend coordination
- [ADR-071](./ADR-071-backend-pluggable-runtime.md): backend-neutral runtime handle
