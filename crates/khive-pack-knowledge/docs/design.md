# khive-pack-knowledge — Design

## Purpose

`khive-pack-knowledge` implements the knowledge corpus verbs for khive. It manages:

- **Corpus tier** — slug-keyed atoms and domain groupings stored in dedicated SQL tables
  (`knowledge_atoms`, `knowledge_domains` — V19 migration)
- **Section tier** — structured subsections per atom (10-value closed enum: overview, core_model,
  boundary_conditions, formalism, operational_guidance, examples, failure_modes, expert_lens,
  references, other)
- **KG concept tier** — `learn` / `cite` / `topic` verbs as sugar over the KG entity layer

## ADR Compliance

### Knowledge Pack Verb Surface (ADR-047)

- This pack implements the 19 verb corpus surface: atoms/domains CRUD, TF-IDF search with
  embedding rerank, fold, import, edit, challenge, adjudicate, and concept-tier sugar.
- Domain matching is case-insensitive: domain values are trimmed and lowercased before storage
  and comparison. The same normalized value is used in `properties.domain`, promoted tags,
  and response bodies so all three surfaces agree.

### Section Profiles and Vamana ANN Integration (ADR-048)

- The `SectionType` enum is a closed 10-value set. Headings in atlas markdown files are mapped
  to canonical section types via `from_str_loose`, which accepts common heading aliases.
- The section-read verb surface (Phase 3) is not yet wired. Forward-deployed helpers
  (`section_from_row`, `section_to_json`) are retained so Phase 3 can land without structural
  changes.
- Vamana ANN integration provides a parallel semantic signal to TF-IDF scoring. ANN hits are
  fused via RRF (k=60). The index lifecycle is owned by `knowledge/vamana.rs`: warm-load from
  persistent snapshot, fingerprint validation, rebuild from sqlite-vec corpus on stale/miss.

### Section Review Lifecycle (ADR-047)

- `knowledge.challenge` marks a section as disputed and increments `dispute_count` on the parent
  atom. `knowledge.adjudicate` resolves the dispute: `accept` → `verified`, `reject` → `reviewed`.
  This governance is specified in ADR-047 (Knowledge Pack) §section lifecycle governance.
- The Vamana warm-start protocol (`ensure_ann_background`) uses one `AnnWarmState` lifecycle per
  `{namespace, model}` key. Startup v1/v2 discovery and request background warm
  share the same attempt-owned `begin_warm` / `finish_warm` singleflight; a late
  pre-invalidation completion cannot release or complete a newer attempt.

### Pack Self-Registration (ADR-027)

- `KnowledgePackFactory` is submitted via `inventory::submit!` so the runtime can discover and
  load this pack by name without explicit wiring. `REQUIRES = ["kg"]` declares the dependency.

### Edge Ontology (ADR-002)

- `knowledge.cite` creates an `introduced_by` edge from a concept entity to a source entity
  (document, person, or org). The edge direction is concept → source.

### Schema Migration (ADR-015)

- Corpus tables (`knowledge_atoms`, `knowledge_domains`) are added in V19 migration.
- Section table (`knowledge_sections`) is added in a subsequent migration.

### ADR-016: Request DSL

- All 19 verbs are accessible through the `request` tool. The public surface is `KnowledgePack`
  only — `handlers` and `knowledge` modules are `pub(crate)`.

## Consistency Notes

- `knowledge/mod.rs` exceeds the 700-line soft limit by design. The corpus handler logic is
  kept together to avoid requiring ~30 private helpers to become `pub(crate)` and to avoid
  duplicating context structs across submodules. This will be revisited when the section-read
  verb surface stabilizes.
- `knowledge/vamana.rs` also exceeds 700 lines by design: the ANN lifecycle (SharedAnn type,
  snapshot persistence, build, search) is tightly coupled through the shared `AnnState`
  generation and warm-ownership locks and cannot be split without obscuring their ordering.
- The `Section` struct and its associated helper functions (`section_from_row`, `section_to_json`)
  are forward-deployed for Phase 3; they carry `#[allow(dead_code)]` with REASON annotations.

## Module Boundaries

| Module                  | Responsibility                                                               |
| ----------------------- | ---------------------------------------------------------------------------- |
| `lib.rs`                | Pack registration, `Pack` trait impl, `PackRuntime::dispatch` shim           |
| `vocab.rs`              | `KNOWLEDGE_HANDLERS` static array — 19 verb descriptors                      |
| `handlers.rs`           | `learn`, `cite`, `topic` verbs (KG concept tier sugar)                       |
| `knowledge/mod.rs`      | Corpus handler implementations (19 verbs) and all shared SQL/scoring helpers |
| `knowledge/schema.rs`   | Param and record types for serde deserialization and SQL row mapping         |
| `knowledge/vamana.rs`   | Shared Vamana ANN index lifecycle (warm-start, build, search, RRF fusion)    |
| `knowledge/matching.rs` | TF-IDF term matching primitives (tokenize, exact match, count)               |

## Namespace Isolation

All corpus SQL queries include `AND namespace = ?` predicates scoped to the caller token's
namespace. The `knowledge.import` verb delegates to `upsert_atoms` and `edit`, which each
enforce the caller namespace — no cross-namespace write is possible. `knowledge.compose`
accepts an explicit `namespace` as ADR-007's exact single-namespace escape. The same derived
token scopes automatic suggestion, corpus/section reads, KG blending, and the cross-pack
brain-profile weight read; Tier-3 pack-local feedback state is keyed by that namespace too.
Nested profile dispatches preserve the authorized token's per-request actor and scope. Direct
pack calls must present a matching authorized token, whose visibility is then narrowed to the
one explicit namespace. An absent parameter preserves the existing caller-token scope.
Other corpus handlers continue to consume the registry's transport-level namespace routing.

## Test Coverage

- `tests/integration.rs` — full verb surface, happy path + edge cases
- `tests/fixes.rs` — targeted regression coverage for audit-identified invariants
- `tests/bench.rs` — warm-latency smoke test (ignored by default; see `docs/benchmarks.md`)
