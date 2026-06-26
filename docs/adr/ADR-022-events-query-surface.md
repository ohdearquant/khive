# ADR-022: Events Query Surface

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive

## Context

[ADR-004](ADR-004-substrate-observables.md) declares Event as the third substrate
alongside Note and Entity, with the explicit semantic "what happened" — the
append-only operation audit log. The `EventStore` trait exists at the storage layer
([ADR-005](ADR-005-storage-capability-traits.md)) and dispatch-time event emission is
wired in via the gate audit path ([ADR-018](ADR-018-authorization-gate.md)).

What is missing: a query surface. Events are appended at the storage level, but no verb
handler exposes them through the agent surface. Downstream consumers that want to:

- Poll for recent audit events ("did anything get denied in the last hour?")
- Build observability dashboards over operation history
- Correlate gate denials with specific entity changes
- Audit a session's full operation log

...have no MCP path. They must reach into the database directly, bypassing the
namespace isolation contract ([ADR-007](ADR-007-namespace.md)) and coupling to SQLite
internals.

[ADR-016](ADR-016-request-dsl.md) anticipated this: events-as-observables become a new
`kind=` value on existing verbs rather than introducing event-specific verbs. This ADR
activates that anticipation.

### Scope

This ADR specifies the MCP query surface for events: which verbs accept `kind="event"`,
the filter shape, what is explicitly excluded, and the storage-layer index addition
required to make the dominant query pattern efficient. It does NOT modify the Event
substrate itself ([ADR-004](ADR-004-substrate-observables.md)) or change which
operations emit events ([ADR-018](ADR-018-authorization-gate.md)).

## Decision

### 1. Verb shape — `list(kind="event")` and `get(id=<uuid>)`

Events are exposed through the existing `list` verb via the `kind="event"` discriminator,
and through `get` via UUID auto-detection. No new top-level verbs are introduced.

**`list(kind="event", ...)`** is the primary access path. It maps to
`EventStore::query_events` with an `EventFilter` constructed from the wire parameters
and a `PageRequest` derived from `limit`/`offset`.

**`get(id=<uuid>)`** already resolves across substrates (entities, notes, edges). This
ADR extends resolution to also check `EventStore::get_event` when the UUID is not found
in the entity, note, or edge tables. The `Resolved::Event` variant already exists in
the runtime but is unreachable from `get` until this wiring lands.

**`get(id=...)`** on the events surface resolves event UUIDs. For aggregate-ID lookup
(e.g., a `proposal_id` that threads multiple lifecycle events together), the pack
owning the aggregate provides its own resolution verb. Do not overload bare
`get(id=...)` for both event-UUID and aggregate-ID lookup — that creates ambiguous
collision policy. See ADR-046 for the proposal pack's resolution path.

**`create(kind="event", ...)`**, **`update`** over events, and **`delete`** over events
are **prohibited**. Events are immutable by construction ([ADR-004](ADR-004-substrate-observables.md))
— the pack handler returns `InvalidParams("events are immutable — create/update/delete
are not permitted")` if called with an event target. For `update` and `delete`, the
check occurs after UUID resolution (to catch callers passing an event UUID without an
explicit `kind`) but before any patch is applied.

Making the constraint explicit (a clear error) rather than implicit (silent no-op) surfaces
agent logic errors immediately. An agent that calls `delete(id=<event_uuid>)` expecting
events to be mutable has a bug; returning a clear error tells them so.

**`search(kind="event", query=...)`** is **out of scope for v1**. Events have a JSON
`data` field but no natural full-text column. FTS over JSON-as-text is awkward, and the
useful queries are predicate-based (filter by verb/outcome/actor/time range), not
similarity-based. The `EventFilter` predicate set covers all practical access patterns.
FTS is tracked as future work; deferring it avoids locking a potentially wrong
text-extraction strategy.

**`query(...)` (GQL/SPARQL)** is **excluded**. Events are tabular with no edges and no
graph structure. GQL pattern matching over events would be gratuitous.

### 2. Filter shape for `list(kind="event", ...)`

The wire parameters map to the existing `EventFilter` struct from the storage layer:

| Wire parameter | Type         | Maps to `EventFilter` field      | Notes                                    |
| -------------- | ------------ | -------------------------------- | ---------------------------------------- |
| `kind`         | string       | `kinds: Vec<EventKind>`          | Single typed event kind; one-element vec |
| `kinds`        | [string]     | `kinds: Vec<EventKind>`          | Multi-value form; merged with `kind`     |
| `verb`         | string       | `verbs: Vec<String>`             | Single verb; stored as one-element vec   |
| `verbs`        | [string]     | `verbs: Vec<String>`             | Multi-value form; merged with `verb`     |
| `outcome`      | string       | (post-filter — see below)        | `"success"` \| `"denied"` \| `"error"`   |
| `actor`        | string       | `actors: Vec<String>`            | Exact match on free-form actor string    |
| `substrate`    | string       | `substrates: Vec<SubstrateKind>` | `"note"` \| `"entity"` \| `"event"`      |
| `since`        | int (μs UTC) | `after: Option<i64>`             | Exclusive lower bound on `created_at`    |
| `until`        | int (μs UTC) | `before: Option<i64>`            | Exclusive upper bound on `created_at`    |
| `limit`        | u32          | `PageRequest::limit`             | Default 100, max 1000                    |
| `offset`       | u32          | `PageRequest::offset`            | Default 0                                |

**Outcome post-filter.** `EventOutcome` is not in `EventFilter` — the existing SQL builder
does not push outcome into the WHERE clause. The handler applies `outcome` as a
post-query scan: iterate raw event pages internally (fetching `limit`-sized batches),
apply the outcome predicate per row, skip the first `offset` matches, collect `limit`
matches, and stop when either `limit` matches are collected or the store returns fewer
rows than requested (EOF).

A bounded-scan ceiling of `(offset + limit) * 20` total raw rows prevents unbounded
iteration; if the ceiling is reached before `limit` matches are collected, the handler
returns a short page.

This avoids a breaking change to `EventFilter` (which is a public storage type) for a
feature that personal-deployment volumes will rarely stress. When volume justifies
index-backed filtering, `EventFilter` can be extended under a semver bump.

**Namespace isolation.** The handler always forces the caller's namespace into the
filter; the wire-level `namespace` parameter (if any) is dropped before reaching the
storage layer. Callers cannot enumerate events from foreign namespaces — this matches
the namespace isolation contract from [ADR-007](ADR-007-namespace.md).

**Default ordering.** `query_events` orders by `(created_at DESC, event_id DESC)` —
the §3b canonical newest-first order — returning the most recent events first and
disambiguating clock ties deterministically. Not overridable in v1.

### 3. Aggregation — deferred

`count_events` exists on `EventStore` but a `count(kind="event", ...)` verb is **not**
added in this ADR. Downstream consumers needing aggregate counts can issue
`list(kind="event", limit=1000)` and aggregate client-side; typical event volumes
make this feasible.

A future verb `count(kind="event", group_by="verb")` is explicitly deferred — group-by
semantics require a new `GROUP BY` path in the SQL builder that is disproportionate to
the current use case.

### 3a. Cognitive-primitive framing — `EventFilter` ↔ `Objective<Event>`; aggregators are `Fold<Event, State>`

The events query surface (this ADR), the audit-emit path ([ADR-018](ADR-018-authorization-gate.md)),
and the pack-internal aggregators (brain's posteriors per ADR-032, future analytics)
form one pipeline whose layers map cleanly onto the cognitive primitives from
[ADR-024](ADR-024-fold-cognitive-primitives.md):

```
EventStore (ADR-005)              ──  IO layer (append + query)
        │
        ▼
Event substrate (ADR-004)         ──  plain typed records
        │
        ▼   EventFilter (canonical) — SQL WHERE OR in-memory matches()
        ▼   .as_objective() → Objective<Event> adapter view
        ▼
Filtered event stream
        │
        ▼   reduce via Fold<Event, State>
        ▼
Consumer state                    ──  brain posteriors, audit summary, dashboard, etc.
```

**`EventFilter` is canonical for v1 predicates.** It is the SQL-executable predicate
type used by the storage layer (`EventStore::query_events`) and the same predicate
applied in-memory by consumers replaying events. Arbitrary `Objective<Event>` impls are
NOT required to compile to SQL in v1 — that is an explicit non-goal. SQL compilation of
arbitrary objectives requires a partial-compilation story, failure modes, capability
detection, and a predicate AST — out of scope.

**`EventFilter` is a closed struct, not an open trait.** Every field maps to a SQL
predicate that the storage layer knows how to lower. Pack consumers compose filters
by setting fields; they do not extend the type. New predicates require a new field
plus a semver bump on `EventFilter` and on `EventStore::query_events`. The closed
shape is what makes "WHERE pushdown" a contract rather than a hope.

The v1 field set and its SQL lowering:

| Field        | Type                 | SQL fragment                                                                                                                                             |
| ------------ | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `kinds`      | `Vec<EventKind>`     | `kind IN (?, ?, …)` (omitted if empty)                                                                                                                   |
| `verbs`      | `Vec<String>`        | `verb IN (?, ?, …)` (omitted if empty)                                                                                                                   |
| `actors`     | `Vec<String>`        | `actor IN (?, ?, …)` (omitted if empty)                                                                                                                  |
| `substrates` | `Vec<SubstrateKind>` | `substrate IN (?, ?, …)` (omitted if empty)                                                                                                              |
| `after`      | `Option<i64>`        | `created_at > ?` (omitted if None)                                                                                                                       |
| `before`     | `Option<i64>`        | `created_at < ?` (omitted if None)                                                                                                                       |
| `session_id` | `Option<Uuid>`       | `session_id = ?` (omitted if None; ADR-041)                                                                                                              |
| `observed`   | `Vec<Uuid>`          | `EXISTS (SELECT 1 FROM event_observations o WHERE o.event_id = events.id AND o.entity_id IN (?, …))` (omitted if empty; ADR-041)                         |
| `selected`   | `Vec<Uuid>`          | `EXISTS (SELECT 1 FROM event_observations o WHERE o.event_id = events.id AND o.role = 'selected' AND o.entity_id IN (?, …))` (omitted if empty; ADR-041) |

The full predicate is the AND of all non-empty/non-None fields, ANDed with the
implicit namespace filter forced by the handler (§2). Match-all is an `EventFilter`
with all fields empty — `WHERE namespace = ?` only.

The `observed` and `selected` fields require the `event_observations` projection
table from ADR-041's migration to exist; the storage layer MUST reject filters
that set these fields against a database whose schema predates that migration.
The `session_id` field requires the `session_id` column on `events` from the same
migration.

### Filter semantics

- `kinds`: typed event discriminants. Use when consumers handle specific `EventKind`
  variants (projections, workers, replay). Example: a brain profile waking only on
  `EventKind::RecallExecuted`.
- `verbs`: causal/audit surface. Use when querying "which events were caused by request
  verb X" (audit, debugging). Example: "show me all events caused by `merge`."
- Within one field, values are OR'd (`kind IN (...)` / `verb IN (...)`).
- Across fields, predicates are AND'd (`kind IN (...) AND verb IN (...)`).
- Namespace scoping is supplied by `&NamespaceToken` at call entry, not as an
  `EventFilter` field.

Wire shape (request DSL):

- `kind=...` or `kinds=[...]` → `kinds: Vec<EventKind>`
- `verb=...` or `verbs=[...]` → `verbs: Vec<String>`

Both forms are accepted on the wire. Internally the filter struct names are plural.

Future predicate kinds become new fields on this struct with their own lowering
rule. Each addition is a semver bump on `EventFilter` and on `EventStore::query_events`
— this is non-negotiable, since the closed shape is what makes "WHERE pushdown"
a contract rather than a hope. Nesting (And/Or/Not trees) is explicitly deferred
— every consumer use case today is expressible as the conjunction of the listed
fields. If a real need for disjunctive event filters emerges, it gets its own ADR.

The `Objective<Event>` connection is provided via an adapter:

```rust
impl EventFilter {
    /// In-memory predicate evaluation. Semantics MUST match the SQL WHERE generator.
    pub fn matches(&self, event: &Event) -> bool { /* ... */ }

    /// View this filter as an Objective<Event>; passes return 1.0, fails return 0.0.
    pub fn as_objective(&self) -> EventFilterObjective<'_> {
        EventFilterObjective { filter: self }
    }
}

pub struct EventFilterObjective<'a> { filter: &'a EventFilter }

impl<'a> Objective<Event> for EventFilterObjective<'a> {
    fn score(&self, event: &Event, _ctx: &ObjectiveContext) -> f64 {
        if self.filter.matches(event) { 1.0 } else { 0.0 }
    }
}
```

This buys shared semantics without making the storage layer depend on arbitrary
objective compilation. Consumers that want a typed Objective surface get one; the
storage path stays SQL-native.

**The effective event predicate is `EventFilter` + handler-level outcome filter.** §2
of this ADR defines `outcome` as a post-query filter (not in `EventFilter` itself). For
consumers that need a single predicate object, future work may introduce:

```rust
pub struct EventQueryPredicate {
    pub filter:   EventFilter,
    pub outcomes: Vec<EventOutcome>,
}
```

with `matches` / `as_objective` semantics. Deferred until a consumer needs it.

**Pack-internal event aggregators SHOULD implement `Fold<Event, State>`** from ADR-024
rather than defining ad-hoc reducer types. Brain's `BalancedRecallProfile` is exactly
this — it accumulates posterior state from feedback events. Future audit summaries,
validation reports over event streams, or dashboard counters fit the same abstraction.
The combinators from ADR-024 (`SequentialFold`, `FilterFold`, `MapFold`, `DualFold`)
compose naturally over event streams.

**Dispatch boundary**: the runtime is responsible for event _delivery_, not Fold
_execution_. Runtime-level Fold registries are out of scope for v1. Pack consumers
own their state, snapshots, schema migrations, and cursor persistence — see ADR-017's
`PackEventConsumer` trait for the dispatch contract.

### 3b. Event ordering — weakly monotonic timestamps + deterministic tiebreaker

Events have **weakly monotonic** `created_at` timestamps. Equal timestamps are legal
(clock-skew, multi-threaded inserts in the same microsecond). Strict monotonicity is
not required and would be expensive to enforce across threads, processes, and storage
backends.

**Canonical replay order** (for `Fold<Event, State>` consumers and any deterministic
catch-up):

```sql
ORDER BY created_at ASC, event_id ASC
```

**Canonical newest-first order** (for human-facing listing):

```sql
ORDER BY created_at DESC, event_id DESC
```

`event_id` is compared by canonical UUID byte order. It does NOT need to encode true
insertion order — it only needs to provide a stable total order among events with
equal `created_at`. UUID v4's randomness is sufficient; UUID v7's time-ordering is
welcome but not required.

**Timestamp-only cursors are unsafe.** A consumer that resumes with `since=t` after
processing event `(created_at=t, event_id=A)` may skip `(created_at=t, event_id=B)`.
Event consumers MUST use a compound cursor:

```rust
pub struct EventCursor {
    pub created_at: i64,    // microseconds UTC
    pub event_id:   Uuid,
}
```

Ascending catch-up query (the replay form):

```sql
WHERE created_at > :ts
   OR (created_at = :ts AND event_id > :id)
ORDER BY created_at ASC, event_id ASC
```

Newest-first (the list form):

```sql
WHERE created_at < :ts
   OR (created_at = :ts AND event_id < :id)
ORDER BY created_at DESC, event_id DESC
```

`EventStore` implementations that return ordered results MUST include `event_id` as
the deterministic tiebreaker. The `since` / `until` parameters on `list(kind="event")`
remain timestamp-only (human-facing); programmatic event consumers persist the full
`EventCursor` themselves.

**Deferred**: a monotonic append-sequence counter (`event_seq`) is NOT introduced in
v1. The timestamp+id pair gives deterministic replay order. A true commit-order
counter would only be needed if a consumer requires distinct "happened-before" causal
ordering — a different invariant from replay determinism. Add when evidence demands.

### 4. Composite index — schema migration

The existing events table has four single-column indexes (`namespace`, `verb`,
`substrate`, `created_at DESC`). The dominant query patterns are:

1. Unfiltered listing: `WHERE namespace = ? ORDER BY created_at DESC LIMIT ?`
2. Cursor-based catch-up (§3b): `WHERE namespace = ? AND (created_at > :ts OR
   (created_at = :ts AND event_id > :id)) ORDER BY created_at ASC, event_id ASC`

A composite index covering both `created_at` and `event_id` lets the planner satisfy
the equality predicate, range scan, sort, AND tiebreaker in a single index walk —
without it, the tiebreaker forces a row-by-row lookup or a file sort once
multiple events share a microsecond.

A new versioned migration ([ADR-015](ADR-015-schema-migrations.md)) adds:

```sql
CREATE INDEX IF NOT EXISTS idx_events_ns_created_id
    ON events(namespace, created_at DESC, id DESC);
```

(Use `id` or `event_id` per the storage column name — see `crates/khive-storage/src/event.rs`.)

Additive. No existing index dropped. No data migration. The migration is required at
the time this ADR is implemented; deferring it means every deployment that grows to
non-trivial event counts hits both the file-sort degradation AND the tiebreaker
lookup penalty before a migration is available.

### 5. Runtime operations

A new method `list_events` on the runtime provides the typed bridge from the pack
handler to the store:

```rust
pub async fn list_events(
    &self,
    token: &NamespaceToken,
    filter: EventFilter,
    page: PageRequest,
) -> RuntimeResult<Page<Event>>;
```

Namespace scoping is supplied via `&NamespaceToken`, not via a string parameter on
`EventFilter` or call signatures. Per ADR-007, the token is the authorization proof;
raw namespace strings are not trusted at the events query boundary. The runtime
derives the namespace from the token internally when constructing the store call.

The `get` verb's UUID resolution is extended to fall through to `EventStore::get_event`
after entity/note/edge misses.

No new field on `KhiveRuntime`. `runtime.events(namespace)` already exists at the
storage-trait layer.

## Rationale

### Why `list(kind="event")` instead of dedicated `list_events` / `search_events`

[ADR-016](ADR-016-request-dsl.md) established `kind=` as the substrate discriminator for
exactly this expansion pattern. Adding `kind="event"` is a one-line catalog update;
adding dedicated verbs would split documentation, contradict the single-surface
principle, and force agents to learn a second naming convention for the same operation
shape.

The discoverability concern (dedicated verbs are more explicit) does not apply here —
the dynamic verb catalog in the `request` description already lists all verbs and their
valid `kind` values.

### Why FTS over events is excluded in v1

The `Event.data` field is `Option<serde_json::Value>`. Full-text search over JSON
requires either indexing the serialized string (fragile, key-order-dependent) or
extracting fields explicitly (schema knowledge FTS does not have). The `EventFilter`
predicate set covers every access pattern currently known: filter by verb, outcome,
actor, substrate, time range. Deferring FTS avoids locking a wrong text-extraction
strategy before the use case is concrete.

### Why outcome filter is post-query

`EventFilter` is a public type in the storage crate. Adding an `outcomes` field is a
breaking change to downstream consumers that construct `EventFilter` directly. The
post-query filter avoids that semver event for a feature that personal deployments
rarely stress (most events are `success`; deny events are the interesting minority at
low volume).

When volume justifies index-backed filtering, `EventFilter` can be extended under a
semver bump. The post-query approach in v1 is the right tradeoff between completeness
and stability.

### Why the composite index is added now

The single-column indexes cannot serve the dominant query pattern without a file sort.
At low event volume this is invisible; at thousands of events it degrades to a full
table scan with sort. The composite index is a one-line DDL addition with no schema
risk; deferring it means every deployment that grows beyond the trivial case hits the
degradation before a migration is available.

### Why `create`/`update`/`delete` over events return errors rather than silently no-op

Silent no-ops on a prohibited operation hide bugs in agent code. An agent that calls
`delete(id=<event_uuid>)` expecting events to be mutable has a logic error; returning a
clear error ("events are immutable") surfaces it immediately instead of letting the
agent conclude the operation succeeded.

For `update`, the check must occur after UUID resolution (so callers who pass an event
UUID without explicit `kind` are caught) but before any patch is applied (so no
partial mutation happens before the rejection).

## Alternatives Considered

| Alternative                                            | Why rejected                                                                                      |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------- |
| Dedicated verbs `list_events` / `get_event`            | Contradicts ADR-016 verb consolidation; duplicates existing structure; splits documentation.      |
| GQL/SPARQL over events via `query(...)`                | Events are tabular with no edges; graph patterns are meaningless on row data.                     |
| FTS via `search(kind="event", query=...)`              | `data` field is JSON; no natural FTS column; useful queries are predicate-based. Deferred.        |
| `count(kind="event", group_by=...)` now                | Requires new GROUP BY path in storage; disproportionate to current use case. Deferred.            |
| Index-backed outcome filter (extend `EventFilter`)     | Breaking semver event for the storage crate; current volume doesn't justify.                      |
| Skip composite index until needed                      | Predictable degradation as event tables grow; one-line DDL prevents it now.                       |
| Silent no-op on `create`/`update`/`delete` over events | Hides agent logic errors; explicit immutability error is more diagnostic.                         |
| Allow callers to override default ordering             | YAGNI; `created_at DESC` covers every known use case; can be added without breaking the contract. |

## Consequences

### Positive

- The Event substrate becomes fully first-class on the agent surface: created at dispatch
  time, queryable through `list`, retrievable through `get`.
- Downstream audit consumers can poll `list(kind="event", verb="...", since=...)` via the
  standard MCP surface without coupling to SQLite internals.
- The `Resolved::Event` runtime variant becomes reachable from `get`.
- The composite index on `(namespace, created_at DESC)` eliminates file sorts on the
  dominant query path before they become observable.

### Negative

- The list handler grows an event branch. Maintenance surface increases by one match arm
  plus the `EventFilter`-construction sub-struct. Bounded scope.
- Outcome filtering is post-query in v1. A caller filtering on `outcome="denied"` over a
  namespace with many success events fetches more rows than needed. Acceptable at
  typical scale; tracked for future when high-volume deployments stress it.
- The `get` UUID-resolution loop adds a third storage call in the miss path (entity miss
  → note miss → edge miss → event lookup). In practice agents calling `get` know the
  substrate; this degradation is only observable on unknown-UUID calls.

### Neutral

- No new top-level verbs. The MCP surface continues to expose `request` as the only tool
  ([ADR-016](ADR-016-request-dsl.md)). The catalog description gains `kind="event"`
  under `list`.
- Events are never returned by `search(kind="entity")` or `search(kind="note")`. The
  substrates remain independent; cross-substrate navigation for events is not in scope.
- The immutability contract is a new enforcement path, not new behaviour. Events were
  always immutable; the error just makes the boundary explicit.

## Implementation

### Schema migration

Migration V8 (`events_namespace_ts_id_idx`). The new query surface index is owned by this ADR. See ADR-015's Migration Ledger for the full version map.

```rust
// crates/khive-db/src/migrations.rs
VersionedMigration {
    version: 8,
    name: "events_namespace_ts_id_idx",
    up: "CREATE INDEX IF NOT EXISTS idx_events_ns_created_id \
         ON events(namespace, created_at DESC, id DESC);",
}
```

The trailing `id DESC` column serves the §3b tiebreaker — without it, the planner can
range-scan on `(namespace, created_at)` but must do a row-by-row lookup or file sort to
honor the `event_id` tiebreaker once multiple events share a microsecond.

### Runtime additions

```rust
// crates/khive-runtime/src/operations.rs
impl KhiveRuntime {
    pub async fn list_events(
        &self,
        token: &NamespaceToken,
        filter: EventFilter,
        page: PageRequest,
    ) -> RuntimeResult<Page<Event>> {
        let ns = token.namespace();
        let store = self.events(ns)?;
        store.query_events(filter, page).await.map_err(Into::into)
    }
}
```

### Pack handler additions

- `list` handler: add `KindSpec::Event` match arm; construct `EventFilter` from wire
  params; force caller's namespace; apply post-query outcome filter with the bounded
  over-fetch loop
- `get` handler: extend UUID resolution to check `EventStore::get_event` after
  entity/note/edge misses; branch on `Resolved::Event`
- `create`/`update`/`delete` handlers: add immutability guard that returns
  `InvalidParams("events are immutable")` when the target kind/UUID resolves to an event
- Vocab registration: register `"event"` as a valid `kind` for `list`. `get` does not
  use `kind` — UUID resolution does the dispatch

### Tests

| Scenario                                                | Assert                                                                                    |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| Dispatch a `create` verb; `list(kind="event", limit=5)` | Returns ≥1 event with the right `verb` and `outcome`                                      |
| `list(kind="event", outcome="denied")`                  | Returns only denied events; post-filter respects offset/limit                             |
| `list(kind="event", since=<t>, until=<t+1h>)`           | Time-bounded results, sorted DESC                                                         |
| `get(id=<event_uuid>)`                                  | Returns the event record                                                                  |
| `create(kind="event", ...)`                             | Returns `InvalidParams` error                                                             |
| `update(id=<event_uuid>, ...)`                          | Returns `InvalidParams("immutable")` error after resolution, before patch                 |
| `delete(id=<event_uuid>)`                               | Returns `InvalidParams("immutable")` error                                                |
| Cross-namespace isolation                               | Caller in namespace A cannot list events from namespace B even if `namespace=B` is passed |

## References

- [ADR-004](ADR-004-substrate-observables.md): Substrate observables — Event as the third
  substrate; immutability contract
- [ADR-005](ADR-005-storage-capability-traits.md): Storage capability traits — `EventStore`
  trait this ADR consumes
- [ADR-007](ADR-007-namespace.md): Namespace — isolation contract enforced by the
  handler
- [ADR-014](ADR-014-curation-operations.md): Curation operations — `update`/`delete`
  semantics that this ADR's immutability guard sits in front of
- [ADR-015](ADR-015-schema-migrations.md): Schema migrations — pattern for the composite
  index addition
- [ADR-016](ADR-016-request-dsl.md): Request DSL — `kind=` discriminator expansion this
  ADR uses; single MCP surface
- [ADR-018](ADR-018-authorization-gate.md): Authorization gate — dispatch-time event
  emission this ADR provides the query surface for
- `crates/khive-storage/src/event.rs`: `EventStore` trait, `EventFilter`, `Event` type
- `crates/khive-db/src/stores/event.rs`: `SqlEventStore` — `query_events`, existing DDL
  and indexes
- `crates/khive-runtime/src/operations.rs`: `Resolved::Event` variant
- `crates/khive-pack-kg/src/handlers.rs`: `handle_list`, `handle_get`, `handle_update`,
  `handle_delete` — extension points
