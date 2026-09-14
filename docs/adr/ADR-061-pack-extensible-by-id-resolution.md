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

**`handle_merge`**: [Amendment 1](#amendment-1-unsupported-generic-mutation-of-pack-private-records)
adds diagnostic-only resolver probes after existing pre-mutation operand lookups return `NotFound`.
Generic merge does not gain private-record mutation support.

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
  **Amendment 1 qualification:** `KindSpec` remains unchanged, but `merge.rs` gains the
  diagnostic-only resolver use specified below.
- `merge(into_id=<pack-uuid>)` returns `NotFound` (unchanged; no `KindSpec` extension means
  `merge` never reaches a resolver probe).
  **Superseded for diagnostics by Amendment 1:** a live private-record resolver claim now
  directs callers to the owning pack; generic merge remains unsupported for that record.
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

---

## Amendment 1: unsupported generic mutation of pack-private records

**Status**: Proposed, 2026-09-14; pending contract acceptance. **Issue**: #558.

### Decision and scope

Generic `update` and generic `merge` do not mutate records owned by pack-private tables.
Callers use the owning pack's mutation verbs, such as `knowledge.upsert_atoms`,
`knowledge.upsert_domains` and `knowledge.edit` for knowledge records. These examples are
ways to edit records, not a promise of an equivalent pack-specific merge operation. This
amendment adds neither mutation methods to `PackByIdResolver` nor a `KindSpec` variant.

During generic merge's existing pre-mutation operand checks, a `NotFound` for a resolved
UUID triggers the registered live by-ID resolver probes for that same operand. A resolver
claim produces `InvalidInput` stating that generic merge of pack-private records is
unsupported and directing the caller to the owning pack's verbs. This applies to both
`into_id` and `from_id` in the default/entity and explicit note or granular-kind branches.
`force` and `dry_run` retain that refusal. The directing error does not include private
record payloads.

If no resolver claims the UUID, return the original `NotFound` unchanged, including its
message. Other lookup and resolver errors propagate unchanged. Successful ordinary
substrate lookups do not trigger these probes. Preserve existing validation and operand
order: an earlier failure or malformed request is not replaced by a diagnostic about a
later operand. This mapping does not wrap the mutating runtime merge call or reinterpret
its errors after a possible commit.

Resolver use is diagnostic-only and performs no generic private-record mutation. Existing
`get`/`delete` support remains unchanged. Generic update's existing inferred-kind diagnostic
also remains unchanged; this amendment does not extend it to explicit-kind update routes.
Full UUID parsing and existing prefix/name resolution retain their current reachability;
no private-record short-ID lookup is added. Gate authorization remains at dispatch, and
by-ID resolver lookup remains namespace-blind.

### Compatibility and validation

The observable change is the error class and useful direction for a live pack-private UUID
reached during a merge operand lookup. Ordinary missing IDs, kind mismatches, malformed
requests, aliases, ownership checks, safety floors, ordinary merge and dry-run behavior
retain their contracts. No private data, schema or record migration is required. Generic
private-record mutation support remains a separate future decision.

Before implementation acceptance, real registered knowledge atom and domain fixtures must
prove positive generic-get reachability and the new directing refusal at either operand
position, across default/entity and note/granular routes, including aliases, force and
dry-run. Compare complete authoritative records, domain mirrors, incident edges and merge
events before and after refusals. Positive ordinary entity/note merges and dry-run previews
must still work; original missing-ID and validation errors must remain unchanged. Resolver
errors must propagate, while an unclaimed UUID retains its original error. Existing
get/delete and inferred-kind update controls remain green.

A tests-only baseline must fail at the intended new directing-error assertions. Isolated
mutations that omit either operand or the note route, ignore resolver claims or failures,
replace the direction with a bare error, or refuse all merges must be caught by the
corresponding negative and positive controls. These are acceptance requirements, not
reported native results. Implementation and merged-main validation remain pending.
