# khive-pack-kg Design

**Last reviewed:** 2026-06-06

## Scope

The KG pack provides 16 verb handlers for the knowledge graph substrate: entity
CRUD, note CRUD, edge creation/traversal, hybrid search, graph queries (GQL),
and event-sourced proposals (ADR-046). It is the first-party pack shipped with
the khive binary.

## ADR Compliance

| ADR                                                                | What it governs                                   | Status      |
| ------------------------------------------------------------------ | ------------------------------------------------- | ----------- |
| [ADR-001](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)       | 9 entity kinds (8 base + resource per ADR-048)    | Implemented |
| [ADR-002](../../../docs/adr/ADR-002-edge-ontology.md)              | 15 edge relations, closed set                     | Implemented |
| [ADR-007](../../../docs/adr/ADR-007-namespace.md)                  | KG uses shared `local` namespace                  | Implemented |
| [ADR-013](../../../docs/adr/ADR-013-note-kind-taxonomy.md)         | 5 base note kinds                                 | Implemented |
| [ADR-014](../../../docs/adr/ADR-014-curation-operations.md)        | UUID-only get/update/delete                       | Implemented |
| [ADR-017](../../../docs/adr/ADR-017-pack-standard.md)              | Pack trait, vocabulary, edge rules                | Implemented |
| [ADR-001](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)       | Alias normalization (paper→document, write-time)  | Implemented |
| [ADR-045](../../../docs/adr/ADR-045-verb-response-presentation.md) | ISO-8601 timestamps at handler boundary           | Implemented |
| [ADR-046](../../../docs/adr/ADR-046-event-sourced-proposals.md)    | Event-sourced proposals (propose/review/withdraw) | Implemented |
| [ADR-048](../../../docs/adr/ADR-048-knowledge-section-profiles.md) | `resource` entity kind (9th kind)                 | Implemented |

## Primary Modules

| Module                                                            | Purpose                                                  |
| ----------------------------------------------------------------- | -------------------------------------------------------- |
| [`src/lib.rs`](../src/lib.rs)                                     | Pack re-exports and crate documentation                  |
| [`src/pack.rs`](../src/pack.rs)                                   | KgPack struct, Pack trait impl, edge endpoint rules      |
| [`src/dispatch.rs`](../src/dispatch.rs)                           | PackRuntime impl, inventory self-registration            |
| [`src/handler_defs.rs`](../src/handler_defs.rs)                   | KG_HANDLERS static table (16 HandlerDef entries)         |
| [`src/handlers/mod.rs`](../src/handlers/mod.rs)                   | 16 verb handler implementations                          |
| [`src/vocab.rs`](../src/vocab.rs)                                 | EntityKind (9) and NoteKind (5) enums with alias parsing |
| [`src/entity_type_registry.rs`](../src/entity_type_registry.rs)   | Validates entity_type against per-kind subtypes          |
| [`src/apply_worker/mod.rs`](../src/apply_worker/mod.rs)           | Applies approved proposal changesets to KG               |
| [`src/projection_worker/mod.rs`](../src/projection_worker/mod.rs) | Maintains proposals_open projection table                |

## Tests

| Path                                                          | What it covers                                                                   |
| ------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| [`tests/integration.rs`](../tests/integration.rs)             | End-to-end verb dispatch tests                                                   |
| [`tests/apply_worker.rs`](../tests/apply_worker.rs)           | Proposal apply worker scenarios                                                  |
| [`tests/projection_worker.rs`](../tests/projection_worker.rs) | Projection CAS and state machine tests                                           |
| `src/handlers/tests.rs` (inline)                              | Unit tests for param deserialization, weight validation, timestamp normalization |
| `src/vocab.rs` (inline)                                       | Entity/note kind roundtrip and alias tests                                       |

## Invariants

1. Edge weights must be finite and in `[0.0, 1.0]`. Invalid values are rejected,
   not clamped.
2. Entity kinds and note kinds are validated against the pack vocabulary and
   registry. Unknown kinds return an error listing valid options.
3. The `resource` entity kind (ADR-048) is registered via `ENTITY_KINDS` and
   resolved through `vocab::EntityKind::from_str` (which also handles aliases
   `atom`, `skill`, `tool`, `prompt`, `template`, `runbook`).
4. Namespace override: KG entity/edge verbs use the shared `local` namespace
   regardless of caller namespace (ADR-007). Note verbs use the caller namespace.
5. Proposals follow the state machine:
   `open -> approved -> applying -> applied` or `open -> withdrawn`.
   The `applying` state is transient and blocks concurrent withdraw.

## Failure Modes

- **Invalid weight**: returns `RuntimeError::InvalidInput` with the supplied
  value and valid range.
- **Unknown kind**: returns `RuntimeError::InvalidInput` listing all valid kinds
  from the registry.
- **Endpoint violation**: link returns the ADR-002 error enriched with valid
  relations for the specific entity-kind pair.
- **Proposal CAS miss**: returns success with `cas_hit: false`; no duplicate
  events are emitted.
