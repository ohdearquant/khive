# ADR-051: Section-level embeddings and hybrid compose scoring

## Status

Accepted (2026-06-07)

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

### Read — hybrid compose scoring

> **Status: deferred to Phase 2 (issue #6).** Phase 1 ships the **write** side only —
> section embeddings populated by `kkernel reindex`. The read-side scoring described
> below is the accepted design, not yet implemented; `knowledge.compose` remains
> atom-level until #6 lands. The `type_prior` term is served by brain profile section
> posteriors (ADR-048 §4), which requires the brain primitives to move into a shared
> crate first (issue #5) — `knowledge` cannot depend on the `brain` pack.

Compose scores each candidate section with the spec's hybrid formula:

```
0.55 · cosine(query, section)
0.20 · bm25(query, heading + content)
0.10 · cosine(query, atom)
0.10 · domain_score
0.05 · type_prior
```

- Section and atom cosines use the default embedder; the query is embedded once.
- BM25 is normalised over the compose candidate set.
- **Fallback:** a section with no stored embedding scores via the existing
  keyword-only formula — compose works with zero, partial, or full section
  embeddings, and never blocks on the backfill.
- Section vectors are **lazily batch-loaded for the shortlisted atoms** with an
  atom-level LRU; we do **not** load all section vectors at startup and do **not**
  add a section ANN index until the query-time access pattern is proven (spec Q4).

### Cross-encoder rerank (deferred, optional)

The spec's Phase 3 cross-encoder rerank (`ms-marco-MiniLM-L6-v2`, top-20, off the
critical path) is **out of scope** for this ADR and gated behind a future
feature flag.

## Consequences

- `knowledge_sections.embedding` becomes load-bearing once the read side lands.
  Surfacing section coverage in `knowledge.stats` is part of the deferred read-side
  work (issue #6); Phase 1 reports atom coverage only.
- Compose gains a semantic signal (synonym/paraphrase recall) over keyword-only
  — delivered by the deferred read side (issue #6).
- No schema migration is required for v1 (the column exists); adding
  `atom_section_state` later is additive.
- Reindex cost grows: sections outnumber atoms ~5× (358k vs 68k locally), all
  embedded with the default model. The `--no-sections` flag bounds this when only
  atom/graph vectors are needed.
- Knowledge stays single-model; if section-level multi-model fusion is ever
  wanted, that is a separate ADR (and likely the spec's separate-table design).

## References

- Issue #6 — hybrid section scoring + compose pipeline (the read-side work this ADR specifies)
- Issue #5 — extract brain posterior/profile primitives into a shared crate (unblocks the `type_prior` term)
- [ADR-021](ADR-021-memory-pack.md), [ADR-048](ADR-048-knowledge-section-profiles.md)
