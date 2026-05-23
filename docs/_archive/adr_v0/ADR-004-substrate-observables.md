# ADR-004: Substrate Model — Three Observables (Note, Entity, Event)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A knowledge graph platform needs a small set of primitive data shapes from which everything else is
composed. Without explicit primitives, every service invents its own row format and the system
fragments into incompatible schemas.

The challenge: pick the right primitives. Too few (e.g., "just blobs and edges") and every service
reinvents structure. Too many and the system becomes a typology zoo where agents don't know which to
use.

We need primitives that:

1. Cover the full range of research KG operations (knowledge, dialog, audit).
2. Are agent-comprehensible — clear semantics for which to use when.
3. Map cleanly to SQL tables for storage efficiency.
4. Support both immutable (event log) and mutable (notes, entities) data.

## Decision

**Three substrate observables, each backed by its own SQL table:**

| Substrate  | Mutability              | What it represents                   | Examples                                           |
| ---------- | ----------------------- | ------------------------------------ | -------------------------------------------------- |
| **Note**   | Mutable + soft-delete   | Temporal-referential records         | Observations, insights, questions, decisions, refs |
| **Entity** | Mutable + soft-delete   | Graph nodes (typed, with properties) | Concepts, documents, projects, people              |
| **Event**  | Immutable (append-only) | Operation log                        | Verb invocations, audit trail                      |

Defined in `khive-types`:

```rust
pub enum SubstrateKind {
    Note = 0,
    Entity = 1,
    Event = 2,
}
pub const SUBSTRATE_COUNT: usize = 3;
```

## Rationale

### Why these three?

Each observable answers a distinct epistemological question:

- **Note** → "What did the agent observe or conclude at time T?" (temporal state)
- **Entity** → "What things exist in the world and how do they relate?" (graph state)
- **Event** → "What happened?" (history state)

Removing any of these forces conflation. Storing decisions as entities loses temporal semantics.
Storing observations as events loses mutability.

### Why Note as a polymorphic type?

The Note table holds observations, insights, questions, decisions, and references — discriminated by
`NoteKind` (see ADR-019). These all share:

- Temporal nature (created_at, decay_factor, salience, expires_at)
- Soft-delete support
- Content as a polymorphic JSON blob plus a free-form `properties` map

Splitting them into separate tables would duplicate schema for marginal benefit. The polymorphism is
honest — these ARE the same kind of thing (temporal records about the world).

### Why Entity separate from Note?

Entities and notes share a `Header` (id, namespace, timestamps) but differ fundamentally:

- Entities have **a type** (concept/document/dataset/...) and **edges to other entities**. The graph
  structure is the point.
- Notes have **a kind** (observation/insight/decision/...) and **content**. The temporal record is
  the point.

A research paper might exist as both: an Entity (Document kind, with edges to concepts) AND have a
Note (observation captured while reading it). They're not the same thing.

### Why Event immutable?

Events are the audit trail. If they were mutable, they'd be useless for:

- Reconstructing what an agent did
- Detecting compromise
- Replaying state derivation

The cost of "I can't fix a typo in an event" is far lower than the value of "events are trustworthy
by construction."

### Why exactly 3 (not 4+)?

We considered separate substrates for:

- **Document** (papers, citations) → folded into Entity with `EntityKind::Document`
- **Relation/Edge** → stored in the entity layer's `graph_edges` table, not a primary substrate
  (edges have no namespace-level existence apart from the entities they connect)
- **Message** (inter-agent communication) → out of scope for the open-source substrate; if a
  deployment needs messaging it can layer on top of `notes` with a `kind="observation"` and
  sender/recipient properties, or live in a separate service.

Each fold was deliberate: the primitives stay at 3, and the discriminators (`EntityKind`,
`NoteKind`) handle the variation.

## Alternatives Considered

| Alternative                                                    | Pros                       | Cons                                                           | Why rejected            |
| -------------------------------------------------------------- | -------------------------- | -------------------------------------------------------------- | ----------------------- |
| Just "blobs + edges" (RDF-style)                               | Maximum flexibility        | Every service reinvents structure                              | Loses too much semantic |
| Separate tables per kind (memories, tasks, journals, ...)      | Type-safe SQL              | Schema explosion, joins everywhere                             | Cost > benefit          |
| Adding "Atom" as a 5th substrate (immutable content-addressed) | Captures provenance neatly | Atoms are an internal detail, not a primary observable for OSS | Defer; can add later    |
| Merging Note and Event                                         | Fewer tables               | Loses immutability guarantee for Event                         | Auditability matters    |

## Consequences

### Positive

- Three trait families (`NoteStore`, `EntityStore` via `GraphStore`, `EventStore`).
- Agents know exactly which substrate to use — clear semantics per primitive.
- Storage is efficient — no synthetic union tables.
- Adding new "kinds" (NoteKind, EntityKind) is a schema-free change.

### Negative

- Cross-substrate queries (e.g., "show me the note about entity X") require join logic at the
  service layer. Mitigated: the entity service handles this; storage stays primitive.

### Neutral

- The discriminator enums (`NoteKind`, `EntityKind`, `EventOutcome`) are part of the public API and
  require ADRs to extend. This is appropriate friction.

## Implementation

In `khive-types`:

```
crates/khive-types/src/
├── note.rs       // Note, NoteKind, NoteStatus
├── entity.rs     // Entity, EntityKind, Link, PropertyValue
├── event.rs      // Event, EventOutcome, EventBuilder
└── substrate.rs  // SubstrateKind enum + dispatch helpers
```

In `khive-storage`:

```
crates/khive-storage/src/
├── note.rs       // NoteStore trait + storage-level Note
├── graph.rs      // GraphStore trait (Entity edges)
└── event.rs      // EventStore trait + storage-level Event
```

In `khive-db`:

- Substrates back onto four SQL tables: `entities` and `graph_edges` (Entity substrate), `notes`,
  and `events`.
- Schema is applied via versioned migrations (see ADR-022).
- Namespace is a caller-supplied parameter at the storage layer; enforcement happens at the
  service/runtime layer (see ADR-007).

## References

- ADR-001: Entity Kind Taxonomy
- ADR-002: Edge Ontology
- `crates/khive-types/src/substrate.rs`: SubstrateKind enum
- `crates/khive-storage/`: trait definitions
