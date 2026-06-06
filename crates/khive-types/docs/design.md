# khive-types Design

## ADR Compliance

### ADR-001: Entity Kind Taxonomy

- `EntityKind` is a closed enum with exactly 8 variants: `concept`, `document`, `dataset`,
  `project`, `person`, `org`, `artifact`, `service`.
- `EntityKind::ALL` enumerates them in taxonomy-table order.
- `FromStr` accepts the 8 canonical names (case-insensitive) plus convenience aliases
  (e.g., `"paper"` → `Document`, `"repo"` → `Project`). Aliases resolve to the base kind
  only; the subtype string (`entity_type`) is carried separately.
- `Entity.entity_type` holds the pack-governed subtype token; ontology type strings must not
  be stored raw in `properties`.

### ADR-002: Edge Ontology

- `EdgeRelation` is a closed enum with exactly 15 canonical relations.
- `EdgeRelation::ALL` lists them in ontology-table order.
- Wire format is snake_case (e.g., `"part_of"`, `"introduced_by"`).
- `FromStr` accepts canonical snake_case names, hyphen variants, and squashed forms
  (e.g., `"partof"`, `"derivedfrom"`) for ergonomic DSL entry. Squashed forms are not
  stored on the wire.
- `EdgeCategory` groups the 15 relations into 8 structural categories for query planners
  and UI rendering.
- Symmetric relations (`competes_with`, `composed_with`) are identified via `is_symmetric()`.

### ADR-004: Substrate Model

- Three substrates: `Note`, `Entity`, `Event` — represented by `SubstrateKind`.
- `SUBSTRATE_COUNT` is a compile-time constant (3).
- `Note` carries a pack-owned `kind` string validated by the loaded pack at the service boundary.
- `Note.status` (`NoteStatus`) is a cross-cutting lifecycle field distinct from pack-specific
  lifecycle fields (which use `"kind_status"` in `properties` to avoid semantic collision).
- `Entity.kind` is the closed `EntityKind` base enum.
- `Event` is append-only and never mutated or deleted.

### ADR-013: Note Kind Taxonomy

- The 5 base note kinds (`observation`, `insight`, `question`, `decision`, `reference`) are
  declared by the kg pack, not hardcoded in `khive-types`. This crate only carries the
  `Note` struct with a free-form `kind: String` validated at the pack boundary.

### ADR-017 / ADR-002: Pack-Extensible Edge Endpoints

- `EdgeEndpointRule` declares the types allowed at each end of an edge for a specific relation.
- Pack-declared rules are **additive**: they extend the allowed `(source, relation, target)`
  triples beyond the base contract. Packs cannot tighten base rules.
- `EndpointKind` distinguishes note-substrate endpoints (`NoteOfKind`) from entity-substrate
  endpoints (`EntityOfKind`).

### ADR-019: Pack-Auxiliary Schema

- `PackSchemaPlan` carries idempotent DDL statements a pack needs applied to the auxiliary
  schema. Statements use `CREATE TABLE IF NOT EXISTS`; they are not part of the core versioned
  migration chain.

### ADR-021: Edge Relation Enum (Closed Set)

- The `EdgeRelation` enum is the closed set — not extensible. Only the per-relation endpoint
  contract (via `EdgeEndpointRule`) is extensible by packs.

### ADR-023: Handler Visibility and Discovery

- `HandlerDef` replaces the deprecated `VerbDef` type alias.
- `Visibility::Verb` entries are surfaced on the MCP wire; `Visibility::Subhandler` entries
  are internal (operator-only).
- The `params` slice on `HandlerDef` enables `help=true` schema introspection. Empty (`&[]`)
  is the correct default for handlers without a fixed parameter schema.

### ADR-025: Speech-Act Taxonomy for Verbs

- `VerbCategory` classifies verbs by illocutionary force: `Assertive`, `Directive`,
  `Commissive`, `Declaration`. `Expressive` is intentionally absent — no verb currently
  uses it.
- The category is a documentation and introspection tag only. It is NOT used for permission
  checking, transport routing, or return-shape selection.
- Every `Visibility::Verb` handler MUST carry a category.

### ADR-031: Pack-Extensible Edge Endpoints (extends ADR-017)

- Endpoint contract extension is additive only. Pack rules declare additional allowed
  `(source, relation, target)` triples; they cannot remove or tighten base-contract rules.

### ADR-034: Pack Validation Rules

- `Pack::VALIDATION_RULES` is a declarative catalog of rule identifiers contributed by a
  pack. Rule IDs are namespaced `<pack-name>/<rule-id>`. Actual rule implementations live
  in `khive-runtime`; this const is metadata-only.

### ADR-037: Pack Dependencies

- `Pack::REQUIRES` declares other pack names whose vocabulary this pack references. The
  runtime validates that every required pack is loaded before registration.

### ADR-045: Verb Presentation Policy

- `VerbPresentationPolicy` controls whether a verb's response can be trimmed by
  agent-mode transforms.
- `AlwaysVerbose` verbs bypass agent-mode transforms entirely. The current set:
  `get`, `link`, `query`, `traverse`, `neighbors`, `brain.feedback`.
- `link` is `AlwaysVerbose` because the returned edge ID is the only handle for follow-up
  graph traversal calls. At scale (~65K edges), two edges can share the same 8-character
  prefix, so shortening the edge ID breaks downstream chaining.
- `brain.feedback` is `AlwaysVerbose` because callers chain `target_id` from the response
  into subsequent feedback or profile queries; an 8-char prefix is ambiguous.

### ADR-046: Proposal Lifecycle

- `EventKind` includes `ProposalCreated`, `ProposalReviewed`, `ProposalApplied`,
  `ProposalWithdrawn` for the event-sourced proposal state machine.
- `ProposalChangeset` is the typed change payload; `EntityDraft`, `ProposalEntityPatch`,
  `NoteDraft` are structured drafts for adding/modifying entities and notes via proposals.
- `EntityDraft.kind` is validated against the closed 8-kind entity taxonomy at apply time.
- `ProposalDecision.as_str()` returns the bare variant name for TEXT column storage — callers
  must NOT use `serde_json::to_string`, which adds JSON quoting.

## Consistency Notes

- `NoteLifecycleSpec.field` is documented to use `"kind_status"` for pack-owned lifecycle
  fields to avoid collision with `Note.status` (`NoteStatus`). This is a convention enforced
  by documentation; the runtime does not validate the field name string.
- `VerbDef` is deprecated in favor of `HandlerDef`. The `#[allow(deprecated)]` in `lib.rs`
  exists for the re-export only; remove once all downstream crates migrate.
- `PropertyValue` supports recursive arrays and objects (`Array`, `Object` variants) for
  free-form JSON properties. The `Null` variant exists for explicit null representation.
- `Details` (on `KhiveError`) silently truncates to 8 key-value pairs. This is intentional
  — bounded metadata prevents unbounded allocations on error paths.
- `ErrorKind` and `ErrorDomain` closed taxonomies: new variants are a source-breaking change
  and require an ADR before being added.
