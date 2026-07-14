# ADR-041: Event Provenance Projection — Hybrid Log + Graph Edges

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Depends on**:

- ADR-004 (Substrate Observables — Event substrate)
- ADR-017 (Pack Standard — PackEventConsumer atomicity)
- ADR-022 (Events Query Surface — EventFilter, ordering, cursor)
- ADR-024 (Fold Cognitive Primitives)
- ADR-032 (Brain Profile Orchestration — Fold consumer of events)

---

## Context

[ADR-022](ADR-022-events-query-surface.md) establishes the event log as an append-only,
ordered, queryable substrate. Brain (ADR-032), validation pipelines (ADR-034), and
audit consumers fold over this log via `Fold<Event, State>`. The Event struct carries a
JSON `payload` field that holds operation-specific structured data:

```rust
pub struct Event {
    pub id:         Uuid,
    pub created_at: i64,
    pub namespace:  String,
    pub actor:      Option<String>,
    pub verb:       String,
    pub kind:       EventKind,
    pub payload:    serde_json::Value,
    // ...
}
```

The references inside `payload` to other substrate records (which memory entities a
recall observed, which note the feedback was _about_, which task transitioned out of
what state) are buried in JSON. To answer "what memories did recall events in the last
hour observe?" today, a consumer must scan every matching event row, deserialize its
payload, decode the candidate list, and aggregate in app code. That's a JSON-LIKE scan
where it should be an indexable JOIN.

The same data, modeled as edges, would make provenance queries graph-native:
`MATCH (e:event {kind:'recall'})-[:observed]->(m:memory) WHERE e.created_at > :since`.
But ADR-002's edge ontology is closed (15 edge relations — closed taxonomy; entity↔entity by default) and
ADR-022 §3b's append-only ordering invariant rests on events being a separate substrate
from entities. Promoting events into the entity table to "make them queryable" would
break replay determinism, cursor semantics, and PackEventConsumer atomicity (ADR-017).

> **Amended ([ADR-055](ADR-055-epistemic-edge-relations.md))**: ADR-055 added 2
> epistemic relations (`supports`, `refutes`); the current total is 17 edge relations.

This ADR resolves the tension via a **hybrid** model: the event log stays canonical
(append-only, ordered, cursor-anchored), and a **sibling projection table**
`event_observations` records the structured references at write time. Synthetic graph
edges expose the projection to GQL/SPARQL without touching the canonical edges table.

The Fold/Objective story (ADR-024 §"Pack-internal aggregators") gets materially cleaner
as a side effect — Fold consumers receive pre-decoded `EventView`s with typed
provenance instead of raw payload JSON. EventFilter (ADR-022 §3a) gains
`Observed(EntityId)` / `Selected(EntityId)` predicates that lower to SQL JOINs.

### Scope

This ADR specifies:

- The `event_observations` projection table and its schema.
- The four canonical observation roles (`Candidate`, `Selected`, `Target`, `Signal`).
- The write-time projection contract: which events project, what they project, in
  what transaction.
- The `EventView` type passed to `Fold` consumers.
- `EventFilter::Observed(EntityId)` / `Selected(EntityId)` field additions and their
  SQL lowering.
- Synthetic GQL/SPARQL edge exposure.
- Session-chain ordering (implicit, NOT a `followed_by` edge).

It does NOT:

- Modify the Event substrate (`events` table stays as ADR-022 specifies).
- Promote events into the entity substrate.
- Add new relations to ADR-002's closed edge ontology.
- Add `followed_by` or `caused_by` edges (deferred — implicit ordering covers v1).

---

## Decision

### 1. Two layers, not one

```text
┌─────────────────────────────────────────────────────────────┐
│ events                          (ADR-022 — canonical log)   │
│   id, created_at, namespace, actor, verb, kind, payload     │
│   append-only, ordered by (created_at, event_id)            │
│   primary store for Fold consumers (ADR-017 cursor anchor)  │
└─────────────────────────────────────────────────────────────┘
            │
            │  (same transaction, write-time projection)
            ▼
┌─────────────────────────────────────────────────────────────┐
│ event_observations              (ADR-041 — projection)      │
│   event_id, entity_id, role, position                       │
│   one row per (event, referenced substrate record, role)    │
│   queryable as synthetic edges via GQL/SPARQL               │
│   read-only outside the event emitter                       │
└─────────────────────────────────────────────────────────────┘
```

The `events` table remains the system of record. The `event_observations` table is a
**denormalized projection** of references that already exist in event payloads, lifted
out of JSON into a relational shape for efficient querying.

### 2. Schema

```sql
CREATE TABLE event_observations (
    event_id       TEXT NOT NULL,        -- FK to events.id (UUID string)
    entity_id      TEXT NOT NULL,        -- FK to entities.id OR notes.id (polymorphic
                                         -- by referent_kind below — substrate table is
                                         -- determined by referent_kind, not enforced as
                                         -- a hard FK because the projection spans two
                                         -- substrate tables)
    referent_kind  TEXT NOT NULL,        -- "entity" | "note"
    role           TEXT NOT NULL,        -- see §3
    position       INTEGER NOT NULL,     -- 0-based ordering within (event_id, role)
    PRIMARY KEY (event_id, role, position)
);

CREATE INDEX idx_event_obs_entity      ON event_observations(entity_id, role);
CREATE INDEX idx_event_obs_event_role  ON event_observations(event_id, role);
```

**Why `referent_kind`** instead of one column per substrate: the observed referent may
be an entity (a `person` who provided feedback) or a note (a `memory` returned by
recall). Two `*_id` columns with one NULL would split the index; a single polymorphic
column with a `referent_kind` discriminator keeps the schema flat and the indexes
useful. The dispatch is on `referent_kind`, validated at write time against the loaded
pack registry.

**Why no `created_at` on the projection**: the projection inherits temporal context
from `events.created_at` via the `event_id` FK. Duplicating it would double the storage
and create a denormalization invariant the runtime would have to enforce.

**Why `position`**: candidate lists, selected lists, and signal arrays are ordered;
recall returns N candidates in a deterministic order; the rerank output position
matters. `position` captures that. For roles where order is meaningless (e.g., a
single `Target`), `position = 0`.

### 3. Observation roles — closed enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRole {
    /// One of the candidates surfaced to the consumer.
    /// Example: each memory returned by `recall.candidates`; each rerank input.
    Candidate,
    /// The candidate(s) actually chosen by the consumer.
    /// Example: the top-K returned by `recall`; the rerank top-N.
    Selected,
    /// The primary target the operation acted on.
    /// Example: the entity created by `create`; the note updated by `update`;
    /// the task transitioned by `transition`.
    Target,
    /// A feedback or signal record attached to the event.
    /// Example: the entity a `FeedbackExplicit` event is about; the memory a
    /// `brain.feedback` call rates.
    Signal,
}
```

Four roles cover every emit pattern across the v1 pack set (kg, gtd, memory, brain,
future). Roles are closed; extending requires this ADR.

Per-verb role mapping (the v1 contract emitters MUST honor):

| Event kind                                        | Roles emitted                                                                    | Notes                                                                                           |
| ------------------------------------------------- | -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `RecallExecuted`                                  | `Candidate` (per candidate, ordered by pre-rerank score), `Selected` (per top-K) | The selected list is a subset of candidates; both rows exist (different `role` discriminators). |
| `SearchExecuted`                                  | `Candidate`, `Selected`                                                          | Mirror of recall.                                                                               |
| `RerankExecuted` (ADR-042)                        | `Candidate`, `Selected`                                                          | Rerank's input candidates from recall; rerank's output as selected.                             |
| `LinkCreated`                                     | `Target` (source), `Target` (target) — `position=0` and `position=1`             | Both endpoints.                                                                                 |
| `EntityCreated`, `EntityUpdated`, `EntityDeleted` | `Target`                                                                         | The acted-upon entity.                                                                          |
| `NoteCreated`, `NoteUpdated`, `NoteDeleted`       | `Target`                                                                         | The acted-upon note.                                                                            |
| `TaskTransitioned`                                | `Target`                                                                         | The task.                                                                                       |
| `FeedbackExplicit`                                | `Signal`                                                                         | The entity/note the feedback is about.                                                          |
| `MemoryConsolidated` (future)                     | `Candidate` (memories merged), `Selected` (resulting memory)                     | Future.                                                                                         |

Other event kinds (e.g., audit-only events with no substrate references) project zero
rows. The projection is opt-in per event kind, not mandatory.

### 4. Write-time contract

The runtime is responsible for projection. When a verb handler emits an event via the
dispatch path (ADR-018), the runtime inspects the event kind, extracts the references
from the payload via a per-kind decoder, and writes the corresponding
`event_observations` rows **in the same SQLite transaction** as the `events` row
insert.

```rust
// Inside the runtime's emit_event path:
let mut tx = self.events_db.begin().await?;
tx.insert_event(&event).await?;
let rows = match event.kind {
    EventKind::RecallExecuted => decode_recall_observations(&event)?,
    EventKind::LinkCreated    => decode_link_observations(&event)?,
    // ... per-kind decoder
    _                         => Vec::new(),
};
for row in rows {
    tx.insert_observation(&event.id, &row).await?;
}
tx.commit().await?;
```

If the per-kind decoder fails (malformed payload), the entire transaction aborts and
the event is NOT appended — payload errors are emitter bugs, not silent drops.

**Schema-on-write trade-off**: emitters commit at write time to a structured shape.
Adding a new event kind that should project requires (a) registering its decoder, (b)
defining its role mapping. Existing event kinds with no decoder produce no projection
rows — backward-compatible. Renaming or restructuring decoded fields requires an event
payload-schema migration (ADR-032 §3 mentions the migration registry).

### 5. EventView — the Fold consumer surface

PackEventConsumer (ADR-017) hands Folds enriched event views, not raw events:

```rust
pub struct EventView {
    pub event:        Event,
    pub observations: Vec<EventObservation>,
}

pub struct EventObservation {
    pub entity_id:     Uuid,
    pub referent_kind: ReferentKind,    // Entity | Note
    pub role:          ObservationRole,
    pub position:      u32,
}
```

The runtime fetches the `events` row + matching `event_observations` rows in one
JOIN before invoking `on_event`. The Fold body matches on `view.observations` for
typed provenance access:

```rust
fn reduce(&self, state: S, view: &EventView, _ctx: &FoldContext) -> S {
    let candidates: Vec<_> = view.observations.iter()
        .filter(|o| o.role == ObservationRole::Candidate)
        .collect();
    let selected = view.observations.iter()
        .find(|o| o.role == ObservationRole::Selected);
    // typed access, no JSON-LIKE
}
```

Fold purity (ADR-024 v1 invariants) holds: the dispatcher does the JOIN; the Fold
consumes pre-enriched values without IO. Atomicity (ADR-017): cursor + state +
observations all live in their respective tables, written/read in the dispatcher's
transaction.

The `Fold<L, S>` trait stays generic (ADR-024 §1) — `L` is whatever the impl
declares. PackEventConsumer dispatches by handing the consumer the `EventView`;
the consumer is responsible for picking the right call shape based on its Fold's
type parameter:

```rust
// For Fold<Event, S> impls — explicit unwrap, no Deref magic:
fold.reduce(state, &view.event, &fold_ctx);

// For Fold<EventView, S> impls (wishing typed observation access):
fold.reduce(state, view, &fold_ctx);
```

Existing `Fold<Event, State>` impls (e.g., `LoraEvolver` in ADR-032 §5b) continue
to compile unchanged — only their call site at the consumer changes from passing
an `&Event` directly to passing `&view.event`. New impls that need observation
access declare `Fold<EventView, State>` and consume `view.observations` directly.

EventView does NOT implement `Deref<Target=Event>`. Callers access the underlying
event explicitly via `view.event`. This prevents replay determinism from leaking
into the view-layer shape. Relying on auto-coercion across two trait shapes would
hide which form a fold expects and couple replay consumers to the view-layer type.

### 6. EventFilter extensions

ADR-022 §3a's closed-struct `EventFilter` gains three fields:

| Field        | Type           | SQL lowering                                                                                                                  |
| ------------ | -------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `observed`   | `Vec<Uuid>`    | `EXISTS (SELECT 1 FROM event_observations o WHERE o.event_id = events.id AND o.role = 'candidate' AND o.entity_id IN (?, …))` |
| `selected`   | `Vec<Uuid>`    | Same as above with `role = 'selected'`                                                                                        |
| `session_id` | `Option<Uuid>` | `session_id = ?`                                                                                                              |

The full WHERE clause stays the AND of all non-empty/non-None fields. The JOIN is a
sub-query the planner can optimize given the `idx_event_obs_entity` index — index seek
on `entity_id`, not a scan. Adding these three fields is a semver bump on `EventFilter`
and on `EventStore::query_events` per ADR-022 §3a's closed-struct rule.

The `observed` and `selected` fields are unusable against a database whose schema
predates this ADR's migration — the EXISTS sub-query references `event_observations`,
which only exists after the migration runs. The storage layer MUST reject filters
that set these fields when `event_observations` is absent (return a clean error,
not a SQL "no such table"). Same rule for `session_id` on the events column.

Brain profiles whose `event_filter` includes `observed: [memory_id]` wake only for
events that touched that memory. The earlier "every profile fires on every event"
fanout becomes "wake on graph-proximity" — meaningful for high-cardinality profile
deployments (ADR-032 §10).

### `served_by_profile_id` projection (forward-ref ADR-032)

ADR-004 reserves `served_by_profile_id: Option<Uuid>` inside the event payload for
events served by a brain profile. ADR-041 does NOT add a top-level EventFilter
field for this; profile-scoped event queries use the existing payload-extraction
path:

```rust
EventFilter::default()
    .with_payload_predicate("served_by_profile_id", PropertyOp::Eq(profile_id))
```

Index: a partial index on `json_extract(payload, '$.served_by_profile_id')` is added
in the migration that introduces ADR-032 profile orchestration (see ADR-032 §10 SQL).
ADR-041's projection tables do NOT mirror this field — profile-served events are
queried via payload, not via projection columns.

### 7. Sessions — implicit ordering, NOT graph edges

Sessions are derivable from existing event fields, NOT from `followed_by` edges:

```sql
-- All events in a session, in causal order:
SELECT * FROM events
WHERE namespace = ? AND actor = ? AND session_id = ?
ORDER BY created_at ASC, event_id ASC;
```

This needs one additional column on `events`: `session_id: Option<Uuid>`. The emitter
fills it from the caller's `RuntimeContext`. Sessions span events from many verbs and
many packs — `session_id` is a property of the dispatch context, not a substrate
relationship.

Storing this as an edge (`session-[contains]->event`, or `event-[followed_by]->event`)
would create a 1:N edge fanout per session (one edge per event, plus chain edges between
consecutive events) — at 100K events per active week per session, that's millions of
rows of structural edges. The implicit-ordering query above is index-supported and
costs zero edges.

Cross-session causality (did session B reuse memories session A surfaced?) is a JOIN
through `event_observations`, not a graph traversal:

```sql
-- Memories session A observed, then session B selected:
SELECT DISTINCT obs_a.entity_id
FROM event_observations obs_a
JOIN events e_a ON e_a.id = obs_a.event_id
JOIN event_observations obs_b ON obs_b.entity_id = obs_a.entity_id
JOIN events e_b ON e_b.id = obs_b.event_id
WHERE e_a.session_id = :session_a AND obs_a.role = 'candidate'
  AND e_b.session_id = :session_b AND obs_b.role = 'selected'
  AND e_b.created_at > e_a.created_at;
```

### 8. Synthetic GQL/SPARQL edges

The query layer (ADR-008) exposes `event_observations` rows as virtual edges with
relation derived from `role`:

| `role`      | Synthetic relation      | Direction           |
| ----------- | ----------------------- | ------------------- |
| `Candidate` | `observed_as_candidate` | event → entity/note |
| `Selected`  | `observed_as_selected`  | event → entity/note |
| `Target`    | `observed_as_target`    | event → entity/note |
| `Signal`    | `observed_as_signal`    | event → entity/note |

These relations are **synthetic** — they do NOT appear in `khive_types::EdgeRelation`
(ADR-002's closed enum) and do NOT appear in the `edges` table. They are compiled into
the GQL/SPARQL pattern matcher as JOINs against `event_observations`. Calling
`link(source_id=<event_uuid>, target_id=<entity_uuid>, relation="observed_as_candidate")`
returns `InvalidRelation` — synthetic edges are read-only via query.

This keeps ADR-002's closed-set invariant intact while making the projection
queryable as a graph at the GQL/SPARQL layer.

### 9. What this does NOT add

- **No new ADR-002 edge relations.** ADR-002's 15 relations remain unchanged. `observed_*` are synthetic, query-layer only.
- **No event-as-entity promotion.** Events stay in `events`; the projection is its own
  table.
- **No `followed_by` edges.** Session chaining is `(session_id, created_at, event_id)`
  ordering, not stored edges.
- **No new verbs.** `link`/`create` over events stays prohibited (ADR-022 §1). The
  projection is read-only via query and Fold consumers.
- **No payload changes.** Existing event payloads continue to carry the same JSON;
  the projection is a denormalized supplement, not a replacement.

---

## Rationale

### Why a projection table and not just JSON-LIKE queries

JSON-LIKE scans grow linearly with event volume and don't benefit from indexes. At 1M
events and 10 candidates per recall, "which memories were ever surfaced?" is 10M
JSON-payload reads on every query. The projection table gives a B-tree index on
`(entity_id, role)` — O(log N) seek, irrespective of total event count.

The storage cost is bounded: 1M events × 10 observations × ~40 bytes per row = 400MB
worst case for a high-volume deployment. SQLite handles this comfortably. The schema is
denormalized, but the projection is derived from canonical payloads — no consistency
invariant beyond write-time transactionality.

### Why hybrid (log + projection) and not log-only or graph-only

**Log-only** (status quo before this ADR): cursor semantics are pure, Fold determinism
is uncomplicated, but provenance queries require app-side JSON parsing on every read.
Brain LoRA-class profiles' "weight memories that consistently get positive feedback"
needs to JOIN observations to feedback events — without the projection, that's an
app-side scan.

**Graph-only** (events promoted into entities): wins on graph-query expressiveness,
loses cursor-based replay (entities don't have monotonic ordering), loses
PackEventConsumer atomicity (the projection now spans the entity-substrate edge
table). And ADR-002's closed relation set forbids adding `observed`/`selected`
relations without an amendment — a substrate-level change for a denormalization
concern is the wrong layer.

**Hybrid**: log stays canonical (ordering, replay, cursor — all preserved). Projection
is derived, denormalized, indexable. Synthetic edges expose it to GQL/SPARQL without
touching ADR-002. Each layer carries the semantics it's best at.

### Why write-time projection (not read-time)

Read-time projection (compute observations on the fly when queried) keeps storage small
but pays the JSON-decode cost on every query. Write-time pays it once per event and
indexes the result. The Fold consumer rate is much lower than the event emit rate
(many consumers query the same event repeatedly across replay/backtest/analytics) —
amortize the decode cost at the write.

The cost: emitters must register a payload decoder per event kind. The decoder is
~10 lines of `serde_json` extraction per kind; total registry is bounded by the event
kind enum (~10 kinds today, ~30 projected).

### Why closed role enum

Roles are closed (Candidate / Selected / Target / Signal) for the same reason ADR-002
relations are closed: open enumerations fragment the query surface. A pack defining
its own role string `seen_by` and another pack defining `viewed_by` would split queries
unnecessarily — both are observation events that should aggregate. The four roles
cover every emit pattern in the v1 pack set; opening the set is deferred to when a
real pattern doesn't fit.

### Why polymorphic referent (one column with referent_kind)

The alternatives were (a) two columns (`entity_id`, `note_id`) with one always NULL, or
(b) two tables (`event_entity_obs`, `event_note_obs`). (a) splits the index by which
column is non-NULL; (b) doubles the JOIN code path in Fold consumers. The polymorphic
column with a discriminator keeps the index dense and the consumer code single-path.

### Why session_id on events, not a Session entity

A `Session` entity in the `entities` table would need its own kind (ADR-001
amendment), its own edges to events (ADR-002 amendment for `session-[contains]-event`),
and its own lifecycle. Sessions don't fit the entity model — they're dispatch-context
identifiers, not researchable concepts. A column on `events` matches their nature
(metadata, not substrate) and avoids two ADR amendments.

If sessions ever become research subjects ("which sessions were most productive?"
with answers that involve traversing session graphs), promoting them to entities is a
follow-up ADR. Today they're context, not content.

---

## Alternatives Considered

| Alternative                                                            | Why rejected                                                                                                                   |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| JSON-LIKE scans on payload references (status quo)                     | O(N) per query; no index; doesn't scale beyond hobby corpus.                                                                   |
| Promote events into the entity substrate; use ADR-002 edges            | Breaks cursor ordering, PackEventConsumer atomicity, append-only contract. Substrate-layer change for denormalization concern. |
| Add new ADR-002 relations (`observed`, `selected`, `target`, `signal`) | Polymorphic source (entity vs event) breaks `edges.source_id REFERENCES entities(id)`. Closed-relation invariant compromised.  |
| Read-time projection (compute on query)                                | Amortizes wrong — many consumers query same event repeatedly; pay decode cost N times instead of once.                         |
| Two-table polymorphism (`event_entity_obs`, `event_note_obs`)          | Doubles JOIN paths in Fold consumers and EventFilter; index split by substrate.                                                |
| `followed_by` event-to-event edges for session chaining                | Millions of rows for session reconstruction that's an indexed range scan. Wrong substrate for transient ordering.              |
| `Session` as an entity kind                                            | Requires ADR-001 + ADR-002 amendments; sessions are dispatch context, not researchable subjects.                               |
| Open role string (pack-extensible roles)                               | Query fragmentation — packs invent synonyms that don't aggregate.                                                              |
| Open referent_kind (any substrate including future)                    | Polymorphism cost grows with substrates; v1's two (entity, note) cover every emit pattern.                                     |
| Async projection (event row first, observations later)                 | Breaks PackEventConsumer atomicity — a Fold replay between event write and projection write sees incomplete data.              |

---

## Consequences

### Positive

- Provenance becomes indexable: O(log N) seek on `(entity_id, role)` instead of O(N)
  JSON-LIKE scans.
- Fold consumers receive typed `EventView` with pre-decoded observations — payload
  schema-on-read coupling eliminated.
- `EventFilter::observed` / `selected` push provenance predicates into the WHERE
  clause; cold profiles with graph-proximity filters wake only for relevant events.
- GQL/SPARQL pattern matchers can traverse event→entity provenance natively via
  synthetic edges, without touching ADR-002's closed relation set.
- Session reconstruction is an indexed range scan, not a graph traversal.
- ADR-022, ADR-017, ADR-024, ADR-002 semantics are preserved verbatim — this ADR is
  additive.

### Negative

- Schema-on-write coupling: adding a new event kind that should project requires
  registering a decoder and declaring its role mapping.
- Storage doubles for high-fanout event kinds (1 event row + ~10 observation rows for
  recall). Bounded; SQLite handles 100M-row scale.
- Cursor + state atomicity guarantee (ADR-017) now spans three tables — `events`,
  `event_observations`, and the pack's state — all in one transaction. Manageable
  with SQLite's single-writer model; concurrent writers across multiple connections
  serialize naturally.
- `EventView.event` is a plain public field (explicit access). Future maintainers must
  remember observations are NOT in `Event` itself, only in `EventView.observations`.

### Neutral

- The projection is opt-in per event kind. Existing kinds with no decoder produce no
  rows — backward-compatible.
- `session_id` is `Option<Uuid>` — events without a session (e.g., system-initiated
  audits) carry `NULL`.
- Synthetic edges are read-only via query; `link(... relation="observed_as_*")`
  rejects with `InvalidRelation`.

---

## Implementation

### Schema migration

The shipped ADR-041 DDL is Migration V13 (`event_observability_provenance`). V8 is the
reserved ADR-041 placeholder (`reserved_adr041_event_observations_and_session_id`) and is
a no-op retained for migration-ledger contiguity.

```rust
VersionedMigration {
    version: 13,
    name: "event_observability_provenance",
    up: V13_EVENT_OBSERVABILITY_PROVENANCE,
}
```

The generated V13 SQL adds `events.session_id TEXT`, creates `event_observations` with
`event_id TEXT` and `entity_id TEXT`, and creates `idx_events_session`,
`idx_event_obs_entity`, and `idx_event_obs_event_role`.

### Runtime additions

- `crates/khive-storage/src/event.rs`: owns `EventFilter`, `EventView`,
  `EventObservation`, `ObservationRole`, and `ReferentKind`.
- `crates/khive-db/src/stores/event.rs`: owns same-transaction
  `insert_event_with_observations`, per-kind observation decoding, and SQL filtering.
- `crates/khive-query/src/compilers/sql.rs`: owns synthetic `observed_as_*` GQL lowering.
- `crates/khive-query/src/validate.rs`: skips closed-edge validation for synthetic
  `observed_as_*` relation names.

### EventFilter extensions

- `crates/khive-storage/src/event.rs`: `EventFilter` includes `observed`, `selected`,
  `session_id`, and additive `payload_proposal_id`; DB lowering uses `EXISTS` subqueries
  over `event_observations`.

### Per-kind decoder examples

- `RecallExecuted` / `SearchExecuted` / `RerankExecuted`: `payload.candidates` →
  `Candidate` note rows; `payload.selected` / `payload.reranked` / `payload.final_scores`
  → `Selected` note rows.
- `LinkCreated`: `payload.source_id` and `payload.target_id` → two entity `Target`
  rows at `position=0` and `position=1`.
- `FeedbackExplicit`: `event.target_id` → entity **or note** `Signal` row, per
  `event.substrate` (Amendment A1, A2).

### PackEventConsumer dispatch update

No shipped `crates/khive-runtime/src/events.rs`, `observations.rs`, or `event_view.rs`
files own this behavior. Consumers that need event observations read the shipped
`EventView` shape from `khive-storage`.

### Tests

| Scenario                                           | Assert                                                                                                                             |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `recall` emits event                               | `event_observations` rows exist for candidates + selected with correct positions                                                   |
| `link` emits event                                 | Two `Target` rows at positions 0 and 1                                                                                             |
| EventFilter `observed=[mem_id]`                    | Returns only events that observed `mem_id`; one JOIN in the EXPLAIN plan                                                           |
| EventView in Fold                                  | `view.observations` is non-empty for projecting kinds; payload accessible via `view.event.payload` (NOT `view.payload` — no Deref) |
| Synthetic edge in GQL                              | `MATCH (e:event)-[:observed_as_selected]->(m:memory) RETURN m` returns the selected memories                                       |
| `link(event_id, entity_id, observed_as_candidate)` | Returns `InvalidRelation` — synthetic edges are read-only                                                                          |
| Session reconstruction                             | `session_id = :sid ORDER BY (created_at, id)` returns events in causal order                                                       |
| Cross-session reuse JOIN                           | The session-A→session-B JOIN returns correct memory ids                                                                            |

---

## Open Questions

1. **Observation TTL**. If event retention is time-tiered (ADR-032 §Open Questions §1
   on compaction), do `event_observations` rows survive the parent event's compaction?
   Tentative: drop with the parent event (ON DELETE CASCADE via explicit cleanup pass),
   since the observation is semantically pinned to the event it derives from.

2. **`Target` vs `Candidate` for non-recall kinds**. A `LinkCreated` event has two
   `Target` rows (source and target endpoint of the new edge). Should they instead
   be distinguished as `LinkSource` and `LinkTarget`? Today the `position` field
   carries the distinction (0=source, 1=target). Promoting to roles is reserved for
   when a real query needs the distinction without inspecting position.

3. **Observation aggregation views**. Should the projection layer expose pre-aggregated
   views (e.g., a materialized "memories that received N positive feedbacks" view)?
   Deferred — the SQL JOIN over `event_observations` + `events.kind` covers it without
   a view.

4. **Namespace isolation**. The `event_observations` table inherits namespace
   isolation via the JOIN to `events.namespace`. No `namespace` column on the
   projection itself; correct by construction but worth noting in the implementation
   comment.

---

## Amendment A1: `FeedbackExplicit` signal decodes from `target_id`, not `payload.about_id` (2026-07-10, khive#811)

§3's per-verb role mapping and §"Implementation"'s decoder examples originally specified
`FeedbackExplicit: payload.about_id → entity Signal row`. No emitter ever wrote a payload
`about_id` field — `brain.feedback` (`crates/khive-pack-brain/src/handlers.rs`) sets the
feedback subject via `Event::with_target(target)`, i.e. `event.target_id`, matching every
other target-carrying event kind (`EntityUpdated`, `NoteUpdated`, `TaskTransitioned`, …).
`decode_signal_observation` (`crates/khive-db/src/stores/event.rs`) read the nonexistent
payload field instead, so every `FeedbackExplicit` event projected zero `Signal` rows —
the projection was silently empty for the entire lifetime of this ADR's implementation.

This is a decoder bug, not an emitter bug: `event.target_id` is the correct, already-shipped
carrier for "the entity/note this event is about" across every other decoded event kind, and
changing the emitter to duplicate that value into `payload.about_id` would introduce a
redundant field with no consumer. The fix makes `decode_signal_observation` read
`event.target_id`, consistent with `decode_target_observation`.

The role mapping in §3 (`FeedbackExplicit` → `Signal`) and its intent are unchanged — only
the field the decoder reads to populate it.

---

## Amendment A2: `FeedbackExplicit` `Signal` rows admit entity or note referents (2026-07-10, khive#831)

§3 and Amendment A1 described the `FeedbackExplicit` → `Signal` projection as entity-only.
`brain.feedback` targets can resolve to either an entity or a note, but the emitted event
carried a fixed `SubstrateKind::Event` placeholder, so `decode_signal_observation`
hard-coded `ReferentKind::Entity` and `observed_as_signal` (`crates/khive-query/src/compilers/sql.rs`)
only admitted entity referents — a note-typed feedback target could never be resolved
through `observed_as_signal`.

The fix threads the resolved target's actual substrate (`Entity`/`Note`) onto the emitted
event, `decode_signal_observation` picks `ReferentKind` from `event.substrate` (falling
back to `Entity` for pre-fix historical events still carrying the `SubstrateKind::Event`
placeholder), and `observed_as_signal` admits both entity and note referents.

## Amendment A3: `SearchExecuted` admits a typed `result_kind` (2026-07-14, khive#806)

§3's per-verb role mapping and the per-kind decoder examples describe `SearchExecuted`
`Candidate`/`Selected` projections as note rows only. Entity searches (`search(kind="entity")`)
also serve results and must be observable with correct typing: stamping entity UUIDs as
`ReferentKind::Note` would durably misclassify references in the append-only projection.

This amendment makes the `SearchExecuted` referent typing polymorphic:

**Payload key.** `SearchExecuted` payloads gain a `result_kind` string with exactly two
accepted values, `"entity"` and `"note"`, describing the substrate of every UUID in that
payload's `candidates` and `selected` lists. A single `SearchExecuted` event never mixes
substrates; the emitting handler sets `result_kind` from the search's resolved kind.

**Projection rule.** The decoder stamps both `Candidate` and `Selected` rows with
`ReferentKind::Entity` when `result_kind` is `"entity"` and `ReferentKind::Note` when it is
`"note"`. Any other present value is a decode error for that event (consistent with the
closed-value posture elsewhere in this ADR); the error is surfaced, not silently coerced.

**Legacy payloads.** A `SearchExecuted` payload with no `result_kind` key is the documented
historical note shape: the decoder projects note rows exactly as before this amendment.
Missing `result_kind` is never a decode error and requires no payload migration — the
event log is append-only and historical events remain valid as written.

**Query surface.** Synthetic `observed_as_*` traversal and `EventFilter::observed`/`selected`
matching follow the projected `referent_kind`; entity-search observations resolve to entity
referents. `RecallExecuted` and `RerankExecuted` are unchanged by this amendment: their
`Candidate`/`Selected` projections remain note rows (memory recall serves memory notes).

---

## References

- [ADR-002](ADR-002-edge-ontology.md): Edge ontology — closed relation set this ADR
  does NOT extend; synthetic edges are query-layer only.
- [ADR-004](ADR-004-substrate-observables.md): Event substrate — unchanged by this
  ADR.
- [ADR-008](ADR-008-query-layer-separation.md): Query layer — host for synthetic-edge
  pattern compilation.
- [ADR-017](ADR-017-pack-standard.md): PackEventConsumer — `on_event` signature gains
  `EventView`.
- [ADR-022](ADR-022-events-query-surface.md): Events Query Surface — `EventFilter`
  gains `observed` / `selected` fields with explicit SQL lowering.
- [ADR-024](ADR-024-fold-cognitive-primitives.md): Fold — consumers receive
  `EventView`, not raw `Event`.
- [ADR-032](ADR-032-brain-profile-orchestration.md): Brain — LoRA-class profiles use
  `EventFilter::observed` to wake on graph-proximity.
- [ADR-034](ADR-034-kg-validation-pipelines.md): KG validation — streaming rules
  over `ValidationItem::Event` receive `EventView`s.
- `crates/khive-runtime/src/events.rs`: emit path + per-kind decoders.
- `crates/khive-storage/src/event.rs`: `EventFilter` extension.
