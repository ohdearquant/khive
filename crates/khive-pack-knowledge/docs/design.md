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

### ADR-047: Knowledge Pack Verb Surface

- This pack implements the 18 verb corpus surface: atoms/domains CRUD, TF-IDF search with
  embedding rerank, fold, import, edit, challenge, adjudicate, and concept-tier sugar.
- Domain matching is case-insensitive: domain values are trimmed and lowercased before storage
  and comparison. The same normalized value is used in `properties.domain`, promoted tags,
  and response bodies so all three surfaces agree.

### ADR-048: Section Profiles and Vamana ANN Integration

- The `SectionType` enum is a closed 10-value set. Headings in atlas markdown files are mapped
  to canonical section types via `from_str_loose`, which accepts common heading aliases.
- The section-read verb surface (Phase 3) is not yet wired. Forward-deployed helpers
  (`section_from_row`, `section_to_json`) are retained so Phase 3 can land without structural
  changes.
- Vamana ANN integration provides a parallel semantic signal to TF-IDF scoring. ANN hits are
  fused via RRF (k=60). The index lifecycle is owned by `knowledge/vamana.rs`: warm-load from
  persistent snapshot, fingerprint validation, rebuild from sqlite-vec corpus on stale/miss.

### ADR-049: Section Review Lifecycle

- `knowledge.challenge` marks a section as disputed and increments `dispute_count` on the parent
  atom. `knowledge.adjudicate` resolves the dispute: `accept` → `verified`, `reject` → `reviewed`.
- The Vamana warm-start protocol (`ensure_ann_background`) fires at most once per
  `{namespace, model}` key using a `Mutex<HashSet>` single-flight guard.

### ADR-027: Pack Self-Registration

- `KnowledgePackFactory` is submitted via `inventory::submit!` so the runtime can discover and
  load this pack by name without explicit wiring. `REQUIRES = ["kg"]` declares the dependency.

### ADR-002: Edge Ontology

- `knowledge.cite` creates an `introduced_by` edge from a concept entity to a source entity
  (document or person). The edge direction is concept → source.

### ADR-015: Schema Migration

- Corpus tables (`knowledge_atoms`, `knowledge_domains`) are added in V19 migration.
- Section table (`knowledge_sections`) is added in a subsequent migration.

### ADR-016: Request DSL

- All 18 verbs are accessible through the `request` tool. The public surface is `KnowledgePack`
  only — `handlers` and `knowledge` modules are `pub(crate)`.

## Consistency Notes

- `knowledge/mod.rs` exceeds the 700-line soft limit by design. The corpus handler logic is
  kept together to avoid requiring ~30 private helpers to become `pub(crate)` and to avoid
  duplicating context structs across submodules. This will be revisited when the section-read
  verb surface stabilizes.
- `knowledge/vamana.rs` also exceeds 700 lines by design: the ANN lifecycle (SharedAnn type,
  snapshot persistence, build, search) is tightly coupled through the shared `AnnState` lock
  and cannot be split without breaking the atomic lock protocol.
- The `Section` struct and its associated helper functions (`section_from_row`, `section_to_json`)
  are forward-deployed for Phase 3; they carry `#[allow(dead_code)]` with REASON annotations.

## Module Boundaries

| Module | Responsibility |
|--------|---------------|
| `lib.rs` | Pack registration, `Pack` trait impl, `PackRuntime::dispatch` shim |
| `vocab.rs` | `KNOWLEDGE_HANDLERS` static array — 18 verb descriptors |
| `handlers.rs` | `learn`, `cite`, `topic` verbs (KG concept tier sugar) |
| `knowledge/mod.rs` | Corpus handler implementations (18 verbs) and all shared SQL/scoring helpers |
| `knowledge/schema.rs` | Param and record types for serde deserialization and SQL row mapping |
| `knowledge/vamana.rs` | Shared Vamana ANN index lifecycle (warm-start, build, search, RRF fusion) |
| `knowledge/matching.rs` | TF-IDF term matching primitives (tokenize, exact match, count) |

## Namespace Isolation

All corpus SQL queries include `AND namespace = ?` predicates scoped to the caller token's
namespace. The `knowledge.import` verb delegates to `upsert_atoms` and `edit`, which each
enforce the caller namespace — no cross-namespace write is possible. An explicit `namespace`
parameter is not supported (it was removed to prevent contract/implementation mismatches;
see KPK-AUD-006).

## Test Coverage

- `tests/integration.rs` — full verb surface, happy path + edge cases
- `tests/fixes.rs` — targeted regression coverage for audit-identified invariants
- `tests/bench.rs` — warm-latency smoke test (ignored by default; see `docs/benchmarks.md`)
