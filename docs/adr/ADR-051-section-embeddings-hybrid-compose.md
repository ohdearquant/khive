# ADR-051: Section-level embeddings and hybrid compose scoring

## Status

Accepted (2026-06-07). **Fully implemented** (2026-06-08) — Phases 1 and 2 shipped.

## Context

The knowledge corpus stores atoms (`knowledge_atoms`) and their parsed sections
(`knowledge_sections`). Today only **atoms** are embedded: `knowledge.index`
writes one vector per atom into the default embedder's vector store, and search
ranks atoms by TF-IDF with an atom-level embedding rerank.

`knowledge_sections` has carried an unused `embedding BLOB` column since its
introduction. The pre-OSS engine (`engine_v1`) had a real section-level vector
path — `EmbeddedEngine::search_sections` (per-section cosine with fusion-strategy
dispatch) consumed by compose for token-budget section selection. That path was
**not ported** to the OSS pack: the column is never populated and never read, and
compose scores sections by static weights only (`section_type + edge + quality`).
The `retrieval` objective weight that was meant to carry section similarity is
defined but unapplied. This is a regression, not merely an un-run backfill.

A pre-OSS implementation spec defines the intended design — breadcrumb-enriched
section embedding text, hash-incremental backfill, and a hybrid compose score —
tracked for the read side in issue #6.

## Decision

Restore section-level embeddings and hybrid compose, adapted to the OSS schema.

### Storage — reuse the existing column, single-model

The spec proposed a **separate** `section_embeddings` table (its engine_v1 target
lacked a per-section table and wanted multi-model rows). The OSS schema already
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

### ANN warm-start

The stdio MCP server calls `warm_all()` before accepting connections, loading
persisted Vamana snapshots synchronously. This eliminates the cold-start
0-result bug on the first suggest/search call after startup.

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
- PR #19 — progress bars, domain backfill, auto-compose, token budget, codex fixes
- [ADR-021](ADR-021-memory-pack.md), [ADR-048](ADR-048-knowledge-section-profiles.md)
