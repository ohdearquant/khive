# ADR-041: Event Provenance Projection — Hybrid Log + Graph Edges

**Status**: Accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Depends on**: [ADR-002](./ADR-002-edge-ontology.md),
[ADR-004](./ADR-004-substrate-observables.md),
[ADR-008](./ADR-008-query-layer-separation.md),
[ADR-017](./ADR-017-pack-standard.md),
[ADR-022](./ADR-022-events-query-surface.md)

## Context

The event log is append-only, ordered, and cursor-addressable. Event payloads may contain
identifiers of entities or notes that an operation considered, selected, or changed.
Leaving those identifiers only in JSON forces provenance queries to scan and decode event
payloads.

Promoting events into the entity table would break the event substrate's replay and cursor
contract. Writing ordinary graph edges would also extend the closed edge ontology with
relations that are useful only as projections.

## Decision

Keep the event log canonical and add a sibling relational projection,
`event_observations`. The runtime extracts known references at event-write time and commits
the event and projection rows atomically. The query layer exposes projection rows as
read-only synthetic graph edges.

### 1. Two-layer model

```text
events
  id, created_at, namespace, actor, verb, kind, payload
  append-only; ordered by (created_at, id)
                |
                | same transaction
                v
event_observations
  event_id, entity_id, referent_kind, role, position
  read-only outside event emission
```

The projection can be rebuilt from retained event payloads. It is never the source of truth
for event order or payload meaning.

### 2. Schema

```sql
CREATE TABLE event_observations (
    event_id       TEXT NOT NULL,
    entity_id      TEXT NOT NULL,
    referent_kind  TEXT NOT NULL,
    role           TEXT NOT NULL,
    position       INTEGER NOT NULL,
    PRIMARY KEY (event_id, role, position)
);

CREATE INDEX idx_event_obs_entity
    ON event_observations(entity_id, role);
CREATE INDEX idx_event_obs_event_role
    ON event_observations(event_id, role);
```

`referent_kind` is exactly `entity` or `note`. The identifier column is polymorphic
because the projection spans two substrate tables. The runtime validates the discriminator
and target existence before commit.

`position` preserves deterministic order inside one role. `created_at` and namespace
are not duplicated; they are read by joining the parent event.

### 3. Closed observation roles

```rust
enum ObservationRole {
    Candidate,
    Selected,
    Target,
    Signal,
}
```

- `Candidate`: a referent considered by the operation.
- `Selected`: a candidate chosen for the result.
- `Target`: a record directly changed or acted upon.
- `Signal`: a record attached as a typed signal to the event.

The enum is closed. A new role requires an ADR amendment. Event kinds with no projected
referents write no observation rows.

Public mappings include:

| Event kind                  | Projection                                           |
| --------------------------- | ---------------------------------------------------- |
| `SearchExecuted`            | ordered `Candidate` and `Selected` rows              |
| `RerankExecuted`            | ordered input `Candidate` and output `Selected` rows |
| `LinkCreated`               | source and target as `Target` positions 0 and 1      |
| Entity create/update/delete | acted-on entity as `Target`                          |
| Note create/update/delete   | acted-on note as `Target`                            |

### 4. Atomic write-time projection

Each projecting event kind has one decoder that reads the canonical payload and returns
typed observation rows:

```rust
let rows = decode_observations(&event)?;
tx.insert_event(&event).await?;
for row in rows {
    tx.insert_observation(&event.id, &row).await?;
}
tx.commit().await?;
```

A malformed payload for a registered decoder aborts the entire transaction. Silently
dropping projection rows would make event and projection state disagree.

Adding a projecting event kind requires a decoder, role mapping, and tests. Existing kinds
without a decoder remain valid and project zero rows.

### 5. Event view

Event consumers receive:

```rust
struct EventView {
    event: Event,
    observations: Vec<EventObservation>,
}
```

The event remains fully accessible. The view avoids repeated JSON decoding while preserving
the event as the canonical value. Loading a page of events must load observations in a
bounded query rather than issuing one query per event.

### 6. Event filters

`EventFilter` adds:

| Field        | Type           | Meaning                                           |
| ------------ | -------------- | ------------------------------------------------- |
| `observed`   | `Vec<Uuid>`    | Match `Candidate` rows for any listed identifier. |
| `selected`   | `Vec<Uuid>`    | Match `Selected` rows for any listed identifier.  |
| `session_id` | `Option<Uuid>` | Match the event correlation field exactly.        |

Non-empty fields compose with the existing filter by AND. Observation filters lower to an
indexed `EXISTS` subquery. A backend whose schema predates the projection returns a typed
capability error rather than leaking a raw “no such table” error.

### 7. Search result typing

`SearchExecuted` payloads may contain a `result_kind` value of `entity` or `note`.
The decoder applies that discriminator to all candidate and selected identifiers in the
event. A single search event cannot mix substrates.

An unknown present value is a decode error. A historical payload with no
`result_kind` uses the documented legacy note shape and remains readable without payload
migration.

### 8. Correlation ordering

Events sharing a `session_id` are ordered by `(created_at, id)`. Correlation is an
event-dispatch property, not an entity kind or stored graph edge. No
`followed_by` relation is added.

### 9. Synthetic query edges

The query layer maps roles to virtual relations:

| Role        | Synthetic relation      |
| ----------- | ----------------------- |
| `Candidate` | `observed_as_candidate` |
| `Selected`  | `observed_as_selected`  |
| `Target`    | `observed_as_target`    |
| `Signal`    | `observed_as_signal`    |

These names are query-compiler syntax only. They do not appear in
`EdgeRelation` or the `edges` table. Calling `link` with one of them returns
`InvalidRelation`.

### 10. Deletion and retention

Projection rows have the same retention lifetime as their parent event. Deleting an event
through an authorized retention operation deletes its observation rows. Orphan projection
rows are invalid and are removed by integrity repair.

## Invariants

- Event and observations commit or roll back together.
- The event log remains the cursor and replay authority.
- Projection roles and referent kinds are closed values.
- Synthetic relations are read-only and never enter the edge ontology.
- Observation queries inherit namespace from the parent event.
- Position is unique within `(event_id, role)`.
- Event readers avoid N+1 observation loading.

## Verification

Tests must cover:

- search projection for entity and note results;
- legacy search payload typing;
- link endpoints at positions 0 and 1;
- target projection for entity and note mutations;
- malformed decoder payload rollback;
- atomic event/projection failure;
- indexed `observed` and `selected` filters;
- bounded page loading into `EventView`;
- synthetic-edge query lowering;
- `link` rejection for synthetic relations;
- correlation ordering by `(created_at, id)`; and
- parent-event retention removing projection rows.

## Alternatives considered

| Alternative                                    | Reason rejected                                                        |
| ---------------------------------------------- | ---------------------------------------------------------------------- |
| Query JSON payloads at read time               | Requires repeated decoding and poorly indexed scans.                   |
| Promote events to entities                     | Breaks append-only event cursor semantics.                             |
| Store ordinary graph edges                     | Extends the closed ontology and creates polymorphic endpoint problems. |
| Compute the projection only at read time       | Repeats decoder work and permits historical decoder drift.             |
| Add a separate identifier column per substrate | Splits indexes and adds nullable-column invariants.                    |

## Consequences

### Positive

- Provenance filters become indexed relational queries.
- Graph queries can traverse event references without changing the edge ontology.
- Event consumers receive typed references without payload-specific decoding.

### Negative

- Event emission must maintain a second table atomically.
- Payload-schema changes for projecting kinds require decoder compatibility.
- Projection rows add storage proportional to referenced records.

## References

- [ADR-002](./ADR-002-edge-ontology.md): closed persistent relations
- [ADR-004](./ADR-004-substrate-observables.md): event substrate
- [ADR-008](./ADR-008-query-layer-separation.md): synthetic-edge compilation
- [ADR-017](./ADR-017-pack-standard.md): event consumers
- [ADR-022](./ADR-022-events-query-surface.md): event filtering and ordering
- [ADR-034](./ADR-034-kg-validation-pipelines.md): validation consumers
- [ADR-042](./ADR-042-local-rerank-via-lattice-inference.md): rerank projection
- [ADR-055](./ADR-055-epistemic-edge-relations.md): current persistent edge ontology
