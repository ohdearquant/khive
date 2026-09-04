# khive-pack-knowledge — Design

## Purpose

`khive-pack-knowledge` implements the knowledge corpus verbs for khive. It manages:

- **Corpus tier** — slug-keyed atoms and domain groupings stored in dedicated SQL tables
  (`knowledge_atoms`, `knowledge_domains` — V19 migration)
- **Section tier** — structured subsections per atom (10-value closed enum: overview, core_model,
  boundary_conditions, formalism, operational_guidance, examples, failure_modes, expert_lens,
  references, other)
- **Evaluation tier** — operator-triggered, draft-inclusive retrieval evaluation with summaries
  stored in the pack-owned `knowledge_eval_runs` table
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

### Retrieval Quality Measurement (ADR-082)

- `knowledge.eval_retrieval` is an operator-only `Subhandler`, so it does not change the 19-verb
  MCP surface. It searches only atoms at a fixed k of 5 and includes drafts so retrieval quality
  remains independent of finalization coverage.
- Query-set validation is fail-fast. Successful runs persist namespace-scoped aggregate
  precision, recall, and MRR for `knowledge.stats`. Query-set paths must be absolute and are
  canonicalized before reads and persistence so daemon launch directories cannot change a run.

### Edge Ontology (ADR-002)

- `knowledge.cite` creates an `introduced_by` edge from a concept entity to a source entity
  (document, person, or org). The edge direction is concept → source.

### Lexical candidate fetch and stage budget (issue #1930 Amendment 2, issue #2396)

- `fetch_fts_candidates` (`knowledge/search.rs`) runs each FTS5 term in two phases instead of one
  joined query. Phase A ranks bare rowids off the `fts_knowledge` index only (`bm25()` needs the
  index and docsize shadow table, not the content row), carrying no namespace predicate because
  `namespace` is UNINDEXED on that external-content table and filtering on it there would force a
  content-row fetch per candidate anyway:
  `SELECT rowid FROM fts_knowledge WHERE fts_knowledge MATCH ?1 ORDER BY bm25(fts_knowledge), rowid LIMIT ?2`.
  Phase B hydrates only the surviving rowids from `knowledge_atoms` in chunks, applying namespace,
  soft-delete, status, and type eligibility at that point — the first query in the whole fetch to
  touch a full atom row (including `content`), and only for rows that already cleared phase A's
  cap. This closes the read-cost hole where every FTS match paid a scattered read against the
  whole (multi-gigabyte-scale) atom table before its own per-term `LIMIT` applied. The hydration
  statement (`phase_b_hydration_statement`) filters on `+a.namespace = ?1` — a unary-plus, not a
  bare equality — for the same reason as the sibling `hydrate_atoms_statement`/
  `hydrate_domains_statement`: this codebase never runs `ANALYZE` on `knowledge_atoms`, so the
  no-statistics planner prefers `idx_knowledge_atoms_ns` over the rowid primary key for a large
  `rowid IN (...)` list; the unary plus defeats that index without changing the predicate.
- When phase B carries a status or type eligibility predicate, phase A overfetches by
  `PHASE_A_OVERFETCH_FACTOR` (4x) so an ineligible-heavy top page cannot starve phase B of rows
  that are eligible further down the bm25 ranking. If phase A still returned a full page and
  eligible rows remain short of the per-term cap, the probe widens by the same factor once more,
  up to `PHASE_A_WIDEN_CEILING` (8000) — bounding the worst case to a fixed per-term cost instead
  of an unbounded retry loop. If widening reaches the ceiling and the eligible set is _still_
  short, the term falls back once to the pre-two-phase eligibility-scoped join (bm25 over
  `fts_knowledge` joined to `knowledge_atoms` with every predicate applied before its own
  per-term `LIMIT`) so more than `PHASE_A_WIDEN_CEILING` ineligible top-ranked rows can never hide
  a real candidate — this fallback trades the bounded-cost guarantee for correctness on the rare
  term pathological enough to exhaust the ceiling.
- When no term produces an eligible row, the empty-result fallback (a bounded, namespace-filtered
  full scan of `knowledge_atoms` ordered by recency) is gated on namespace-scoped evidence only: a
  chunked `SELECT 1 FROM knowledge_atoms WHERE rowid IN (...) AND namespace = ?1 LIMIT 1` over the
  rowids every term's phase A already fetched, never a second, unscoped `fts_knowledge` probe. An
  unscoped probe is a cross-namespace existence oracle — a caller in namespace A whose term exists
  only in namespace B would see the fallback suppressed (empty result) exactly as for a term that
  exists nowhere, while a caller whose term genuinely matches nothing still falls through to the
  fallback's rows; the two cases must be indistinguishable by response shape.
- Both `PHASE_A_WIDEN_CEILING` and `LEXICAL_STAGE_BUDGET_MS` (below) have test-only overrides
  (`with_phase_a_widen_ceiling_override`, `with_lexical_stage_budget_override_ms`) implemented as
  `tokio::task_local!` values scoped to the calling task, mirroring
  `khive_storage::scope_request_read_deadline`'s own mechanism — never a process-global `AtomicU64`,
  which a concurrently running test with no override of its own could observe.
- The lexical fetch runs under its own read-deadline budget (`LEXICAL_STAGE_BUDGET_MS`, 8s),
  scoped via `khive_storage::scope_request_read_deadline`. Nested deadlines keep whichever is
  earlier while active and pop back to the outer request deadline once the scope exits, so a
  lexical-stage timeout narrows only that stage's own budget — it no longer implies the whole
  request is out of time. `search`, `suggest`, and their decomposed variant gate every downstream
  step (embedding rerank, body-line counts, member-size pricing) on the live ambient
  `khive_storage::request_read_is_cancelled()` check rather than the stage-local timeout flag, so a
  lexical-only degradation with request time left to spare still gets a full rerank pass; the
  `lexical_timeout` degradation flag is still attached to the response whenever the stage itself
  timed out, independent of whether the rest of the request completed normally.
- `load_domain_member_token_sizes` (member-token pricing for `suggest`'s `results[].size`) returns
  a `(HashMap<String, usize>, bool)` — the `bool` marks whether the whole batch timed out before
  any domain could be measured. A single `query_all` call has no partial-completion state, so one
  flag covers every domain in the batch; a genuinely zero-member domain and an unmeasured one are
  otherwise indistinguishable in the plain map. `suggest` never serializes `size: 0` for an
  unmeasured domain — it serializes `null` and lists the domain under
  `degraded.member_sizing_timeout.domain_ids`. `FoldCandidate::size` (`knowledge.fold`) is a
  non-optional `usize`, so a caller that feeds `suggest`'s `results` straight into a `fold` call
  gets a hard parse error on a `null` size instead of `fold` silently admitting an unpriced domain
  as a free item — the intended fail-closed behavior.

### Schema Ownership (ADR-015, ADR-028)

- Corpus tables (`knowledge_atoms`, `knowledge_domains`) are added in V19 migration.
- Section table (`knowledge_sections`) is added in a subsequent migration.
- `knowledge_eval_runs` is auxiliary Knowledge-pack state declared through the pack's static and
  runtime schema plans. Centralized startup applies its idempotent DDL to the backend assigned to
  Knowledge; it does not claim a core migration-ledger version.

### ADR-016: Request DSL

- All 19 verbs are accessible through the `request` tool. The public surface is `KnowledgePack`
  only — `handlers` and `knowledge` modules are `pub(crate)`.

## Consistency Notes

- `knowledge/vamana.rs` also exceeds 700 lines by design: the ANN lifecycle (SharedAnn type,
  snapshot persistence, build, search) is tightly coupled through the shared `AnnState`
  generation and warm-ownership locks and cannot be split without obscuring their ordering.
- The `Section` struct and its associated helper functions (`section_from_row`, `section_to_json`)
  are forward-deployed for Phase 3; they carry `#[allow(dead_code)]` with REASON annotations.

## Module Boundaries

| Module                  | Responsibility                                                                           |
| ----------------------- | ---------------------------------------------------------------------------------------- |
| `lib.rs`                | Public exports and the operator-facing `reindex_knowledge` library entry                 |
| `pack.rs`               | Pack registration, `Pack` trait impl, `PackRuntime::dispatch` shim                       |
| `vocab.rs`              | Pack schema statements and the handler descriptor table (19 public verbs + 1 subhandler) |
| `handlers.rs`           | `learn`, `cite`, `topic` verbs (KG concept tier sugar)                                   |
| `knowledge/mod.rs`      | Knowledge handler module boundaries and shared exports                                   |
| `knowledge/eval.rs`     | Offline query-set validation, atom retrieval scoring, and run persistence                |
| `knowledge/schema.rs`   | Param and record types for serde deserialization and SQL row mapping                     |
| `knowledge/vamana.rs`   | Shared Vamana ANN index lifecycle (warm-start, build, search, RRF fusion)                |
| `knowledge/matching.rs` | TF-IDF term matching primitives (tokenize, exact match, count)                           |

## Namespace Isolation

All corpus SQL queries include `AND namespace = ?` predicates scoped to the caller token's
namespace. The `knowledge.import` verb delegates to the import-preserving atom upsert path and
`edit`, which each enforce the caller namespace — no cross-namespace write is possible.
`knowledge.compose`
accepts an explicit `namespace` as ADR-007's exact single-namespace escape. The same derived
token scopes automatic suggestion, corpus/section reads, KG blending, and the cross-pack
brain-profile weight read; Tier-3 pack-local feedback state is keyed by that namespace too.
Nested profile dispatches preserve the authorized token's per-request actor and scope. Direct
pack calls must present a matching authorized token, whose visibility is then narrowed to the
one explicit namespace. An absent parameter preserves the existing caller-token scope.
Other corpus handlers continue to consume the registry's transport-level namespace routing.
The evaluation runner derives the same token namespace for draft-inclusive search and persisted
run summaries; `knowledge.stats` reads only summaries from that namespace.

## Test Coverage

- `tests/integration.rs` — full verb surface, happy path + edge cases
- `tests/fixes.rs` — targeted regression coverage for audit-identified invariants
- `tests/import_integrity.rs` — bounded traversal, stable path identity, validate-first writes,
  whole-file atom mode, and additive import reporting
- `tests/eval_retrieval.rs` — schema routing/idempotence, validation, draft-corpus scoring,
  persistence, and namespace isolation
- `tests/bench.rs` — warm-latency smoke test (ignored by default; see `docs/benchmarks.md`)
