# ADR-061: Retrieval Infrastructure — Graph-Aware Multi-Stage Pipeline via Fold

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-058 (Fold Cognitive Primitives)\
**Partially supersedes**: ADR-012 §"Why not a khive-retrieval crate" (retrieval scoring
formalized as Objective implementations; ADR-012's storage/inference split and crate placement
decisions remain in effect)

## Context

ADR-012 split the retrieval stack along the inference/storage boundary: lattice-embed owns
embedding generation, khive owns storage + fusion. It explicitly argued against a `khive-retrieval`
crate because the composition logic was "~250 LOC total" in `khive-runtime`.

Three things have changed since then:

1. **Fold primitives landed (ADR-058).** Retrieval scoring IS an Objective. A retrieval pipeline
   IS a ComposePipeline (Anchor → Objective → Selector). The 250 LOC of composition logic has a
   formal home now, and it's not runtime.

2. **The KG has graph structure.** Entities have edges. A retrieval pipeline that ignores graph
   structure (flat vector + flat FTS → RRF) loses the structural signal that edges carry. A
   graph-aware pipeline uses edge traversal to expand or constrain candidate sets.

3. **Multi-stage retrieval is standard.** The current pipeline is single-stage: embed → search →
   fuse → filter alive → truncate. Production retrieval systems use at least two stages: broad
   recall (cheap, high-recall) → rerank (expensive, high-precision). The fold compose pipeline
   (SequentialFold, DualFold) already supports this.

### What exists today

| Component           | Crate           | LOC | What it does                                                                           |
| ------------------- | --------------- | --- | -------------------------------------------------------------------------------------- |
| `VectorStore` trait | `khive-storage` | 446 | Insert, search, delete, capabilities. sqlite-vec backend (brute-force cosine, O(N·D)). |
| `TextSearch`        | `khive-storage` | —   | FTS5 trigram search.                                                                   |
| `hybrid_search`     | `khive-runtime` | 430 | Embed query → vector search + FTS → RRF fusion → alive filter → truncate.              |
| `FusionStrategy`    | `khive-runtime` | 552 | RRF, Weighted, Union, VectorOnly. 4 strategies, well-tested.                           |
| `rrf_score`         | `khive-score`   | ~50 | Reciprocal Rank Fusion score computation.                                              |
| `rerank`            | `khive-runtime` | ~30 | Post-hoc cosine reranking of a candidate set.                                          |

What's missing: graph-aware retrieval, multi-stage pipelines, retrieval-as-fold integration,
and a scalable vector index path.

## Decision

### 1. Retrieval Objectives in consuming crates

Retrieval scoring functions are Objective implementations (ADR-058) that live in `khive-runtime`,
not in `khive-fold`. The foundation-layer fold crate defines the `Objective<T>` trait and common
strategies (MaxScore, Weighted, etc.); domain-specific objectives belong in the crate that has
the domain knowledge.

This partially supersedes ADR-012's "no khive-retrieval crate" stance: retrieval logic stays in
`khive-runtime` (as ADR-012 decided), but the scoring is now formalized as Objective
implementations rather than ad-hoc arithmetic.

Three retrieval objectives in `khive-runtime`:

```rust
/// Scores candidates by cosine similarity to a query vector.
pub struct VectorSimilarityObjective {
    pub query_embedding: Vec<f32>,
}

/// Scores candidates by BM25/FTS relevance to a query string.
pub struct TextRelevanceObjective {
    pub query_text: String,
    pub mode: TextQueryMode,
}

/// Scores candidates by graph proximity to an anchor set.
/// Score decays with hop distance.
pub struct GraphProximityObjective {
    pub anchor_ids: Vec<Uuid>,
    pub max_hops: usize,
    pub decay: f64,
}
```

These are pure-math: they receive pre-computed data (embeddings, text scores, graph distances)
and produce `f64` scores. They do not perform IO. The runtime layer pre-computes the data and
feeds it in via `ObjectiveContext`.

### 2. Multi-stage retrieval pipeline

A retrieval pipeline is a `SequentialFold` (ADR-058 §4) with two or three stages:

```
Stage 1: Broad recall (cheap, high-recall)
  → Vector search OR FTS, pull N×4 candidates
  → Objective: VectorSimilarityObjective or TextRelevanceObjective
  → No budget constraint (take all above threshold)

Stage 2: Rerank (expensive, high-precision)
  → Feed Stage 1 output as candidate set
  → Objective: WeightedObjective(vector=0.4, text=0.3, graph=0.3)
  → Optional: cross-encoder scoring via lattice-embed (when available)

Stage 3: Select (budget-constrained)
  → Feed Stage 2 output to Selector
  → Budget: token count, byte limit, or candidate count
  → SelectorWeights: diversity_bias for category spread
```

Expressed as fold composition:

```rust
let pipeline = SequentialFold::new(
    broad_recall_fold,   // Stage 1: high-recall retrieval
    rerank_fold,         // Stage 2: precision reranking
    |recall_state, ctx| {
        // Map Stage 1 candidates into Stage 2's context
        let mut ctx2 = ctx.clone();
        ctx2.set_extra("candidates", &recall_state.hits);
        ctx2
    },
);
// Stage 3: Selector applied to Stage 2 output
let selected = selector.select(pipeline_output, budget, &weights)?;
```

Or as a `ComposePipeline` (ADR-058 §7):

```rust
let retrieval = ComposePipeline {
    anchor: Box::new(graph_anchor),      // expand candidate set via graph
    objective: Box::new(WeightedObjective::new(vec![
        (0.4, Box::new(vector_sim)),
        (0.3, Box::new(text_rel)),
        (0.3, Box::new(graph_prox)),
    ])),
    selector: Box::new(GreedySelector),
};
```

### 3. Graph-aware retrieval via Anchor

The `Anchor` trait (ADR-058) traces provenance chains. For retrieval, this means:

**Candidate expansion**: given an anchor set (the query entities), traverse edges to find
related entities that vector/text search would miss. A 1-hop expansion from the query anchor
adds direct neighbors; 2-hop adds neighbors-of-neighbors.

**Score boosting**: candidates found via graph traversal get a proximity boost. The
`GraphProximityObjective` applies a decay function: `score = base_score * decay^hops`.
Direct neighbors (1-hop) get `0.7×`, 2-hop gets `0.49×`, etc.

**Filtering**: edges carry relation types (ADR-002, 13 closed relations). Graph-aware retrieval
can filter by relation: "find entities `introduced_by` the same person" or "find concepts that
`extend` this concept."

This is not a new capability — it's the Anchor primitive from ADR-058 applied to retrieval.
The `AnchorGraph` is materialized from `khive-db` edges before the fold runs.

### 4. Fusion strategies stay in `khive-runtime`

The 4 existing `FusionStrategy` implementations (RRF, Weighted, Union, VectorOnly) remain in
`khive-runtime/src/fusion.rs`. They are runtime-level composition of storage results, not fold
primitives. Fold objectives operate on typed candidates with scores; fusion operates on raw
storage hits before they become typed candidates.

The pipeline flow:

```
khive-storage (raw hits) → fusion (merge lists) → typed candidates → fold objectives (score) → selector (budget)
```

Fusion is the bridge between storage and fold. It doesn't need to move.

### 5. VectorStore: sqlite-vec now, HNSW path documented

Current state: sqlite-vec brute-force cosine, O(N·D) per query. The `VectorStoreCapabilities`
trait (already in khive-storage) reports `index_kinds: vec![SqliteVec]`.

**Scale ceiling**: brute-force works for ~100K vectors at 384 dimensions (mE5-small). Beyond
that, query latency exceeds 100ms on commodity hardware.

**HNSW upgrade path** (deferred, not implemented):

1. `VectorStore` trait already supports multiple backends via `capabilities()`.
2. A future `HnswVectorStore` implementation would report `index_kinds: vec![Hnsw]` and
   `supports_filter: true` (HNSW with metadata filtering).
3. The runtime layer selects the backend based on `VectorStoreCapabilities`. No fold or
   fusion code changes — the improvement is purely at the storage level.
4. Candidate source: lattice already has HNSW concepts in the KG (38 edges, densest hub).
   An in-process Rust HNSW (e.g., `instant-distance` or a custom implementation) would
   slot into the `VectorStore` trait.

Decision: document the ceiling and the upgrade path. Do not implement HNSW now. The current
~1.4K LOC retrieval stack handles the "local research KG" use case. HNSW belongs in a
follow-up ADR when a concrete scale benchmark demonstrates the need.

### 6. HyDE and multi-query retrieval

Hypothetical Document Embedding (HyDE): generate a hypothetical answer via LLM, embed it,
search with that embedding. `VectorStore::search_batch` already supports N query vectors.

HyDE belongs in the **service layer**, not in fold:

- It requires LLM inference (IO, async — violates fold's no-IO invariant)
- The fold layer receives the HyDE embeddings as pre-computed inputs via `ObjectiveContext`
- The runtime layer orchestrates: LLM call → embed hypothetical → feed to ComposePipeline

Multi-query retrieval (multiple reformulations of the same query) uses the same pattern:
generate reformulations (service layer) → embed each → search_batch → fuse → fold.

### 7. Retrieval Hoare triple

Per ADR-058 §6, the retrieval fold documents its Hoare triple:

| Component         | Retrieval instantiation                                                                                                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Precondition**  | Query text/vector provided. Anchor set identified (may be empty for unanchored search). VectorStore and TextSearch backends available.                                                     |
| **Program**       | Stage 1: broad recall via VectorSimilarity + TextRelevance. Stage 2: rerank via Weighted composite. Stage 3: select via GreedySelector under budget.                                       |
| **Postcondition** | Output is a deterministic `SelectorOutput<T>` within budget. Ordering is reproducible (DeterministicObjective with UUID tie-breaking). All returned entities are alive (not soft-deleted). |

## Alternatives Considered

### A. Create a `khive-retrieval` crate

ADR-012 rejected this at 250 LOC. With fold integration, the retrieval-specific code grows to
~500-800 LOC (3 new Objectives + pipeline wiring). Still not enough to justify a new crate.
The Objectives go in `khive-fold`; the pipeline wiring goes in `khive-runtime`.

Rejected. Same reasoning as ADR-012, still holds.

### B. Implement HNSW now

Pros: sub-linear retrieval, scales to millions of vectors. Cons: no demonstrated need. The
current sqlite-vec brute-force handles the target workload (research KGs with ~100K entities).
HNSW adds ~3-5K LOC and a new index maintenance burden (insert, delete, merge). Build it when
a benchmark says brute-force is too slow.

Deferred. Upgrade path documented in §5.

### C. Put graph traversal in the runtime, not in fold

Pros: graph traversal requires database access (IO), which violates fold's no-IO invariant.
Cons: the `Anchor` trait is already pure-math — it operates on a materialized `AnchorGraph`,
not on the database. The runtime materializes the graph, then passes it to fold. The Anchor
primitive is the right abstraction.

Rejected. The split is: runtime materializes, fold traverses. IO stays in runtime.

### D. Replace RRF with learned fusion

Pros: learned weights beat hand-tuned constants. Cons: requires training data and a model.
RRF is a strong baseline (k=60 is well-validated) and zero-config. The `FusionStrategy` enum
already supports `Weighted` for when learned weights are available.

Deferred. RRF as default; `Weighted` as the learned-fusion escape hatch.

## Consequences

### Positive

- **Retrieval is formalized as fold**: `VectorSimilarityObjective`, `TextRelevanceObjective`,
  `GraphProximityObjective` are first-class Objective implementations with Hoare triples
- **Graph-aware retrieval**: Anchor-based candidate expansion using the KG's own edge structure
- **Multi-stage pipelines**: broad recall → rerank → select, expressed as SequentialFold
- **Clear upgrade path**: sqlite-vec → HNSW via VectorStore trait, zero fold/fusion changes
- **HyDE-ready**: search_batch + service-layer orchestration, no primitive changes needed

### Negative

- **Pre-computation burden**: graph distances, text scores, and embeddings must be materialized
  before fold runs. The runtime layer handles this, but it's more orchestration than today's
  single `hybrid_search` call.
- **GraphProximityObjective requires AnchorGraph materialization**: for large graphs, this means
  either limiting max_hops or pre-computing a neighborhood cache. The ceiling is documented but
  not solved.

## Open Questions

1. **Graph traversal caching**: should the runtime cache `neighbors()` results for frequently
   queried anchor sets? Or recompute per query?
2. **Cross-encoder reranking**: when lattice-embed adds cross-encoder support, where does it
   plug in? As a Stage 2 Objective? Or as a separate `RerankerObjective` with its own trait?
3. **Embedding model migration**: ADR-040 (in storage, not this repo) deals with embedding model
   changes. What happens to vector search quality when the model changes mid-graph?
4. **Adaptive stage depth**: should the pipeline skip Stage 2 (rerank) when Stage 1 returns
   fewer than N candidates? Or always run all stages?

## References

- ADR-012: Retrieval Architecture — inference in lattice, storage + fusion in khive
- ADR-058: Fold Cognitive Primitives — Hoare-Structured Decisions
- ADR-059: Bayesian Fold Extensions — precision-weighted objectives
- ADR-006: Deterministic Scoring — `DeterministicScore` and canonical ordering
- ADR-002: Edge Ontology — 13 closed edge relations for graph-aware filtering
- `khive-runtime/src/retrieval.rs` — current hybrid_search implementation (430 LOC)
- `khive-runtime/src/fusion.rs` — current FusionStrategy implementations (552 LOC)
- `khive-storage/src/vectors.rs` — VectorStore trait and capabilities (446 LOC)
- Malkov, Y. & Yashunin, D., "Efficient and robust approximate nearest neighbor using
  Hierarchical Navigable Small World graphs" (2018) — HNSW algorithm
- Croft, W.B., Metzler, D., & Strohman, T., "Search Engines: Information Retrieval in Practice"
