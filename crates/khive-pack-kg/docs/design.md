# khive-pack-kg Design

**Last reviewed:** 2026-06-06

## Scope

The KG pack provides 17 verb handlers for the knowledge graph substrate: entity
CRUD, note CRUD, edge creation/traversal, hybrid search, graph queries (GQL),
entity-anchored graph context (ADR-089), and event-sourced proposals (ADR-046).
It is the first-party pack shipped with the khive binary.

## ADR Compliance

| ADR                                                                | What it governs                                   | Status      |
| ------------------------------------------------------------------ | ------------------------------------------------- | ----------- |
| [ADR-001](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)       | 9 entity kinds (8 base + resource per ADR-048)    | Implemented |
| [ADR-002](../../../docs/adr/ADR-002-edge-ontology.md)              | 17 edge relations, closed set (15 base + 2 epistemic via ADR-055) | Implemented |
| [ADR-007](../../../docs/adr/ADR-007-namespace.md)                  | KG uses shared `local` namespace                  | Implemented |
| [ADR-013](../../../docs/adr/ADR-013-note-kind-taxonomy.md)         | 5 base note kinds                                 | Implemented |
| [ADR-014](../../../docs/adr/ADR-014-curation-operations.md)        | UUID-only get/update/delete                       | Implemented |
| [ADR-017](../../../docs/adr/ADR-017-pack-standard.md)              | Pack trait, vocabulary, edge rules                | Implemented |
| [ADR-001](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)       | Alias normalization (paper→document, write-time)  | Implemented |
| [ADR-045](../../../docs/adr/ADR-045-verb-response-presentation.md) | ISO-8601 timestamps at handler boundary           | Implemented |
| [ADR-046](../../../docs/adr/ADR-046-event-sourced-proposals.md)    | Event-sourced proposals (propose/review/withdraw) | Implemented |
| [ADR-048](../../../docs/adr/ADR-048-knowledge-section-profiles.md) | `resource` entity kind (9th kind)                 | Implemented |
| [ADR-089](../../../docs/adr/ADR-089-context-verb.md)               | `context` verb — entity-anchored graph context    | Implemented |

## Primary Modules

| Module                                                            | Purpose                                                  |
| ----------------------------------------------------------------- | -------------------------------------------------------- |
| [`src/lib.rs`](../src/lib.rs)                                     | Pack re-exports and crate documentation                  |
| [`src/pack.rs`](../src/pack.rs)                                   | KgPack struct, Pack trait impl, edge endpoint rules      |
| [`src/dispatch.rs`](../src/dispatch.rs)                           | PackRuntime impl, inventory self-registration            |
| [`src/handler_defs.rs`](../src/handler_defs.rs)                   | KG_HANDLERS static table (17 HandlerDef entries)         |
| [`src/handlers/mod.rs`](../src/handlers/mod.rs)                   | 17 verb handler implementations                          |
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

## `KG_EDGE_RULES` (pack.rs)

Adds person→org, person→project, and org→org pairs to the base edge-endpoint allowlist.
The person→project rows mirror person→org (issue #60): a person is a member of a project
the same way they are a member of an org, so the same member-not-component semantic
stretch accepted for person→org is extended here.

Test rationale (`pack.rs::tests`):

- `kg_pack_edge_rules_contain_no_duplicate_triples`: a duplicated `(relation, source,
  target)` triple would be a no-op additive rule (adding the same endpoint pair a second
  time changes nothing) and is a sign of a copy-paste error. Semantic similarity between
  relations (e.g. multiple relations accepting `org→org`) is expected and correct; the
  test checks only for exact-triple duplicates, not for shared per-relation endpoint sets.
- `kg_pack_edge_rules_cover_expected_relations`: a deliberate-change tripwire over the
  live `KG_EDGE_RULES`, complementing the ADR-076 §D2 non-redundancy certificate in the
  certificate test suite. A change to the set of relations that get pack-level endpoint
  extensions should be a deliberate, reviewed decision — not an accidental side effect.

## `context` verb internals (handlers/context.rs)

`relations_all_symmetric` mirrors `normalize_symmetric_direction` in
`khive-runtime/src/operations.rs` (private to that crate) — kept in lockstep because
`neighbors_with_query` forces `Direction::Both` under this exact condition regardless of
the direction actually requested, and the handler must know that happened to tag
direction correctly instead of issuing a second, redundant call.

`fetch_directed_neighbors` fetches up to `fanout` neighbors of `node_id`, each tagged with
its actual direction relative to `node_id` — it can't just trust a `direction` field on a
plain `NeighborHit` because `neighbors_with_query_directed` only ever tags hits `Out`/`In`
(`Both` never appears in a `DirectedNeighborHit`).

`assemble_within_budget` is a deterministic-order budget walk: it appends anchor entity
records and their neighbor records (each already produced in final display order) until
the next record's compact-JSON Unicode-scalar length would push the running total past
`budget`. Returns (assembled anchors, truncated, dropped anchors, dropped neighbors). A
budget exactly equal to the cumulative size does NOT truncate — the stop condition is
"would push the running total PAST budget", so a record landing exactly on the boundary
still fits.

### `handle_context` stage notes

- **Directed-neighbor fetch**: a single UNION ALL query for both directions (ADR-089
  context-verb optimization) instead of two separate direction-scoped calls — halves the
  storage neighbor SELECT count for this branch. The op already returns hits in global
  weight-descending, node_id-ascending order truncated to `fanout`, so no local
  re-sort/truncate is needed.
- **Stage 1 (anchor resolution)**: `entity_ids` is an explicit entity-anchor contract
  (ADR-089 §1: "honored in full"). `resolve_uuid_async` accepts any syntactically valid
  UUID without checking substrate or existence, so a random UUID, a note UUID, or an edge
  UUID would otherwise resolve here and then silently vanish from the response in Stage
  4's lenient "missing entity" fallback. The handler fails loudly instead: one batch
  existence check names every offending id.
- **Query-anchor overfetch**: fetches a larger candidate window than `limit` so that
  anchors which collapse into `entity_ids` duplicates don't under-fill the query leg —
  ADR-089 §1 promises search "fills up to `limit` additional anchors" after explicit ids,
  which requires looking past the first `limit` hits when some of them overlap explicit
  anchors. Bounded by a documented cap so a pathological overlap can't turn into an
  unbounded search.
- **Stage 2 (expansion), hop-1 stratum**: one stratum across all hop-1 parents under an
  anchor, sorted by weight desc, then neighbor id, then parent id (the last key only
  arbitrates true ties — same neighbor, same weight, different parent — so the "first
  discovering parent" is deterministic).
- **Stage 4 (assembly)**: explicit `entity_ids` anchors are already verified to exist in
  Stage 1; the Stage 4 existence check only guards the residual race of an anchor deleted
  concurrently between resolution and this fetch, or a neighbor entity that vanished the
  same way. Neighbors get the same lenient "missing node reads as absent" convention
  `neighbors_with_query` already applies (it returns an empty Vec rather than erroring on
  a nonexistent `node_id`) — they never enter the budget accounting.
