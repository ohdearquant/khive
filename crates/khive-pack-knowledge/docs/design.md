# khive-pack-knowledge — Design

## Purpose

`khive-pack-knowledge` implements the knowledge corpus verbs for khive. It manages:

- **Corpus tier** — slug-keyed atoms and domain groupings stored in dedicated SQL tables
  (`knowledge_atoms`, `knowledge_domains` — V19 migration)
- **Section tier** — structured subsections per atom (ADR-048)
- **KG concept tier** — `learn` / `cite` / `topic` verbs as sugar over the KG entity layer

## ADR References

| ADR | What it governs |
|-----|----------------|
| ADR-047 | Knowledge pack verb surface and corpus structure |
| ADR-048 | Section profiles, ranking phases, Vamana ANN integration |
| ADR-007 | Namespace isolation model |
| ADR-017 | Pack trait and HANDLERS/REQUIRES surface |
| ADR-015 | Schema migration system (V19 adds `knowledge_atoms` / `knowledge_domains`) |

## Module Boundaries

| Module | Responsibility |
|--------|---------------|
| `lib.rs` | Pack registration, `KNOWLEDGE_HANDLERS` table, `PackRuntime::dispatch` |
| `handlers.rs` | `learn`, `cite`, `topic` verbs (KG concept tier sugar) |
| `knowledge/mod.rs` | Corpus handler implementations (18 verbs) and all shared SQL/scoring helpers |
| `knowledge/schema.rs` | Param and record types for serde deserialization and SQL row mapping |
| `knowledge/vamana.rs` | Shared Vamana ANN index lifecycle (warm-start, build, search, RRF fusion) |
| `knowledge/matching.rs` | TF-IDF term matching primitives (IDF table, BM25 term score) |

## Public Verb Contract

All 18 verbs are accessible through the `request` tool (ADR-016). The public surface is
`KnowledgePack` only — `handlers` and `knowledge` modules are `pub(crate)`.

See `src/lib.rs` for the `KNOWLEDGE_HANDLERS` static array with per-verb param definitions.

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
