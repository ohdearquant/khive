# ADR-084: Pack-Level Multi-Engine Orchestration — Fan-Out, Score Normalization, Weighted RRF

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-081 (Embedder Registry), ADR-082 (Engine config), ADR-083 (Runtime API
takes `model_id`), ADR-061 (Retrieval Infrastructure), ADR-062 (Recall Pipeline)\
**Part of**: ADR-078 (Multi-Engine Embedding umbrella)

## Context

ADR-081 establishes the `Embedder` trait and `EmbedderRegistry`. ADR-082 establishes the
TOML schema operators write. ADR-083 establishes that the runtime no longer generates
embeddings — callers pre-compute vectors and pass `model_id` for table routing.

The remaining decision is: **where in the call graph does the multi-engine fan-out
(parallel embed across N engines + per-engine search + weighted fusion) actually live**, and
what does that orchestration look like in code? This ADR locks the **pack-level
orchestration pattern** — recall and search verbs receive `Arc<EmbedderRegistry>`, fan out
across engines in parallel, apply per-engine score normalization, fuse with weighted RRF.

Multi-engine orchestration lives in **pack handlers**, not in `khive-runtime` and not in
`khive-retrieval`, because the orchestration shape is verb-specific (memory's decay-weighted
recall, kg's entity-scored search, future packs' custom logic).

## Decision

### Multi-engine fan-out is the pack handler's responsibility

Pack handlers receive `Arc<EmbedderRegistry>` (filtered per ADR-081 D8) via the
`KhiveRuntime` they hold (per ADR-083, runtime exposes `embedders()` accessor). Recall and
search verbs implement the four-step fan-out:

1. **Embed query with every configured engine in parallel** —
   `registry.embed_query_all(query_text)` returns `Vec<(EngineConfig, Vec<f32>)>`.
2. **Per engine: search its dedicated vector index** — call `runtime.vector_search(ns,
   engine.model_id, query_vec, candidate_pool_size, kind)` for each engine; collect ranked
   results.
3. **Per-engine score normalization** — apply `noise_floor` (discard hits below) and
   `max_similarity` (cap for normalization); filter by `threshold`. Per-engine calibration
   parameters come from ADR-082.
4. **Weighted RRF across engines** — `khive_fusion::fuse(per_engine_hits,
   FusionStrategy::Weighted { weights }, candidate_pool_size)` where `weights` are pulled
   from `registry.engines()[i].config.weight`.

### Read-side pseudo-spec

```text
async fn handle_recall(args):
    registry = self.runtime.embedders()                       // ADR-083 accessor

    # 1. Parallel embed across engines (asymmetric via query_prefix per ADR-081)
    embeddings = registry.embed_query_all(args.query)         # Vec<(EngineConfig, Vec<f32>)>

    # 2. Per-engine search
    per_engine_hits = []
    for (cfg, query_vec) in embeddings:
        hits = self.runtime.vector_search(
            args.namespace, cfg.name, query_vec,
            args.candidate_pool_size, Some(SubstrateKind::Note))
        normalized = normalize_hits(hits, cfg.noise_floor, cfg.max_similarity)
        filtered = filter_threshold(normalized, cfg.threshold)
        per_engine_hits.append(filtered)

    # 3. Weighted RRF across engines
    weights = registry.engines().iter().map(|e| e.config.weight as f64).collect()
    vector_fused = khive_fusion::fuse(
        per_engine_hits, FusionStrategy::Weighted { weights }, args.candidate_pool_size)

    # 4. Layer FTS5 keyword path
    text_hits = self.runtime.text(args.namespace).search(...)
    final = fuse(vector_fused, text_hits, args.fusion_strategy, args.limit)

    # 5. Apply pack-specific scoring (memory: decay × importance; kg: entity-density; etc.)
    return apply_pack_scoring(final, args)
```

### Write-side mirror

```text
async fn handle_remember(args):
    registry = self.runtime.embedders()
    note_id = self.runtime.create_note(args.content, ...)

    # Embed content with every engine (asymmetric via document_prefix per ADR-081)
    embeddings = registry.embed_document_all(args.content)

    # Write to each engine's per-(model, dim) vector table
    for (cfg, vector) in embeddings:
        self.runtime.upsert_vector(args.namespace, cfg.name, note_id, vector)
```

### Per-engine score normalization

Each engine produces cosine similarity scores; the `noise_floor`, `max_similarity`, and
`threshold` from `EngineConfig` (ADR-082) calibrate per-engine output:

- `noise_floor`: cosine scores below this are treated as random noise; discarded entirely.
- `max_similarity`: cosine cap for normalization. Per-engine scaling brings disparate
  engines onto a comparable scale before fusion.
- `threshold`: per-engine minimum to enter the fusion stage. Engines below threshold for a
  given query effectively don't contribute to the fused list.

These parameters were empirically tuned in khive-internal (the 2026-03-26 Chinese-blindspot
crisis). v1 inherits the same defaults; per-corpus retuning is operator responsibility.

### Weighted RRF rationale

The per-engine weight (ADR-082 §`[[engines]] weight`) reflects each engine's relative quality
for the deployment's corpus. BGE's English semantic strength vs mE5's multilingual coverage
vs Qwen3's instruction-tuned recall — operators pick weights based on empirical retrieval
quality, not configuration aesthetics.

`khive_fusion::FusionStrategy::Weighted { weights }` already exists; the pack handlers just
wire engine weights into it. No new fusion math required.

(Note: this is **engine-level** weighted RRF — fusing per-engine ranked lists. It is distinct
from the **backend-level** unweighted RRF in ADR-087, which fuses per-backend ranked lists at
the substrate-search layer. Different concerns at different layers.)

### Pack-specific scoring layered on top

After multi-engine fusion + text/vector hybrid fusion, pack-specific scoring applies:

- **memory pack** (ADR-036): salience × exp(-decay_factor × age_days) — the decay-weighted
  recall scoring. Multi-engine fusion produces the candidate set; memory scoring re-ranks it.
- **kg pack** (ADR-024 + ADR-061): entity-density or pack-specific scoring of the candidate
  set.
- **future packs**: implement their own scoring on top of the same fused candidate set.

This is why orchestration lives in packs, not in `khive-retrieval` — the latter is a
building-block crate (per its DESIGN.md) and doesn't impose a single scoring shape.

## Layering

| Concern                                                                       | Crate                                           | Why                                                 |
| ----------------------------------------------------------------------------- | ----------------------------------------------- | --------------------------------------------------- |
| Multi-engine fan-out (parallel embed + per-engine search + weighted RRF)      | Each pack's handler                             | Verb-specific orchestration; pack autonomy          |
| Per-engine score normalization helpers (`normalize_hits`, `filter_threshold`) | `khive-embed` (helper module)                   | Co-located with `EngineConfig`; shared across packs |
| `FusionStrategy::Weighted` for engine fusion                                  | `khive-fusion` (already exists)                 | One fusion crate for the workspace                  |
| Pre-computed vector embeddings                                                | `khive-embed` (`EmbedderRegistry::embed_*_all`) | Per ADR-081                                         |
| Per-(model, dim) vector tables                                                | `khive-db` (already exists)                     | Per ADR-082 naming                                  |
| Pack-specific scoring (decay, entity-density)                                 | Each pack's scoring module                      | Pack-specific                                       |

`khive-retrieval` remains a building-blocks crate (per its own DESIGN.md). It exposes
`VectorSearch`, `KeywordSearch`, `HybridSearcher`, `Reranker` traits that pack handlers
compose. Multi-engine is a registry of `VectorSearch` implementations the handler chooses
among, not a single `HybridSearcher` impl. `khive-retrieval` does **not** own multi-engine
fan-out.

## Alternatives Considered

### A. `khive-retrieval` owns multi-engine fan-out

Move `MultiEngineSearcher` into `khive-retrieval` so packs invoke one call. Rejected:
`khive-retrieval` per its DESIGN.md is a building-blocks crate; it has no opinion about
pack-specific scoring; forcing all consumers through one orchestration shape removes pack
autonomy. The candidate set produced by fan-out is what `khive-retrieval` should produce;
the scoring on top is the pack's.

### B. `khive-runtime` owns multi-engine fan-out

`runtime.recall_multi_engine(query)` is a method. Rejected: re-introduces embedder ownership
in the runtime — directly contradicting ADR-083. The runtime would need to call
`registry.embed_query_all`, which is the embedding-generation responsibility we removed.

### C. `MultiEngineSearcher` helper in `khive-embed`

Factor the fan-out into a reusable helper. Considered: if multiple packs adopt identical
fan-out shape (parallel embed + per-engine vector_search + weighted RRF), a helper reduces
duplication. v1: defer until duplication is observable. Pack handlers can copy the pattern;
extract when 3+ packs need identical code.

### D. Sequential per-engine fan-out

Embed and search one engine at a time. Rejected: defeats the parallelism that makes
multi-engine cost-acceptable. `tokio::join_all` / `try_join_all` is the right pattern; cost
is 1× wall-time for N engines.

## Consequences

### Positive

- **Multi-engine quality restored** — peer engines, weighted RRF, per-engine normalization
  matches khive-internal's tuned shape
- **Pack autonomy preserved** — each pack chooses its scoring; multi-engine fan-out is a
  pattern, not a single API
- **Failure isolation** — per ADR-083, if one engine fails the others still serve
- **Asymmetric retrieval correct** — `embed_query_all` / `embed_document_all` apply
  per-engine prefixes (ADR-081)

### Negative

- **Embedding cost scales with N engines** — N parallel embeddings per query and per write.
  Mitigation: `embed_query_all` is internally parallel via `tokio::join_all`; per-engine
  cache (each `Embedder` impl wraps `CachedEmbeddingService`-equivalent) amortizes repeated
  queries.
- **Storage cost scales with N engines** — N vector tables. ~150 MB per engine per 100K
  notes at 384d → ~450 MB with 3 peers. Documented; future ADR may introduce
  `write_engines` allowlist (per ADR-082 OQ-2).
- **Pack handler complexity** — `recall` and `search` handlers grow ~50 LOC each for fan-out.
  Acceptable: explicit > magical.

### Neutral

- `khive-fusion` requires no new fusion strategy — `FusionStrategy::Weighted` already exists
- `khive-retrieval` unchanged — adapters consume per-engine tables instead of a singleton
- `khive-fold` / `khive-objectives` orthogonal — fold's Objective composition operates on
  the candidate set after fusion (per ADR-061)
- MCP wire protocol unchanged — verbs see the same parameters; multi-engine is internal to
  handlers

## Open Questions

1. **`MultiEngineSearcher` helper extraction.** Defer until 3+ packs need identical code.
2. **Cross-engine reranker** — should a top-level cross-encoder reranker operate on the
   fused multi-engine candidate set? Orthogonal — handled by ADR-061's reranker stage if/when
   cross-encoders land.
3. **Per-engine result cache.** `embed_query_all` caches per-engine (each `Embedder` impl).
   Should the post-fusion candidate list also be cached at the pack handler? Probably yes
   for memory recall's repeated queries; defer to operational evidence.

## References

- ADR-024 — Cross-substrate search contract (entity/note unified search)
- ADR-036 — Memory pack semantics (decay-weighted scoring layered on fused candidates)
- ADR-061 — Retrieval Infrastructure (Reranker stage composes after this fusion)
- ADR-062 — Recall Pipeline (`recall.score` sub-handler exposes breakdown)
- ADR-078 — Multi-engine umbrella
- ADR-081 — Embedder trait + Registry (provides `embed_*_all` methods)
- ADR-082 — Engine config (weight + normalization parameters)
- ADR-083 — Runtime API (consumes pre-computed vectors)
- ADR-087 — Substrate-kind federated search (different layer — backend-level unweighted RRF)
- khive-internal `.khive/archive/engine_v1/src/engine/embed.rs` — historical multi-model
  write pipeline
- khive-internal `.khive/archive/engine_v1/src/engine/search.rs` — historical multi-model
  search pipeline
- khive-internal `features/memory/src/impl/core.rs` — multiplicative scoring formula
- `khive_fusion` crate — existing fusion strategies including `Weighted`
