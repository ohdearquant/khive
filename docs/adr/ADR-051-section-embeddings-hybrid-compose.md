# ADR-051: Section-level embeddings and hybrid compose scoring

## Status

Accepted (2026-06-07). **Fully implemented** (2026-06-08) — Phases 1 and 2 shipped.

## Context

The knowledge corpus stores atoms (`knowledge_atoms`) and their parsed sections
(`knowledge_sections`). Today only **atoms** are embedded: `knowledge.index`
writes one vector per atom into the default embedder's vector store, and search
ranks atoms by TF-IDF with an atom-level embedding rerank.

`knowledge_sections` has carried an unused `embedding BLOB` column since its
introduction. An earlier engine (`engine_v1`) had a real section-level vector
path — `EmbeddedEngine::search_sections` (per-section cosine with fusion-strategy
dispatch) consumed by compose for token-budget section selection. That path was
**not ported** to the current pack: the column is never populated and never read, and
compose scores sections by static weights only (`section_type + edge + quality`).
The `retrieval` objective weight that was meant to carry section similarity is
defined but unapplied. This is a regression, not merely an un-run backfill.

An earlier implementation spec defines the intended design — breadcrumb-enriched
section embedding text, hash-incremental backfill, and a hybrid compose score —
tracked for the read side in issue #6.

## Decision

Restore section-level embeddings and hybrid compose, adapted to the current schema.

### Amendment 1 (2026-08-01): searchable-model-only atom indexing

Knowledge atom vectors are also single-model. `knowledge.index` embeds and writes only the
default model because `knowledge.search`, ANN warming, fresh-tail fusion, and compose all embed
and probe only that model. Writing secondary-model atom vectors without a model selector or fused
knowledge retrieval pays embedding and storage cost for rows no knowledge read path can consume.
Multi-model atom indexing must therefore land together with a model-aware or fused knowledge read
path; configuring additional models continues to fan out entity, note, and memory retrieval work.
This records the default-only disposition selected in issue #1513.

### Storage — reuse the existing column, single-model

The spec proposed a **separate** `section_embeddings` table (its engine_v1 target
lacked a per-section table and wanted multi-model rows). The current schema already
makes that choice differently: `knowledge_sections` **is** the per-section table,
with a built-in `embedding BLOB` column, `content_hash`, `sort_order` (the section
index), `heading`, `section_type`, and `tokens`. We therefore populate the
**existing `knowledge_sections.embedding` column** rather than add a redundant
table.

Section embeddings are **single-model** (the default embedder), consistent with
knowledge search, which retrieves via the default embedder's ANN. (Entity/note
vectors fan out across all registered engines; knowledge does not — see
[ADR-021] and the reindex contract.) The blob is little-endian `f32`,
**unit-normalised** so dot product equals cosine.

### Write — section embedding pass in `kkernel reindex`

Section embedding is folded into `kkernel reindex` alongside atoms:

- default: embed entities + notes + knowledge **atoms + sections**
- `--no-sections`: embed atoms but not sections
- `--sections-only`: embed only sections (skip graph + atoms)
- `--no-knowledge` / `--knowledge-only`: gate the whole knowledge pass (existing)

Embed text is **breadcrumb-enriched** so a section carries its context:
`atom_name \n heading \n\n content`, truncated to the model budget while
preserving the breadcrumb prefix. (The spec's `domain_title` breadcrumb is
omitted in v1 — domain membership is an edge lookup the pass does not yet join;
it can be added when section retrieval is wired to domain scoping.)

Re-embed is keyed on `content_hash`: with `--keep-existing`, sections whose
`embedding` is already present are skipped; otherwise all in-scope sections are
re-embedded. Hash-incremental **dirty tracking** (the spec's `atom_section_state`)
is an optimisation deferred to a follow-up; a full re-embed is correct, just less
incremental.

### Read — hybrid compose scoring (implemented)

Compose scores each candidate section with the hybrid formula:

```
0.55 · cosine(query, section)
0.20 · bm25(query, heading + content)
0.10 · cosine(query, atom)
0.10 · domain_score
0.05 · type_prior
```

- Section and atom cosines use the default embedder; the query is embedded once.
- BM25 is normalised over the compose candidate set (Okapi k1=1.5, b=0.75).
- `type_prior` uses brain-core `SectionPosteriorState::deterministic_weights()`
  (softmax over posterior means). The brain primitives were extracted to
  `khive-brain-core` (issue #5 / PR #17) to avoid pack-to-pack dependency.
- `domain_score` is binary membership (1.0 if atom belongs to the requested
  domain, 0.0 otherwise). Engine_v1 used `CONSISTS` edge weights; upgrading
  to weighted membership is a follow-up.
- **Partial coverage:** sections without stored embeddings score with
  `section_cosine=0.0`; BM25, atom cosine, domain, and type signals remain
  active. Compose works with zero, partial, or full section embeddings.
- Section vectors are **lazily batch-loaded for the shortlisted atoms**; no
  section ANN index until the query-time access pattern is proven (spec Q4).

### Token budget

Compose accepts a `max_tokens` parameter (default 8000, range 500–100,000).
Sections are greedily packed by descending score until the character budget
is exhausted (~4 chars/token). The budget applies to both section-mode and
atom-only fallback. This prevents unbounded output (observed 191K–673K chars
in production without budget).

### Auto-compose

When `domain_ids` and `atom_ids` are both absent, compose runs
`knowledge.suggest` internally to select the top N domains (controlled by
`auto_limit`, default 5). The internal suggest call uses the same query;
failures are caught gracefully and return an empty briefing with a
`suggest_error` diagnostic. The 10-word query minimum applies only in
auto-compose mode — explicit IDs accept any non-empty query.

### Query length gates

Empirical evaluation (8-domain sweep, 2026-06-08) showed:

- **Suggest**: short queries (1-3 words) cause disambiguation. Minimum 5 words.
- **Compose** (auto): longer queries (10+ words) produce better section ranking.
  Score spread widens from 0.29 (1-word) to 0.50 (30-word).

These are enforced at the handler boundary with descriptive error messages.

### Embed text enrichment

`atom_embed_text` includes tags: `"{name}\n\n{content}\n\nTags: tag1, tag2"`.
This gives the embedder richer semantic signal, particularly for domain mirror
atoms whose content is a short description.

### Domain mirror atoms

Domains stored in `knowledge_domains` are mirrored into `knowledge_atoms` with
a `type:domain` tag by the `upsert_domains` dual-write. A V3 migration
backfills existing domains (`INSERT OR IGNORE` — skips slug collisions with
real atoms). Suggest finds domains via the normal FTS + ANN + embedding rerank
pipeline with no code changes.

### Short ID prefix resolution

`load_domain_by_id_or_slug` and `load_atom_by_id_or_slug` resolve 8+ character
hex prefixes via `LIKE`. Ambiguous prefixes (>1 match) return an error rather
than silently selecting one.

### FTS trigger optimisation (V2 migration)

The `fts_sections_au` trigger is narrowed to `AFTER UPDATE OF heading, content,
section_type, namespace, atom_id`. Embedding-only UPDATEs during section reindex
no longer churn the FTS5 index (was the root cause of WAL bloat and FTS
corruption at scale).

V2 changed future trigger behavior but did not repair an index that had already
diverged, nor the external-content pitfall where rows predate their triggers.
V23 closes that historical repair gap by rebuilding `fts_sections` during
migration and through the explicit `knowledge.index(rebuild_fts=true)` operator
path, which also runs the rank-1 FTS5 integrity check.

### ANN warm-start

ANN warm-start is owned by the **daemon** process (`kkernel mcp --daemon`), not by
the stdio server. After the daemon socket is bound, `warm_all()` runs in a
background task, loading persisted Vamana snapshots into memory. The stdio MCP
server forwards requests to the warm daemon via Unix socket (`forward_or_spawn`);
it does not call `warm_all()` itself. This was changed in PR #20 (commit 9d9ec12):
blocking stdio startup on `warm_all` delayed MCP connection without benefit.

The net effect: steady-state traffic uses warmed Vamana indexes and benefits
from full ANN recall. A first suggest/search call that races the background
warm task may return results from lexical/atom signals only (BM25/FTS), without
Vamana fusion, until `warm_all()` completes. The zero-result cold-start bug is
resolved for steady-state traffic; there is no hard guarantee that the very
first request will see ANN results.

### Cross-encoder rerank (deferred, optional)

The spec's Phase 3 cross-encoder rerank (`ms-marco-MiniLM-L6-v2`, top-20, off the
critical path) is **out of scope** for this ADR and gated behind a future
feature flag.

## Consequences

- `knowledge_sections.embedding` is now load-bearing. Section coverage is visible
  via compose output (sections with embeddings show `section_cosine > 0`; without,
  `section_cosine = 0.0`).
- Compose has a semantic signal (synonym/paraphrase recall) beyond keyword-only.
- V2 migration (FTS trigger narrowing) and V3 migration (domain mirror backfill)
  are additive — safe for existing databases.
- Reindex cost: sections outnumber atoms ~5× (358K vs 94K including domain mirrors),
  all embedded with the default model at ~30/s. The `--no-sections` flag bounds this.
- Knowledge stays single-model; section-level multi-model fusion is a separate ADR.
- Auto-compose enables single-call query→briefing without knowing domain IDs upfront.
- Token budget (`max_tokens`) prevents unbounded output in production.

## References

- PR #17 — brain-core extraction (ADR-017 compliance)
- PR #18 — hybrid section scoring in compose
- PR #19 — progress bars, domain backfill, auto-compose, token budget, review fixes
- [ADR-021](ADR-021-memory-pack.md), [ADR-048](ADR-048-knowledge-section-profiles.md)

## Amendment 1: Blend KG entities into the compose candidate pool

Status: proposed.

### Problem

`knowledge.compose` reaches lore atoms only. On sharply technical queries, the
measured, expert-curated content often lives as knowledge-graph `concept` and
`document` entities (algorithms, papers, ADRs) rather than as a lore atom —
those entities rank top under `kg.search(kind="entity")` for the same query but
were invisible to compose callers. For example, a query naming a specific
technical technique can return only generic, low-relevance atoms from compose,
while the concept and document entities describing that technique — including
matching ADRs — rank at the top via `kg.search`.

### Decision

`knowledge.compose` blends `concept` and `document` KG entities into the
candidate pool for AUTO and explicit-`domain_ids` calls. `atom_ids`-only calls
never blend — the caller pinned exact atoms and gets exactly those back.

**Candidate discovery** reuses `KhiveRuntime::hybrid_search` — the identical
FTS+ANN RRF-fused retrieval path `kg.search(kind="entity")` dispatches to
(`khive-pack-kg`'s `handle_search` calls the same method) — run once per
blended kind (`concept`, `document`) and deduplicated by entity id. This does
not stand up a parallel retrieval stack; only the final relevance score used
to rank and cap the blended set is recomputed (see Scoring below).

**Scoring / fusion rule.** `hybrid_search`'s RRF-fused score (`k=10`, roughly
0.01–0.09 in practice) is not on a comparable scale to compose's atom/section
scores (embedding-cosine-based, weighted-additive, roughly 0–1). Rather than
rank-fusing two incomparable scales, blended entity candidates are **reranked**
with the exact same signal `rerank_text_items` already applies to atom bodies:
cosine similarity between the query embedding and an embedding of
`name + description`, computed via one shared `embed_batch` call. Because both
pools are scored with the identical metric against the identical query
embedding, the resulting scores land on the same 0–1 scale as atom/section
scores by construction — no separate rank-fusion step is needed to make them
comparable. (`hybrid_search`'s own RRF score is used only for candidate
discovery/ranking within the entity pool before rerank, never surfaced.)

**Inclusion floor.** A blended entity is included only if its reranked score
is `>=` the minimum reranked score among the atoms that made the final compose
body. The floor is self-calibrating (derived from the current request's own
atom scores) rather than a fixed constant, so it tracks whatever relevance bar
the query's atom pool actually cleared instead of an arbitrary threshold that
would need separate tuning per corpus. Applied after rerank, before the
5-entity cap. **Zero-atom edge case (Decision):** if the final compose body
contains zero atoms (e.g. every atom/section was trimmed by `max_tokens`),
the floor is undefined, so the compose blends **no** entities at all — an
entity is never blended into a briefing that has no atoms to calibrate
against, even when `blend_kg` is true and matching entities exist.

**Failure handling (Decision).** KG entity discovery/hydration failures
(`hybrid_search` or entity hydration erroring) degrade to the atom-only
response instead of aborting the whole `knowledge.compose` call — the
already-finalized atom/section body is a valid, useful briefing on its own.
The failure is logged (`tracing::warn!`) and `entities` is simply omitted,
identically to the "nothing blended" case.

**Budget.** Blended entities consume the same `max_tokens` budget as the rest
of the briefing, but are trimmed _after_ the atom/section body is finalized,
against whatever budget is left over (`char_budget - body_used`). A tight
budget never evicts an atom or section to make room for an entity — entities
are purely additive, capped at 5 (`KG_BLEND_CAP`), and are the first (only)
thing dropped when the budget is tight.

**Rendering.** Blended entities render as a distinct `## Knowledge graph`
markdown section (not interleaved into the atom section list), and are listed
in a new `entities` array in the JSON response (`{id, kind, name, score}`) —
additive; `atoms`/`domains`/`sections` are unchanged. `entities` is omitted
entirely when nothing blends (no field, not an empty array), preserving
byte-identical output for callers unaffected by this change.

**Opt-out.** A `blend_kg` boolean param (default `true`) lets callers disable
blending outright; `blend_kg=false` reproduces pre-Amendment-1 behavior exactly.

### Consequences

- Compose briefings on technical queries surface KG-curated concepts/ADRs
  alongside lore atoms without a second manual `kg.search` call.
- One extra `hybrid_search` round-trip per blended kind (2 total) plus one
  `embed_batch` call per compose request when `blend_kg` is enabled and the
  query resolves at least one atom — bounded by the existing per-request
  `ComposeTiming` slow-request WARN.
- Entity search is namespace-scoped identically to `kg.search` (primary
  namespace only for the vector leg per `hybrid_search`'s documented
  cross-namespace deferral) — no new namespace-visibility surface.
- `atom_ids`-only compose is unaffected — an explicit, minimal-surface escape
  hatch for callers who already know exactly which atoms they want.
