# ADR-004: Substrate Observables

**Status**: accepted\
**Date**: 2026-05-22\
**Authors**: khive maintainers

## Context

khive organizes persistent records into substrates: categories of data with distinct
lifecycle, mutability, and query semantics. The original ADR-004 defined three substrates
(Note, Entity, Event) with a closed `SubstrateKind` enum. Since then:

1. **Pack expansion**: pack-defined records use the Note substrate. Kind-specific scoring
   fields sit on every Note regardless of kind, even when they are not meaningful.
2. **Link identity gap**: The `Link` struct has no `namespace`, `created_at`, `updated_at`,
   or `deleted_at`. ADR-002 allows `annotates` targeting edges, and ADR-003 places namespace
   enforcement in the runtime. Both require Link records to be namespace-addressable.
3. **NoteKindSpec**: ADR-001's EntityTypeRegistry pattern (closed base enum, governed
   pack-extensible subtype layer, runtime validation) is the right precedent for note kinds.

## Decision

### Three substrates

```rust
pub enum SubstrateKind {
    Note = 0,
    Entity = 1,
    Event = 2,
}

pub const SUBSTRATE_COUNT: usize = 3;
```

**Note**: mutable, soft-deletable temporal/cognitive records. Single polymorphic table
discriminated by `kind`. Packs extend meaning through `NoteKindSpec`.

**Entity**: mutable, soft-deletable graph nodes. Edges/Links are addressable records
within the Entity substrate's graph layer, not a fourth substrate.

**Event**: immutable, append-only operation audit records.

### Link is addressable, not a substrate

Edge/Link records are not promoted to a fourth substrate. They remain inside the Entity
substrate's graph layer. However, Link records gain namespace identity for validation,
annotation targeting, and cascade behavior.

```rust
pub struct Link {
    pub id: Id128,
    pub namespace: String,
    pub source: Id128,
    pub target: Id128,
    pub relation: EdgeRelation,
    pub properties: BTreeMap<String, PropertyValue>,
    pub weight: f64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
}
```

`Link` and storage `Edge` namespaces are persisted and serialized as validated
strings, matching SQLite `TEXT` and MCP JSON. `Namespace` remains the validation
and authorization-boundary newtype: runtime entry points parse caller-supplied
namespace strings, mint a `NamespaceToken` at the dispatch boundary, and reject
any explicit `LinkSpec.namespace` that does not match the authorized token before
persisting an edge.

This does not create `SubstrateKind::Edge`, an `EdgeStore` trait family, or an independent
agent-facing edge verb surface.

**Promotion trigger**: Edge becomes a fourth substrate only if agents need direct
first-class access to edge records: `get(edge_id)` returning a namespaced Link, edge-local
lifecycle/versioning, edge-owned properties beyond metadata, or pack-owned edge behavior
that cannot be expressed through `link`, `unlink`, and `annotates`. If promoted, the new
variant appends as `Edge = 3`: never inserted at position 2 (Event already occupies it).

**Migration**: `graph_edges` table gains `namespace`, `created_at`, `updated_at`,
`deleted_at` columns in a new versioned migration. Existing rows backfill `namespace`
from the source entity's namespace, `created_at` from the current timestamp, `updated_at`
from `created_at`, and `deleted_at` as NULL.

### Note as polymorphic type

Note remains a single polymorphic table. All note kinds share the same SQL table,
discriminated by `kind`. Packs register kinds and add semantics through `NoteKindSpec`.

Separate tables per kind would fragment search: the `search` verb must be able to answer
"find all notes about RoPE" across kinds without unioning across tables.

### NoteKindSpec

Each pack-added note kind declares a `NoteKindSpec` at registration time. This mirrors
the `EntityTypeRegistry` pattern from ADR-001: closed base enum, governed pack-extensible
subtype layer, runtime validation, boot-time collision checks.

```rust
pub struct NoteKindSpec {
    pub kind: &'static str,
    pub aliases: &'static [&'static str],
    pub required_fields: &'static [&'static str],
    pub lifecycle: NoteLifecycleSpec,
    pub search_profile: SearchProfile,
    pub fields: &'static [KindField],
}

pub struct NoteLifecycleSpec {
    pub field: &'static str,
    pub initial: &'static str,
    pub terminal: &'static [&'static str],
    pub transitions: &'static [(&'static str, &'static str)],
}

pub struct KindField {
    pub name: &'static str,
    pub ty: FieldType,
    pub required: bool,
    pub storage: FieldStorage,
}

pub enum FieldStorage {
    BaseColumn,
    Properties,
}
```

**Phase 1**: Introduce `NoteKindSpec` as a declaration and introspection contract.
Packs may declare lifecycle, search-profile, and field metadata; the runtime collects
those declarations for documentation and future enforcement. Current write-time
note-kind validation is registry/handler based. Lifecycle field routing and
`kind_status` enforcement are deferred to Phase 2 or a future schema ADR.

**Phase 2**: Migrate kind-specific base Note fields (`salience`, `decay_factor`,
`expires_at`) into declared fields. This is a separate schema migration ADR.

### Base Note shape (interim)

Kind-specific scoring fields such as `salience` and `decay_factor` are `Option<f64>`.
Kinds that do not use them store `None`.

```rust
pub struct Note {
    pub header: Header,
    pub kind: String,
    pub status: NoteStatus,
    pub content: String,
    pub properties: BTreeMap<String, PropertyValue>,
    pub tags: Vec<String>,
    pub salience: Option<f64>,
    pub decay_factor: Option<f64>,
    pub expires_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}
```

**Long-term target**: `salience`, `decay_factor`, `expires_at`, and future kind-specific
fields are declared by `NoteKindSpec` rather than hardcoded on the base Note struct. The
full migration is deferred to a schema ADR.

### NoteStatus and kind lifecycle

`NoteStatus` is the universal visibility signal: is this note live in ordinary namespace
queries?

```rust
pub enum NoteStatus {
    Active,
    Archived,
}
```

Kind lifecycle is separate. Each pack declares its own lifecycle via `NoteKindSpec`. The
lifecycle state is stored in a kind-declared field (e.g., `kind_status`), not in
`NoteStatus` and not in `properties["status"]`.

Using a distinct lifecycle field such as `kind_status` avoids collision with
`Note.status` and keeps substrate visibility separate from kind-specific state.

### Event ordering invariant: weakly monotonic timestamps + UUID tiebreaker

Event `created_at` is **weakly monotonic**. Equal timestamps within the same microsecond
are legal: strict monotonicity would require coordination across threads, processes,
clocks, and storage backends that the substrate does not promise.

The canonical total order on events is:

```text
(created_at ASC, event_id ASC)   for replay / catch-up
(created_at DESC, event_id DESC) for newest-first listing
```

`event_id` is compared by canonical UUID byte order. It does not encode true insertion
order; it provides a deterministic tiebreaker when `created_at` ties. Replay consumers
that need lossless catch-up persist a compound cursor:

```rust
pub struct EventCursor {
    pub created_at: Timestamp,
    pub event_id:   Uuid,
}
```

This invariant is consumed by:

- ADR-022 §3b: list/replay query shape and the composite index `idx_events_ns_created_id`.
- ADR-017: `PackEventConsumer` cursor persistence (state + cursor must be atomic).
- ADR-024: `Fold<Event, State>` deterministic replay: same canonical-order
  events + same `FoldContext` ⇒ same final state.

A monotonic append-sequence counter (`event_seq`) is NOT in v1. The timestamp+id pair
gives deterministic replay order; a true commit-order counter is a different invariant
(causal happened-before) and not required.

### Event substrate observable fields

The substrate defines the abstract Event; its concrete column set on the storage
side is the union of the fields added across ADRs that touch the events table.
The canonical v1 set:

| Field        | Type            | Added by   | Purpose                                         |
| ------------ | --------------- | ---------- | ----------------------------------------------- |
| `id`         | `Uuid`          | ADR-004    | Event identity (UUIDv7)                         |
| `namespace`  | `String`        | ADR-004    | Record attribution and namespace scope          |
| `verb`       | `String`        | ADR-004    | The verb that produced the event                |
| `actor`      | `String`        | ADR-004    | Caller identity (agent, user, system)           |
| `substrate`  | `SubstrateKind` | ADR-004    | The substrate the verb acted on                 |
| `payload`    | `JSON`          | ADR-004    | Verb-specific JSON; structure per ADR-022 §3a   |
| `created_at` | `Timestamp`     | ADR-004    | Weakly-monotonic event time                     |
| `session_id` | `Option<Uuid>`  | ADR-041 §7 | Optional session grouping for implicit ordering |

### Event vs Note boundary

Decision rule:

```text
If the record is an append-only audit fact about a system operation, it is an Event.

If the record is a semantic record that may be searched, revised, archived, or used in
scoring, it is a Note.

The operation that creates or updates a Note may still emit an Event, but the Event
records the operation, not the domain payload.
```

| Record                           | Substrate | Why                         |
| -------------------------------- | --------- | --------------------------- |
| `create(entity)` was invoked     | Event     | Append-only operation audit |
| `link(a, relation, b)` succeeded | Event     | Append-only operation audit |

## Rationale

### Why three substrates (not four)?

Edge/Link records have different lifecycle semantics than entities: they are relational,
not independently identifiable to agents. Adding a fourth substrate means a new store
trait family, new verb surface, new VCS snapshot dimension, and new substrate-kind dispatch
in the coordinator. The benefit (direct `get(edge_id)` from agents) is not a current
requirement. Adding namespace identity to Link fixes the practical gap without the
substrate-level complexity.

### Why not collapse Event into Note?

Events must be trustworthy by construction. The append-only, immutable guarantee is
compile-time: Event has no `update` path, no `soft_delete`, no `status` field. Making
events a "special immutable note kind" loses this structural guarantee: the
enforcement becomes runtime convention rather than type-system property.

### Why polymorphic Note (not per-kind tables)?

Fragmenting notes into per-kind tables breaks unified search. The `search` verb would need to union across N
tables whose count grows with each new pack. A single polymorphic table with `kind`
discrimination preserves the unified index while allowing kind-specific semantics
through `NoteKindSpec`.

### Why NoteKindSpec mirrors EntityTypeRegistry?

Both solve the same problem: closed base enum + governed pack-extensible subtype layer.
ADR-001's EntityTypeRegistry validates entity_type at write time in the runtime layer.
NoteKindSpec mirrors that shape for note kinds as a declaration/introspection contract;
write-time lifecycle enforcement is deferred (see Phase 1). Using the same pattern means
pack authors learn one extension mechanism and the runtime has one declaration shape to
collect today and one validation shape to enforce in a future phase.

### Why separate NoteStatus from kind lifecycle?

`NoteStatus` answers: "Is this note visible/live in ordinary namespace queries?"
Kind lifecycle answers: "What domain state is this note in?"

These are different questions with different consumers. A record in a terminal
kind-specific state may still be `NoteStatus::Active` because it should appear in search
results and remain linkable. An archived note (`NoteStatus::Archived`) should not appear
in ordinary queries regardless of its kind lifecycle state.

## Alternatives Considered

| Alternative                                | Why rejected                                                                                                                                      |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Edge as fourth substrate                   | No concrete requirement for agent-addressable edges. Complexity cost (new store trait, verb surface, VCS dimension) not justified.                |
| Collapse Event into Note                   | Loses compile-time immutability guarantee. Append-only enforcement becomes runtime convention.                                                    |
| Per-kind Note tables                       | Fragments unified search. Union across N tables grows with each pack.                                                                             |
| NoteKindSpec as optional                   | Current `salience` pollution on decision notes is the direct consequence. Without a spec, every new kind-specific field lands on the base struct. |
| salience/decay_factor removal now          | Wide breaking change across all crates. `Option<f64>` is semantically honest without the migration cost.                                          |
| NoteStatus expanded to include kind states | NoteStatus becomes a runtime-validated string instead of a compile-time enum. Type regression.                                                    |

## Consequences

### Positive

- Substrate model stays simple: three substrates, no renumbering.
- Link records become namespace-addressable, fixing the annotation and validation gap.
- NoteKindSpec gives pack authors a governed extension mechanism for note kinds.
- Event vs Note boundary has a crisp, testable decision rule.
- `Option<f64>` for salience/decay makes the semantics honest without a wide migration.

### Negative

- Link struct change is a breaking change in khive-types + schema migration.
  Mitigated: backfill strategy is deterministic (namespace from source entity).
- NoteKindSpec is a new trait surface that must be stable.
  Mitigated: mirrors the proven EntityTypeRegistry pattern.
- `kind_status` requires pack-defined lifecycle data to migrate from generic properties.
  Mitigated: the migration is deterministic and eliminates a semantic collision.
- Deferred field migration means `salience: Option<f64>` is transitional: the base
  Note struct still carries kind-specific fields.
  Mitigated: documented as interim, with clear long-term target.

### Neutral

- `SubstrateKind` enum values unchanged. No serialization migration.
- Event substrate scope unchanged (operation audit only).
- MCP wire protocol unchanged: substrate changes are internal.
