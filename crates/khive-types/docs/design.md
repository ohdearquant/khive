# khive-types Design

Core primitives and substrate data types for khive. `#![no_std]` compatible
with minimal dependencies. No ID generation, no clock access, no panics.

## Scope

This crate defines the shared data shapes (Entity, Note, Event, Link) and
closed taxonomies (EntityKind, EdgeRelation, NoteStatus, SubstrateKind) that
every other khive crate depends on. It also carries the `Pack` trait for
declarative pack metadata, the unified `KhiveError` model, and supporting
types for proposals, events, and namespace isolation.

## Relevant ADRs

- [ADR-001: Entity Kind Taxonomy](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)
- [ADR-002: Closed Edge Ontology](../../../docs/adr/ADR-002-edge-ontology.md)
- [ADR-004: Substrate Observables](../../../docs/adr/ADR-004-substrate-observables.md)
- [ADR-013: Note Kind Taxonomy](../../../docs/adr/ADR-013-note-kind-taxonomy.md)
- [ADR-017: Pack Standard](../../../docs/adr/ADR-017-pack-standard.md)
- [ADR-019: GTD Pack](../../../docs/adr/ADR-019-gtd-pack.md)
- [ADR-021: Memory Pack](../../../docs/adr/ADR-021-memory-pack.md)
- [ADR-023: Pack Verb Surface, Visibility, and Composition](../../../docs/adr/ADR-023-declarative-pack-format.md)
- [ADR-025: Verb Surface as Speech-Act Taxonomy](../../../docs/adr/ADR-025-verb-speech-acts.md)
- [ADR-034: KG Validation Pipelines](../../../docs/adr/ADR-034-kg-validation-pipelines.md)
- [ADR-045: Verb Response Presentation Modes](../../../docs/adr/ADR-045-verb-response-presentation.md)
- [ADR-046: Event-Sourced Agent KG Proposals](../../../docs/adr/ADR-046-event-sourced-proposals.md)

## Primary Modules

| Module | Path | Purpose |
|--------|------|---------|
| `entity` | [src/entity.rs](../src/entity.rs) | Entity, EntityKind (8 closed kinds), Link, PropertyValue |
| `edge` | [src/edge.rs](../src/edge.rs) | EdgeRelation (15 closed relations), EdgeCategory |
| `note` | [src/note.rs](../src/note.rs) | Note, NoteStatus |
| `event` | [src/event.rs](../src/event.rs) | Event, EventKind, EventPayload, proposal types |
| `pack` | [src/pack.rs](../src/pack.rs) | Pack trait, HandlerDef, VerbCategory, endpoint rules |
| `substrate` | [src/substrate.rs](../src/substrate.rs) | SubstrateKind (3 substrates) |
| `id` | [src/id.rs](../src/id.rs) | Id128 (128-bit UUID) |
| `namespace` | [src/namespace.rs](../src/namespace.rs) | Namespace (validated string token) |
| `khive_error` | [src/khive_error.rs](../src/khive_error.rs) | KhiveError, ErrorKind, ErrorCode, Details |
| `error` | [src/error.rs](../src/error.rs) | TypeError, UnknownVariant |
| `timestamp` | [src/timestamp.rs](../src/timestamp.rs) | Timestamp (microsecond precision) |
| `header` | [src/header.rs](../src/header.rs) | Header (shared record metadata) |
| `hash` | [src/hash.rs](../src/hash.rs) | Hash32 (256-bit content hash) |
| `vector` | [src/vector.rs](../src/vector.rs) | DistanceMetric |

## Tests

- [tests/khive_error.rs](../tests/khive_error.rs) -- integration tests for KhiveError serde roundtrips
- Inline `#[cfg(test)]` modules in each source file

## Invariants and Failure Modes

- **Closed taxonomies are compile-time enforced.** EntityKind (8 variants),
  EdgeRelation (15 variants), SubstrateKind (3 variants), and NoteStatus are
  closed enums. Unrecognized strings produce `UnknownVariant` errors with the
  valid set listed. Adding variants is a source-breaking change requiring an ADR.
- **Namespace validation rejects invalid input.** Empty, too-long (>256 bytes),
  invalid characters, empty segments, and trailing separators all return
  `NamespaceError`. There is no `From<String>` impl by design.
- **Link.weight documented as [0.0, 1.0].** Enforced via `Link::is_valid()`;
  serde derives do not currently reject out-of-range values at deserialization.
- **Note.salience documented as [0.0, 1.0], decay_factor as non-negative.**
  Enforced via `Note::is_valid()`; serde derives do not currently reject
  out-of-range values at deserialization.
- **Details silently truncates to 8 key-value pairs.** This is intentional to
  bound metadata allocation on error paths.
- **ErrorKind and ErrorDomain are closed taxonomies.** New variants are a
  source-breaking change and require an ADR.
- **`#![forbid(unsafe_code)]`** enforced crate-wide.

## ADR Compliance

### ADR-001: Entity Kind Taxonomy

- `EntityKind` is a closed enum with exactly 8 variants: `concept`, `document`,
  `dataset`, `project`, `person`, `org`, `artifact`, `service`.
- `EntityKind::ALL` enumerates them in taxonomy-table order.
- `FromStr` accepts the 8 canonical names (case-insensitive) plus convenience
  aliases (e.g., `"paper"` -> `Document`, `"repo"` -> `Project`). Aliases
  resolve to the base kind only; the subtype string (`entity_type`) is carried
  separately.
- `Entity.entity_type` holds the pack-governed subtype token; ontology type
  strings must not be stored raw in `properties`.

### Edge Ontology (ADR-002)

- `EdgeRelation` is a closed enum with exactly 17 canonical relations (15 base
  per ADR-002 + 2 epistemic `supports`/`refutes` added by ADR-055).
- `EdgeRelation::ALL` lists them in ontology-table order.
- Wire format is snake_case (e.g., `"part_of"`, `"introduced_by"`).
- `FromStr` accepts canonical snake_case names, hyphen variants, and squashed
  forms (e.g., `"partof"`, `"derivedfrom"`) for ergonomic DSL entry. Squashed
  forms are not stored on the wire.
- `EdgeCategory` groups the 17 relations into 9 structural categories for query
  planners and UI rendering.
- Symmetric relations (`competes_with`, `composed_with`) are identified via
  `is_symmetric()`.

### Substrate Model (ADR-004)

- Three substrates: `Note`, `Entity`, `Event` -- represented by `SubstrateKind`.
- `SUBSTRATE_COUNT` is a compile-time constant (3).
- `Note` carries a pack-owned `kind` string validated by the loaded pack at the
  service boundary.
- `Note.status` (`NoteStatus`) is a cross-cutting lifecycle field distinct from
  pack-specific lifecycle fields (which use `"kind_status"` in `properties` to
  avoid semantic collision).
- `Entity.kind` is the closed `EntityKind` base enum.
- `Event` is append-only and never mutated or deleted.

### ADR-013: Note Kind Taxonomy

- The 5 base note kinds (`observation`, `insight`, `question`, `decision`,
  `reference`) are declared by the kg pack, not hardcoded in `khive-types`.
  This crate only carries the `Note` struct with a free-form `kind: String`
  validated at the pack boundary.

### Pack-Extensible Edge Endpoints (ADR-017)

- `EdgeEndpointRule` declares the types allowed at each end of an edge for a
  specific relation.
- Pack-declared rules are **additive**: they extend the allowed
  `(source, relation, target)` triples beyond the base contract. Packs cannot
  tighten base rules.
- `EndpointKind` distinguishes note-substrate endpoints (`NoteOfKind`) from
  entity-substrate endpoints (`EntityOfKind`).

### ADR-019: GTD Pack

- `PackSchemaPlan` carries idempotent DDL statements a pack needs applied to the
  auxiliary schema. Statements use `CREATE TABLE IF NOT EXISTS`; they are not
  part of the core versioned migration chain.

### ADR-021: Memory Pack

- The `EdgeRelation` enum is the closed set -- not extensible. Only the
  per-relation endpoint contract (via `EdgeEndpointRule`) is extensible by packs.

### Handler Visibility and Discovery (ADR-023)

- `HandlerDef` replaces the deprecated `VerbDef` type alias.
- `Visibility::Verb` entries are surfaced on the MCP wire; `Visibility::Subhandler`
  entries are internal (operator-only).
- The `params` slice on `HandlerDef` enables `help=true` schema introspection.
  Empty (`&[]`) is the correct default for handlers without a fixed parameter
  schema.

### Speech-Act Taxonomy for Verbs (ADR-025)

- `VerbCategory` classifies verbs by illocutionary force: `Assertive`,
  `Directive`, `Commissive`, `Declaration`. `Expressive` is intentionally
  absent -- no verb currently uses it.
- The category is a documentation and introspection tag only. It is NOT used for
  permission checking, transport routing, or return-shape selection.
- Every `Visibility::Verb` handler MUST carry a category.

### Pack Validation Rules (ADR-034)

- `Pack::VALIDATION_RULES` is a declarative catalog of rule identifiers
  contributed by a pack. Rule IDs are namespaced `<pack-name>/<rule-id>`. Actual
  rule implementations live in `khive-runtime`; this const is metadata-only.

### Verb Presentation Policy (ADR-045)

- `VerbPresentationPolicy` controls whether a verb's response can be trimmed by
  agent-mode transforms.
- `AlwaysVerbose` verbs bypass agent-mode transforms entirely. The current set:
  `get`, `link`, `query`, `traverse`, `neighbors`, `brain.feedback`.
- `link` is `AlwaysVerbose` because the returned edge ID is the only handle for
  follow-up graph traversal calls. At scale (~65K edges), two edges can share
  the same 8-character prefix, so shortening the edge ID breaks downstream
  chaining.
- `brain.feedback` is `AlwaysVerbose` because callers chain `target_id` from
  the response into subsequent feedback or profile queries; an 8-char prefix is
  ambiguous.

### Proposal Lifecycle (ADR-046)

- `EventKind` includes `ProposalCreated`, `ProposalReviewed`,
  `ProposalApplied`, `ProposalWithdrawn` for the event-sourced proposal state
  machine.
- `ProposalChangeset` is the typed change payload; `EntityDraft`,
  `ProposalEntityPatch`, `NoteDraft` are structured drafts for adding/modifying
  entities and notes via proposals.
- `EntityDraft.kind` is validated against the closed 8-kind entity taxonomy at
  apply time.
- `ProposalDecision.as_str()` returns the bare variant name for TEXT column
  storage -- callers must NOT use `serde_json::to_string`, which adds JSON
  quoting.

## Consistency Notes

- `NoteLifecycleSpec.field` is documented to use `"kind_status"` for pack-owned
  lifecycle fields to avoid collision with `Note.status` (`NoteStatus`). This is
  a convention enforced by documentation; the runtime does not validate the field
  name string.
- `VerbDef` is deprecated in favor of `HandlerDef`. The `#[allow(deprecated)]`
  in `lib.rs` exists for the re-export only; remove once all downstream crates
  migrate.
- `PropertyValue` supports recursive arrays and objects (`Array`, `Object`
  variants) for free-form JSON properties. The `Null` variant exists for
  explicit null representation.
- `Details` (on `KhiveError`) silently truncates to 8 key-value pairs. This is
  intentional -- bounded metadata prevents unbounded allocations on error paths.
- `ErrorKind` and `ErrorDomain` closed taxonomies: new variants are a
  source-breaking change and require an ADR before being added.

## Verification

```bash
cargo check -p khive-types
cargo test -p khive-types
cargo test -p khive-types --features serde
cargo check -p khive-types --no-default-features
cargo clippy -p khive-types -- -D warnings
```

Last reviewed: 2026-06-06
