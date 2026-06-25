# ADR-074: Graph-Aware Recall — Graph-Proximity Signal in Memory Retrieval

**Status**: Proposed\
**Date**: 2026-06-25\
**Authors**: Ocean, lambda:khive\
**Measurement evidence**: lattice dogfood loop, delta #1 (2026-06-25)\
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack), [ADR-033](ADR-033-recall-pipeline.md) (Recall Pipeline), [ADR-042](ADR-042-local-rerank-via-lattice-inference.md) (Composable Rerank Pipeline), [ADR-002](ADR-002-edge-ontology.md) (Closed Edge Ontology)\
**GitHub**: #139 (Personalized PageRank), #80 (retrieval coverage metric)

---

## Context

### What ships today

`memory.recall` runs a multi-stage pipeline (ADR-033): FTS5 candidates and vector candidates are fused via RRF, then an optional weighted-feature reranker (`RecallConfig.reranker_weights`) replaces the default scoring when any weight is set. The shipped feature keys for `weighted_rerank` in `crates/khive-pack-memory/src/rerank.rs` (lines 29-37) are exactly five:

| Key            | Source                                             |
| -------------- | -------------------------------------------------- |
| `relevance`    | Fused retrieval score (RRF/weighted fusion output) |
| `salience`     | Decay-adjusted note salience                       |
| `temporal`     | Half-life recency score                            |
| `text_match`   | Boolean: candidate appeared in FTS results         |
| `vector_match` | Boolean: candidate appeared in vector results      |

`graph_proximity` is listed as a planned built-in name in `crates/khive-pack-memory/src/config.rs:53`, but it is not a match arm in `rerank.rs`. A caller who sets `reranker_weights["graph_proximity"]` today gets the key silently ignored (falls through to `_ => continue`).

The typed knowledge-graph edges, the substrate khive was designed around, are not consulted at all during flat recall.

### Two measured gaps (lattice dogfood, delta #1, 2026-06-25)

**RECALL gap.** An actionable memory was reachable only by entity-anchored graph expansion, not by flat top-k. A `search(kind=entity)` on the query term returned an entity directly connected to the target memory via a typed edge. That memory was absent from flat-recall results entirely. This is the sibling-cluster case: the flat retrieval score was below the cutoff, but graph proximity to the query anchor would have surfaced it with +1 recall lift.

**RANK gap.** A second actionable memory (a trap warning relevant to the active work) was surfaced by flat recall at rank #2 with a raw fusion score of 0.475. The entity anchoring the active work had a direct `depends_on` edge to the entity that memory annotated. Graph-proximity signal would have promoted this memory to rank #1. The flat recall result was not wrong; it was deprioritized enough to risk being skipped.

Neither gap was resolved by tuning the existing five feature weights. The graph topology carries independent information that lexical and dense recall do not encode.

---

## Decision

### 1. Extend `weighted_rerank` with a `graph_proximity` feature key

This is the ADR-033/042 aligned move: extend the existing weighted-feature reranker with a sixth key rather than bolt on a separate pipeline stage. Adding `graph_proximity` to `crates/khive-pack-memory/src/rerank.rs` requires adding one match arm and plumbing a pre-computed proximity score into `RerankFeatures`. No new verb, no new pipeline branch, no schema change.

The REPLACE semantics from ADR-033 §6.2 and ADR-042 §1 remain in effect: when `reranker_weights` is non-empty, the weighted combination of whichever keys are set becomes the final score. A caller enabling graph proximity must also weight the other signals they care about. A minimal graph-aware config looks like:

```json
{
  "reranker_weights": {
    "relevance": 0.6,
    "graph_proximity": 0.3,
    "salience": 0.1
  }
}
```

### 2. Pipeline shape for the graph-proximity feature

The proximity score is computed in a pre-rerank step added to the `memory.recall` handler in `crates/khive-pack-memory/src/handlers/recall.rs`. The step runs only when `reranker_weights["graph_proximity"] > 0`.

**Step A: Anchor.** Derive a set of anchor entity IDs from the query. The primary path is a `search(kind=entity, query=<recall_query>)` call on the runtime, returning the top-N entity hits. An alternative path accepts caller-supplied anchor IDs via a `reranker_params["graph_proximity"]["anchor_ids"]` list (ADR-042 §7 `reranker_params` mechanism). Both paths may be combined.

**Step B: Expand.** For each anchor, call the existing `neighbors` or `traverse` primitives in `crates/khive-runtime/src/operations.rs` (lines 1521 and 1585 respectively) with a configurable hop budget (default: 2 hops, direction: both). The expansion uses the full closed edge ontology from ADR-002 with no relation filter by default; optional `relation` filtering mirrors the existing `neighbors_with_query` interface.

**Step C: Score proximity.** Assign each expanded node a proximity score using a decaying function of hop distance and edge weight:

```text
proximity(node, hop_distance, edge_weight) = edge_weight / (1 + hop_distance)
```

When a node is reachable by multiple paths, keep the maximum proximity score. The score range is (0, 1] for a single-hop edge of weight 1.0 and decays with distance and lower edge weights.

Optionally, a Personalized PageRank (PPR) seeded at the anchors provides a more principled proximity measure that accounts for graph structure (GitHub #139). PPR and the hop-decay formula are interchangeable at the config layer; the default is hop-decay as it requires no additional computation beyond the existing `traverse` call.

**Step D: Propagate to memories.** Map proximity scores from entity nodes to memory notes. A memory note that has an `annotates` edge pointing to an entity node inherits that entity's proximity score. A memory note reachable in the traversal via any graph path inherits the proximity score of the closest anchor-connected node on that path. Notes with no graph connection to any anchor receive a proximity score of 0.0.

**Step E: Feed into weighted_rerank.** The proximity score is passed as `features.graph_proximity` into `RerankFeatures` and handled by the new match arm in `weighted_rerank`. The REPLACE semantics remain: the full `reranker_weights` map determines the final score.

### 3. Measurement protocol

Graph-recall value is measured on two axes per loop iteration. Subjective assessments ("cycles saved", "felt more relevant") are not acceptable measurements.

**Axis 1: RECALL-lift.** For each actionable item identified post-session:

- `in_flat_recall`: boolean, was this item in the flat-recall top-k?
- `found_via_graph`: boolean, was this item reachable only via graph expansion?

An item where `in_flat_recall = false` and `found_via_graph = true` is a graph-exclusive recall. The running fraction of such items over total actionable items is the recall-lift figure.

**Axis 2: RANK-lift.** For actionable items already in flat recall, record the rank position with and without graph-proximity weighting. A positive rank shift (lower rank number) when graph proximity is enabled is a rank-lift event.

The lattice loop logs the boolean pair plus rank for each delta into the `khive-runs` dataset. GitHub #80 (retrieval coverage metric, currently at 0.0) is the measurement accumulation point. The headline parity number — the fraction of sessions where graph recall outperformed flat recall on at least one axis — is a TODO pending lattice accumulation.

### 4. Default-on gate

Graph fusion becomes a default recall feature only after measured recall-parity evidence at scale. Until that threshold is established, the feature ships opt-in: `graph_proximity` is not present in the default `RecallConfig.reranker_weights` (which defaults to `HashMap::new()`). Callers who want graph-aware recall add the key explicitly or configure it in pack settings. The parity evidence and Ocean sign-off are both required to flip the default.

---

## Alternatives Considered

| Alternative                                                                 | Why rejected                                                                                                                                                                                                                                                         |
| --------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status quo flat recall                                                      | Leaves the graph entirely unused; both measured gaps persist.                                                                                                                                                                                                        |
| Separate `recall(mode=graph)` pipeline                                      | Duplicates the candidate fusion path; two parallel pipelines to maintain; single fusion point is cleaner and consistent with ADR-033's architecture.                                                                                                                 |
| Offline precomputed graph/node embeddings                                   | Staleness: the live KG is mutable; precomputed scores are stale by construction. Live traversal via existing `neighbors`/`traverse` is simpler and always current.                                                                                                   |
| Pure graph-only ranking                                                     | The RANK gap case showed that flat recall still surfaced the correct item; the graph signal was needed for ordering, not discovery alone. Dropping lexical/dense recall entirely would regress discovery quality.                                                    |
| New `graph_proximity` reranker trait object (ADR-042 §7 native model class) | The hop-decay formula is arithmetic, not a model. A full `Reranker` trait object adds interface surface for a formula that fits inside one match arm. ADR-042 §7 explicitly reserves the model-class path for cross-encoders and similar inference-backed rerankers. |

---

## Open Questions

Acceptance is pending the lattice parity measurements and Ocean sign-off. These questions shape implementation details and are not resolved by this ADR.

1. **Fusion weights.** What ratio of `graph_proximity` to `relevance` performs best across the dogfood corpus? The measurement protocol accumulates this signal; the shipped default config (if any) should come from measured evidence.
2. **Hop budget.** Is a 2-hop expansion the right default? Wider expansion improves recall at the cost of proximity score dilution for distant nodes.
3. **PPR vs. hop-decay.** Personalized PageRank gives a principled global proximity measure that is resistant to hub nodes inflating scores. The tradeoff is implementation complexity versus the simple hop-decay formula. Decision deferred to when the measurement infrastructure is in place.
4. **Anchor source.** Should anchors come from query entity-search only, or should the `memory.recall` handler also accept an explicit active-context entity list from the caller? The latter is more precise but requires callers to maintain and pass their active context.
5. **Opt-in flag vs. always-on with zero default weight.** A `graph_proximity` key in `reranker_weights` at weight 0.0 is a no-op today (the zero-weight branch in `weighted_rerank` skips the key). Shipping the feature as always-present in `RerankFeatures` with zero default weight is equivalent to opt-in but makes the feature visible in score breakdowns at no cost. This is preferred for observability.

---

## Consequences

### Positive

- The typed KG edges participate in recall for the first time, surfacing memories that flat lexical/dense retrieval cannot reach.
- The extension uses the existing `weighted_rerank` framework (ADR-033/042) rather than introducing a new pipeline. Implementation scope is bounded to one new match arm in `rerank.rs`, one pre-rerank step in the recall handler, and a proximity scorer.
- Measurement is defined rigorously before implementation. The recall-lift and rank-lift axes are trackable per session with no additional instrumentation beyond what the lattice loop already logs.
- The `reranker_params` mechanism from ADR-042 §7 provides the configuration path for anchor IDs and PPR parameters without touching the `RecallConfig` struct.

### Negative

- The pre-rerank step adds graph expansion to the recall hot path when enabled. Each `recall` call with `graph_proximity > 0` triggers at least one `neighbors` or `traverse` call. Latency impact is bounded by the hop budget but is not zero.
- Callers using `reranker_weights` must now set all desired feature weights together (REPLACE semantics). Adding `graph_proximity` without also setting `relevance` and `salience` weights produces a scoring formula dominated by graph proximity alone. This is not a behavior regression — the REPLACE contract is documented in ADR-033 §6.2 — but it increases the cognitive load on callers who configure the reranker.
- The default-on gate means the feature ships as opt-in for an indeterminate period until parity is established. Callers who want graph-aware recall must configure it explicitly.

### Neutral

- No schema migration. No DDL change. No new edge relation. No new entity kind. No new verb.
- The `graph_proximity` key added to the `weighted_rerank` match arm is forward-compatible: callers who set the key on an older binary receive the silent-ignore behavior described in ADR-033 §6.2 ("unknown keys are silently ignored for forward-compat").
- The proximity scorer is a pure function of the traversal results. It does not depend on embedding models, brain profiles, or any external state beyond the KG graph store.

---

## References

- [ADR-002](ADR-002-edge-ontology.md): Closed edge ontology — the 17 typed relations traversed during expansion
- [ADR-021](ADR-021-memory-pack.md): Memory pack — `annotates` edge from memory note to source entity, the primary propagation path
- [ADR-033](ADR-033-recall-pipeline.md): Recall pipeline — `RecallConfig.reranker_weights`, REPLACE semantics, five shipped feature keys
- [ADR-042](ADR-042-local-rerank-via-lattice-inference.md): Composable rerank pipeline — `reranker_params` mechanism, `graph_proximity` listed as a planned built-in name in §7
- GitHub #139: Personalized PageRank — alternative proximity scorer to the hop-decay formula
- GitHub #80: Retrieval coverage metric — the measurement accumulation point for recall-lift parity figures
- `crates/khive-pack-memory/src/rerank.rs`: `weighted_rerank` function; the new `graph_proximity` match arm lands here
- `crates/khive-pack-memory/src/handlers/recall.rs`: the pre-rerank proximity step lands here
- `crates/khive-runtime/src/operations.rs` (lines 1521, 1585, 1670): `neighbors`, `traverse`, `enrich_neighbor_hits` — the graph expansion primitives reused by the proximity step
