# ADR-061: Pack-Extensible by-ID Resolution

**Status**: Accepted
**Date**: 2026-06-16
**Amends**: ADR-017 (Pack Standard) — adds `PackByIdResolver` sub-trait
**Completes**: ADR-007 Rule 2 — globally-unique UUID contract extended to pack-private tables
**Issue**: #158

---

## Context

ADR-007 Rule 2 establishes that by-ID operations resolve a record solely by UUID, without
namespace filtering. The `resolve_by_id` function in `operations.rs` covers entity and note
substrates only. The knowledge pack stores records in private SQL tables (`knowledge_atoms`,
`knowledge_domains`) that are invisible to this resolver: `get(id=<atom-uuid>)` and
`delete(id=<atom-uuid>)` return `NotFound` even for valid UUIDs.

The `gtd` and `memory` packs are unaffected — they write into the shared `notes` table and are
found by the existing resolver. Brain, comm, and schedule pack schemas are unverified;
follow-up items will assess each.

---

## Decision

Introduce a `PackByIdResolver` sub-trait in `crates/khive-runtime/src/pack.rs`. Packs that own
private SQL tables implement this sub-trait and register it at build time. The `kg` pack's
`handle_get` and `handle_delete` probe registered resolvers when the standard substrates return
nothing. The `Resolved` enum gains a `PackRecord` variant.

---

## Mechanism

### 1. `PackByIdResolver` sub-trait

```rust
#[async_trait]
pub trait PackByIdResolver: Send + Sync {
    async fn resolve_by_id(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<Resolved>, RuntimeError>;

    async fn resolve_by_id_including_deleted(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<Resolved>, RuntimeError> {
        self.resolve_by_id(id).await
    }

    async fn delete_by_id(
        &self,
        id: uuid::Uuid,
        hard: bool,
    ) -> Result<serde_json::Value, RuntimeError>;
}
```

Both `resolve_by_id` and `delete_by_id` are required. Implementing one without the other is a
compile-time error. `resolve_by_id` must not filter by namespace (ADR-007: by-ID resolution is
namespace-blind). `delete_by_id` defaults to soft-delete when the pack's table has a
`deleted_at` column, honoring `hard=true` for permanent removal.

### 2. `Resolved::PackRecord` variant

Add to `operations.rs`:

```rust
pub enum Resolved {
    Entity(Entity),
    Note(Note),
    Event(Event),
    PackRecord { pack: String, kind: String, data: serde_json::Value },
}
```

This is a breaking enum change. Updated match sites:

| Site                                         | Required arm                                                                                 |
| -------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `resolved_pair` (`operations.rs`)            | `Resolved::PackRecord { .. } => None`                                                        |
| edge-endpoint tuple match (`operations.rs`)  | `(Resolved::PackRecord{..}, _)` and `(_, Resolved::PackRecord{..})` returning `InvalidInput` |
| `validate_context_entity` (`khive-pack-gtd`) | `Some(Resolved::PackRecord{..}) => Err(InvalidInput(...))`                                   |

`KindSpec` is NOT extended. Pack identity flows through `Resolved::PackRecord`, not through a
`KindSpec` variant.

### 3. `VerbRegistry` resolver collection

Add a separate resolver collection to `VerbRegistry`:

```rust
resolvers: Arc<Vec<(String, Box<dyn PackByIdResolver>)>>
```

`VerbRegistryBuilder` gains a `register_resolver` method. The `VerbRegistry` exposes a
`resolvers()` accessor returning `&[(String, Box<dyn PackByIdResolver>)]`.

`PackFactory` gains an optional `create_resolver` method (defaults to `None`) so existing pack
factories compile unchanged. `PackRegistry::register_packs` calls `create_resolver` for each
factory and registers the result if `Some`.

### 4. kg pack handler changes

**`handle_get`**: after the event probe and before the proposal fallback, iterate registered
resolvers. If any resolver returns `Some(Resolved::PackRecord { kind, data, .. })`, return the
record directly via `flatten_get_result`.

**`handle_delete`**: catch `Err(RuntimeError::NotFound(_))` from `infer_kind_from_uuid` (or
`infer_kind_from_uuid_including_deleted` on `hard=true`). Before re-raising, iterate resolvers
and call `resolve_by_id` (or `resolve_by_id_including_deleted`). If any resolver claims the
UUID, call `resolver.delete_by_id(id, hard)`. If none match, re-raise `NotFound`.

**`handle_update`**: catch `Err(RuntimeError::NotFound(_))` from `infer_kind_from_uuid`. Before
re-raising, probe resolvers. If any claims the UUID, return `InvalidInput` explaining that
pack-private record update is not yet supported via the generic `update` verb.

### 5. Knowledge pack implementation

`KnowledgePack` implements `PackByIdResolver`:

**`resolve_by_id`**: query `knowledge_domains` first (`deleted_at IS NULL`); if found, return
`PackRecord { kind: "domain", ... }`. If not found, query `knowledge_atoms` (`deleted_at IS
NULL`); if found, return `PackRecord { kind: "atom", ... }`. Domains must be queried first
because the domain mirror in `knowledge_atoms` shares the domain's UUID.

**`resolve_by_id_including_deleted`**: same queries without the `deleted_at IS NULL` guard.

**`delete_by_id`**:

- `kind == "domain"`: soft-delete `knowledge_domains` AND the mirror row in `knowledge_atoms`
  (both by UUID). When `hard=true`: hard-delete both. The mirror must be tombstoned to close
  the FTS leak — `knowledge.search` filters `deleted_at IS NULL`.
- `kind == "atom"`: soft-delete `knowledge_atoms` by UUID. When `hard=true`: hard-delete.
- Response: `{ "deleted": true, "id": "<uuid>", "kind": "<domain|atom>", "hard": <bool> }`.

### 6. Authorization unaffected

Pack resolver hooks are UUID lookups only. No namespace parameter, no actor check. The Gate
fires at verb dispatch before `handle_get` or `handle_delete` runs (ADR-018). No inline
namespace or actor equality checks may appear in resolver implementations.

---

## ADR-017 Amendment

Add to the `PackRuntime` section of ADR-017:

> **By-ID resolution sub-trait.** Packs that own private SQL tables and issue UUIDs through
> their verbs must implement `PackByIdResolver` and register via
> `VerbRegistryBuilder::register_resolver`. The sub-trait bundles `resolve_by_id` and
> `delete_by_id` as a unit — partial implementation is a compile-time error. Packs whose
> records live in the shared entity/note substrate (gtd, memory) do not implement this
> sub-trait. `resolve_by_id` must not filter by namespace. `delete_by_id` must default to
> soft-delete if the pack's table has a `deleted_at` column, honoring `hard=true`.

| Pack      | Private tables                         | Implements `PackByIdResolver` |
| --------- | -------------------------------------- | ----------------------------- |
| kg        | none                                   | no                            |
| gtd       | none                                   | no                            |
| memory    | none                                   | no                            |
| knowledge | `knowledge_atoms`, `knowledge_domains` | yes (this ADR)                |
| brain     | unverified                             | deferred                      |
| comm      | unverified                             | deferred                      |
| schedule  | unverified                             | deferred                      |

---

## Consequences

### Positive

- ADR-007's globally-unique UUID contract is fully satisfied for pack-private knowledge records.
- `delete(id)` no longer silently returns `NotFound` for valid knowledge UUIDs.
- Domain delete tombstones the mirror atom in `knowledge_atoms`, closing the FTS leak.
- The sub-trait provides compile-time "both-or-neither" atomicity.
- Authorization seam (ADR-018) is unaffected.

### Negative

- `Resolved::PackRecord` is a breaking enum change requiring three exhaustive match updates.
- `KindSpec` is NOT extended — no cascade across `list.rs`, `create.rs`, `search.rs`,
  `merge.rs`, `common.rs`.
- `merge(into_id=<pack-uuid>)` returns `NotFound` (unchanged; no `KindSpec` extension means
  `merge` never reaches a resolver probe).
- `update(id=<pack-uuid>)` returns `InvalidInput` directing callers to pack-specific verbs.
  Deferred to a future ADR.

---

## Alternatives Considered

**Add `knowledge.delete_domains` verb.** Avoids the `Resolved` enum change but violates ADR-007:
callers must track which pack issued a UUID to choose the right delete verb. Rejected.

**Migrate to shared tables.** Domain/atom records do not map onto entity/note semantics. Schema
migration plus data migration for no correctness gain. Rejected.

**Special-case knowledge in `handle_get`.** Does not generalize. Coupling accumulates in the
wrong direction. Rejected.

---

## References

- ADR-007: Namespace — normative home for globally-unique UUID and by-ID namespace-blind contract
- ADR-017: Pack Standard — this ADR amends the PackRuntime trait surface
- ADR-018: Authorization Gate — gate fires at verb dispatch, not in resolver hooks
- Issue #158: `get`/`delete` cannot resolve knowledge pack records
