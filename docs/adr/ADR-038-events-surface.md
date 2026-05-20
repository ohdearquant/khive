# ADR-038: Events Surface — Runtime Operations and MCP Verbs over EventStore

**Status**: accepted
**Date**: 2026-05-19
**Authors**: khive maintainers

## Context

ADR-004 (§"Three Substrate Observables") declares `Event` as the third substrate alongside `Note`
and `Entity`, with the explicit semantic "what happened" — the history observable. The `EventStore`
trait exists in `khive-storage` (line 83 in `crates/khive-storage/src/event.rs`) and is backed by
`SqlEventStore` in `khive-db`. ADR-033 defined the `AuditEvent` type for gate-layer observability,
deferring storage-backed emission and query surface to v0.3 (ADR-033 Implementation Status table,
rows "Query surface" and "EventStore wiring").

The current state: events are appended at the storage level but **no pack handler exposes them
through the verb surface**. The `list` verb's `kind` discriminator in `khive-pack-kg`
(`crates/khive-pack-kg/src/handlers.rs`, `resolve_kind_spec`) handles `"entity"`, `"note"`, and
`"edge"` — not `"event"`. As a result:

- A downstream metering daemon (per ADR-034 §"Future Work") that wants to poll `EventStore` for
  `Obligation::Meter` records has no MCP path — it must reach into the DB directly, bypassing the
  namespace isolation contract (CLAUDE.md §"Namespace isolation").
- An observability dashboard that wants to answer "how many deny events in the last hour?" has no
  verb to call.
- Every consumer of audit data either duplicates access logic or couples to SQLite internals —
  defeating the point of the `EventStore` abstraction.

ADR-023 (verb-consolidated surface) explicitly notes that events-as-observables would "just become
new `kind=` values" (§"Versioning tools"), preserving the existing verb-consolidated surface by
adding `event` as a `kind=` value rather than introducing new event-specific verbs. This ADR
activates that anticipation.

This ADR closes GitHub issue [#5](https://github.com/ohdearquant/khive/issues/5).

## Decision

### D1: Verb shape — `list(kind="event")` and `get(id=<event_uuid>)`

Events are exposed through the existing `list` verb via `kind="event"` and through `get` via
UUID resolution (no `kind` parameter — `get` auto-detects the substrate from the UUID),
consistent with ADR-023's verb-consolidation principle. No new top-level verbs are introduced.

**`list(kind="event", ...)`** is the primary access path. It maps to `EventStore::query_events`
with an `EventFilter` constructed from the caller's parameters (see D2) and a `PageRequest`
derived from `limit` / `offset`.

**`get(id=<uuid>)`** already resolves across substrates (entities, notes, edges). This ADR extends
the resolution to also check `EventStore::get_event` when the UUID is not found in the entity or
note tables. The `Resolved::Event` variant already exists in
`crates/khive-runtime/src/operations.rs` (line 32) — it is not yet reachable from the `get`
handler; this ADR completes that wiring.

**`create(kind="event", ...)`**, **`update`**, and **`delete`** over events are **prohibited**.
Events are immutable by construction (ADR-004 §"Why Event immutable?"). The pack handler must
return `invalid_params("events are immutable — create/update/delete are not permitted")` if
called with an event target. This makes the constraint explicit rather than silently absent.

**`search(kind="event", query=...)`** is **out of scope for v0.1**. Events have a JSON `data`
field but no natural full-text content column. FTS over JSON-as-text is awkward and the useful
queries are filtered listing, not semantic proximity. The `EventFilter` predicate set (D2) covers
all practical access patterns. FTS is tracked as a future ADR.

**`query(...)` (GQL/SPARQL)** is **excluded**. Events are tabular; they have no edges and no
graph structure. GQL pattern matching over events would be gratuitous.

### D2: Filter shape for `list(kind="event", ...)`

The `ListParams` struct in `crates/khive-pack-kg/src/handlers.rs` (line 192) gains an event-specific
branch. These parameters map directly to the existing `EventFilter` struct
(`crates/khive-storage/src/event.rs`, line 71):

| Wire parameter | Type | Maps to `EventFilter` field | Notes |
|---|---|---|---|
| `verb` | `string` | `verbs: Vec<String>` | Single verb string; stored as a one-element vec |
| `verbs` | `[string]` | `verbs: Vec<String>` | Multi-value form; `verb` and `verbs` are merged |
| `outcome` | `string` | not in `EventFilter` — post-filter | `"success"` \| `"denied"` \| `"error"` |
| `actor` | `string` | `actors: Vec<String>` | Exact match; free-form actor string |
| `substrate` | `string` | `substrates: Vec<SubstrateKind>` | `"note"` \| `"entity"` \| `"event"` |
| `since` | `int` (microseconds UTC) | `after: Option<i64>` | Exclusive lower bound on `created_at` |
| `until` | `int` (microseconds UTC) | `before: Option<i64>` | Exclusive upper bound on `created_at` |
| `limit` | `u32` | `PageRequest::limit` | Default `100`, max `1000` |
| `offset` | `u32` | `PageRequest::offset` | Default `0` |

**Outcome filter note.** `EventOutcome` is not in `EventFilter` — the existing SQL builder
(`build_event_filter_sql` in `crates/khive-db/src/stores/event.rs`, line 213) does not filter by
outcome. The handler applies `outcome` as a post-query scan: it iterates raw event pages
internally (fetching `limit`-sized batches from the store), applies the outcome predicate to each
row, skips the first `offset` matching rows, collects `limit` matching rows, and stops when
either `limit` matches are collected or the store returns fewer rows than requested (EOF). The
caller's `offset` and `limit` apply to the **filtered** result set, not to raw events, so
standard pagination semantics are preserved. A bounded-scan ceiling of `(offset + limit) * 20`
total raw rows prevents unbounded iteration; if the ceiling is reached before `limit` matches
are collected (after skipping `offset`), the handler returns a short page. At expected event volumes (hundreds to low thousands per namespace in a
personal deployment) this scan is acceptable. A future migration can add `outcome` to
`EventFilter` and the SQL query when high-volume deployments need index-backed outcome filtering.

**Namespace isolation.** The `list` handler always passes the caller's namespace as the default
when building the `EventFilter`. Callers cannot enumerate events from foreign namespaces — the
`build_event_filter_sql` function (line 221) forces `namespace = ?` to the caller's namespace
when `filter.namespaces` is empty. Passing explicit `namespaces` in the filter is not exposed on
the wire; the handler ignores any such field.

**Default ordering.** `query_events` orders by `created_at DESC` (line 444 in `SqlEventStore`),
returning the most recent events first. This default is not overridable in v0.1.

### D3: Aggregation — deferred to v0.2

`count_events` exists on `EventStore` (`crates/khive-storage/src/event.rs`, line 93) and on
`SqlEventStore`. A `count(kind="event", ...)` verb is **not** added in this ADR. Downstream
consumers that need aggregate counts for dashboards can issue `list(kind="event", limit=1000)`
and aggregate client-side; event volumes at OSS deployment scale make this feasible.

A future verb `count(kind="event", group_by="verb")` is explicitly deferred — the group-by
semantics require a new `GROUP BY` path in `SqlEventStore` that is disproportionate to the OSS
use case. This is noted as a known gap.

### D4: Schema migration — one composite index

The existing `events` DDL (line 487 in `crates/khive-db/src/stores/event.rs`) creates four
separate indexes:

```sql
CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);
CREATE INDEX IF NOT EXISTS idx_events_verb      ON events(verb);
CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);
CREATE INDEX IF NOT EXISTS idx_events_created   ON events(created_at DESC);
```

The dominant query pattern is `WHERE namespace = ? ORDER BY created_at DESC LIMIT ?` — the hot
path for unfiltered `list(kind="event")`. SQLite's query planner will use either the namespace
index or the created_at index, not both simultaneously. A composite index on
`(namespace, created_at DESC)` allows the planner to satisfy both the equality predicate and the
sort in a single index scan, avoiding a file sort for large event tables.

A new versioned migration adds:

```sql
CREATE INDEX IF NOT EXISTS idx_events_ns_created ON events(namespace, created_at DESC);
```

This index is additive — no existing index is dropped, no column is changed. The migration
follows ADR-022 (§"Migrations only" — append a new `VersionedMigration` with
`version = <last + 1>`). No data migration is required.

### D5: Runtime operations

A new method `list_events` on `KhiveRuntime` in `crates/khive-runtime/src/operations.rs`
provides the typed bridge from the pack handler to the store:

```rust
pub async fn list_events(
    &self,
    namespace: Option<&str>,
    filter: EventFilter,
    limit: u32,
    offset: u32,
) -> RuntimeResult<Page<StorageEvent>> {
    let store = self.events(namespace)?;
    store.query_events(filter, PageRequest { limit, offset }).await
        .map_err(RuntimeError::from)
}
```

`KhiveRuntime::events(namespace)` already exists (line 153 of
`crates/khive-runtime/src/runtime.rs`). No new field is added to `KhiveRuntime`.

The `get` verb handler, when resolving a UUID, is extended to fall through to
`EventStore::get_event` after checking entities, notes, and edges — consistent with the existing
`Resolved` enum pattern (line 30 in `operations.rs`).

## Rationale

### Why `list(kind="event")` and not dedicated verbs `list_events` / `search_events`

ADR-023 §"Verb shape" established `kind=` as the substrate discriminant for exactly this expansion
pattern: "If we later add events as observables, the verb surface absorbs them without growing."
Dedicated verbs would add a second way to reach the same handler, split documentation, and
contradict ADR-027's single-surface principle. The `kind=` form is what agents already learn for
`list(kind="entity")` and `list(kind="note")`.

The discoverability concern (dedicated verbs are more explicit in `tools/list`) does not apply
here — ADR-027's dynamic verb catalog in the `request` description already lists all verbs and
their valid `kind` values. Adding `"event"` to the `list` description is a one-line catalog
update.

### Why FTS-over-events is excluded in v0.1

The `Event.data` field is an optional JSON blob (`Option<Value>` in storage). Full-text search
over JSON requires either indexing the serialized string (fragile, order-dependent) or extracting
fields explicitly (schema knowledge the FTS engine doesn't have). The `EventFilter` predicate set
covers every access pattern known at time of writing. Deferring FTS avoids locking a
potentially wrong text extraction strategy.

### Why outcome filtering is post-query in v0.1

`EventFilter` is a public type in `khive-storage` — adding an `outcomes` field is a breaking
change to downstream crates that construct `EventFilter` directly. The post-query filter avoids a
`khive-storage` semver event for a feature that personal deployments will rarely need (most events
are `success`; deny events are the interesting minority at low volume). When volume justifies
index-backed filtering, the `EventFilter` can be extended under a semver bump.

### Why the composite index is added now

The four single-column indexes that exist today cannot serve `WHERE namespace = ? ORDER BY
created_at DESC` without a file sort. On a fresh deployment this is invisible. On a deployment
that has accumulated thousands of events — the metering daemon ADR-034 anticipates — it degrades
to a full table scan with sort. The composite index is a one-line DDL addition with no schema
risk; deferring it means every deployment that grows to non-trivial event counts hits the
degradation before a migration is available.

### Why `create/update/delete` over events return errors rather than silently no-op

Silent no-ops on a prohibited operation hide bugs in agent code. An agent that calls
`delete(id=<event_uuid>)` expecting events to be mutable has a logic error; returning a clear
error ("events are immutable") surfaces that immediately instead of letting the agent conclude the
delete succeeded (it would return not-found if the UUID-resolution path simply skipped events).

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
|---|---|---|---|
| Dedicated verbs `list_events` / `get_event` | More explicit in docs; no `kind=` lookup | Contradicts ADR-023 verb consolidation; adds two verbs that duplicate existing structure | ADR-023 reserved `kind=event` exactly for this; dedicated verbs are net regression |
| GQL/SPARQL over events via `query(...)` | Uniform query interface | Events are tabular with no edges; GQL graph patterns are meaningless; adds implementation burden for zero benefit | Wrong tool; filter-based listing is the correct abstraction for tabular data |
| FTS (`search(kind="event", query=...)`) | Uniform with entity/note search | `data` field is JSON; no natural FTS column; useful queries are predicate-based, not similarity-based | Deferred — requires explicit text-extraction strategy; not needed for v0.1 use cases |
| Expose `count(kind="event", group_by=...)` now | Dashboard convenience | Requires new GROUP BY path in `SqlEventStore`; disproportionate to OSS use case | Deferred to v0.2; client-side aggregation is sufficient at OSS event volumes |
| Post-query outcome filter (this ADR's D2 choice) | No `EventFilter` semver bump | Full page scanned before filtering when outcome narrows significantly | Accepted for v0.1: deny volumes are small; a future semver bump can add index-backed filtering |
| Index-backed outcome filter in `EventFilter` now | DB-level precision | Breaking change to `EventFilter`; semver event for all `khive-storage` consumers | Defer to v0.2 when volume justifies it |
| Skip composite index until needed | Zero migration complexity | Predictable query degradation as event tables grow; prevents ADR-034 metering daemon from being efficient | One-line DDL addition now prevents all deployments from hitting the degradation |

## Consequences

### Positive

- The Event substrate becomes fully first-class: created, queryable, and discoverable through
  the same verb surface as Notes and Entities.
- Downstream metering consumers (ADR-034 §"Future Work") can poll `list(kind="event",
  verb="...", since=...)` via MCP without coupling to SQLite internals.
- The `Resolved::Event` variant in `operations.rs` (previously unreachable from `get`) becomes
  active — `get(id=<event_uuid>)` returns the event record.
- The composite index on `(namespace, created_at DESC)` eliminates file sorts on the dominant
  query path before they become observable.

### Negative

- The `list` handler in `khive-pack-kg` grows an event branch. Maintenance surface increases
  proportionally (one new match arm, one new params sub-struct).
- Outcome filtering is post-query in v0.1. A caller filtering on `outcome="denied"` over a
  namespace with many success events fetches more rows than needed. Acceptable at OSS scale;
  not acceptable for high-volume cloud deployments (tracked for v0.2).
- The `get` verb's resolution loop adds a third storage call in the miss path (entity miss →
  note miss → event lookup). In practice, agents that call `get(id=...)` know their substrate;
  this degradation is only observed on genuine unknown-UUID calls.

### Neutral

- No new top-level verbs are added. `tools/list` continues to return exactly one tool
  (`request`). The catalog description gains `kind="event"` under `list`; `get` continues to
  use UUID resolution with no `kind` parameter.
- Events are never returned by `search(kind="entity")` or `search(kind="note")`. The substrates
  remain independent; cross-substrate navigation for events is not in scope.
- The immutability contract (`create/update/delete` return errors) is a new enforcement path,
  not new behavior — events were always immutable; the error just makes the boundary explicit.

## Implementation

No code is written as part of this ADR. The following table describes the changes that an
implementation PR will make:

| Step | File | Change |
|---|---|---|
| 1. Composite index migration | `crates/khive-db/src/migrations.rs` | Add `VersionedMigration { version: <next>, sql: "CREATE INDEX IF NOT EXISTS idx_events_ns_created ON events(namespace, created_at DESC)" }` |
| 2. `list_events` runtime op | `crates/khive-runtime/src/operations.rs` | Add `KhiveRuntime::list_events(namespace, filter, limit, offset)` calling `self.events(namespace)?.query_events(...)` |
| 3. `get` UUID resolution extension | `crates/khive-pack-kg/src/handlers.rs` | In `handle_get`, extend UUID resolution to check `EventStore::get_event` after entity/note/edge misses (no `kind` parameter — `get` auto-detects substrate from UUID). Alternatively, refactor `handle_get` to use `KhiveRuntime::resolve` (which already checks events at `operations.rs` line 706) and branch on `Resolved::Event`. |
| 4. `ListParams` event branch | `crates/khive-pack-kg/src/handlers.rs` | Add event sub-struct to `ListParams`; add `KindSpec::Event` match arm in `handle_list`; construct `EventFilter` from wire params; apply post-query outcome filter |
| 5. Immutability guards | `crates/khive-pack-kg/src/handlers.rs` | `handle_create`, `handle_update`, and `handle_delete`: return `ImmutableRecord { kind: "event" }` error when the target `kind` or resolved UUID identifies an event record. For `handle_update`, the check must occur after UUID resolution (to catch callers who pass an event UUID without an explicit `kind`) but before any patch is applied. |
| 6. Vocab registration | `crates/khive-pack-kg/src/vocab.rs` | Register `"event"` as a valid `kind` for `list` in `KgVocab::valid_list_kinds()` (`get` does not use `kind` — it resolves UUID directly) |
| 7. Smoke test | `tests/smoke_test.py` | After dispatching at least one verb that writes an audit event (a `create` call is sufficient — every verb dispatch produces an `Event` if the store is wired), call `list(kind="event", limit=5)` and assert the response contains at least one item with the expected `verb` and `outcome` fields |

**Prerequisite:** ADR-035 landed dispatch-time `EventStore` persistence (closing the item ADR-033
originally deferred to v0.3). This ADR depends on that accepted surface and adds
`list(kind="event")` and `get(id=<event_uuid>)` query access on top of the persistence wiring.

**No changes to:** `khive-gate`, `khive-types`, `khive-storage` (other than the composite index
migration which touches `khive-db`). The `EventFilter` type is used as-is from
`khive-storage::event::EventFilter`; no new fields are added in this ADR.

## References

- [ADR-004](ADR-004-substrate-observables.md): Event as the third substrate — §"Why Event immutable?"; `SubstrateKind` enum; `EventStore` trait family
- [ADR-023](ADR-023-verb-consolidated-mcp-surface.md): Verb-consolidated surface — §"Versioning tools" (anticipates `kind="event"`); `kind=` discriminant spec; `ListParams` shape
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single tool MCP surface — single dispatch site invariant; dynamic verb catalog
- [ADR-033](ADR-033-audit-envelope.md): Audit envelope — `AuditEvent` type; Implementation Status table rows "Query surface" and "EventStore wiring" (deferred to v0.3; this ADR delivers the query surface portion)
- [ADR-034](ADR-034-identity-session-metering-hooks.md): Identity, session, and metering hooks — §"Future Work / AuditEvent pull-based consumers": "a pattern emerges for any consumer to poll the event log"; this ADR delivers that polling surface
- [ADR-022](ADR-022-schema-migrations.md): Schema migrations — migration system used for the composite index addition
- `crates/khive-types/src/event.rs`: `Event`, `EventOutcome`, `EventBuilder` types
- `crates/khive-storage/src/event.rs`: `EventStore` trait, `EventFilter`, storage-level `Event`
- `crates/khive-db/src/stores/event.rs`: `SqlEventStore` — `query_events` (line 417), `count_events` (line 467), `build_event_filter_sql` (line 213), existing DDL and indexes (line 487)
- `crates/khive-runtime/src/operations.rs`: `Resolved::Event` variant (line 32) — already present, not yet reachable from `get`; `KhiveRuntime::events()` (line 153 in `runtime.rs`)
- `crates/khive-pack-kg/src/handlers.rs`: `ListParams` (line 192), `resolve_kind_spec` (line 93), `handle_list` (line 517)
- GitHub issue [#5](https://github.com/ohdearquant/khive/issues/5): Events surface — scope, dependencies, priority
