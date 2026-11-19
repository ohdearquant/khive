# ADR-074: Graph-Aware Recall — Graph-Proximity Signal in Memory Retrieval

**Status**: Proposed\
**Date**: 2026-06-25\
**Authors**: khive maintainers
**Measurement evidence**: lattice dogfood loop, delta #1 (2026-06-25)\
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack), [ADR-033](ADR-033-recall-pipeline.md) (Recall Pipeline), [ADR-042](ADR-042-local-rerank-via-lattice-inference.md) (Composable Rerank Pipeline), [ADR-002](ADR-002-edge-ontology.md) (Closed Edge Ontology)\
**GitHub**: #139 (Personalized PageRank), #80 (closed retrieval coverage metric), #5555 (graph proximity), #5984
(deferred caller anchors) (corrected by Amendment 1)

---

## Context

### What ships today

`memory.recall` runs a multi-stage pipeline (ADR-033): FTS5 candidates and vector candidates are fused via RRF, then an
optional weighted-feature reranker (`RecallConfig.reranker_weights`) replaces the default scoring when the map is
non-empty (corrected by Amendment 1). The shipped feature keys for `weighted_rerank` in
`crates/khive-pack-memory/src/rerank.rs` (the fixed-order `weighted_features` array inside `weighted_rerank`) are
exactly six:

| Key               | Source                                                   |
| ----------------- | -------------------------------------------------------- |
| `relevance`       | Fused retrieval score (RRF/weighted fusion output)       |
| `salience`        | Decay-adjusted note salience                             |
| `temporal`        | Half-life recency score                                  |
| `text_match`      | Boolean: candidate appeared in FTS results               |
| `vector_match`    | Boolean: candidate appeared in vector results            |
| `graph_proximity` | Precomputed graph proximity; recall handlers supply zero |

`graph_proximity` is one of the six keys `weighted_rerank` looks up (corrected by Amendment 1). It is listed as a
built-in name in `crates/khive-pack-memory/src/config.rs` (the doc comment on `RecallConfig::reranker_weights`).
A caller who sets `reranker_weights["graph_proximity"]` to a positive value gets the key included in the fixed-order
`weighted_features` array in `rerank.rs`, so it contributes to normalization even though the recall handlers supply a
zero feature value (corrected by Amendment 1).

The typed knowledge-graph edges, the substrate khive was designed around, are consulted during flat recall for incoming
`supersedes` suppression and optional `annotates` source provenance, but not for graph-proximity scoring (corrected by
Amendment 1).

### Two measured gaps (lattice dogfood, delta #1, 2026-06-25)

**RECALL gap.** An actionable memory was reachable only by entity-anchored graph expansion, not by flat top-k. A `search(kind=entity)` on the query term returned an entity directly connected to the target memory via a typed edge. That memory was absent from flat-recall results entirely. This is the sibling-cluster case: the flat retrieval score was below the cutoff, but graph proximity to the query anchor would have surfaced it with +1 recall lift.

**RANK gap.** A second actionable memory (a trap warning relevant to the active work) was surfaced by flat recall at rank #2 with a raw fusion score of 0.475. The entity anchoring the active work had a direct `depends_on` edge to the entity that memory annotated. Graph-proximity signal would have promoted this memory to rank #1. The flat recall result was not wrong; it was deprioritized enough to risk being skipped.

Neither gap was resolved by tuning the existing five feature weights. The graph topology carries independent information that lexical and dense recall do not encode.

---

## Decision

### 1. Extend `weighted_rerank` with a `graph_proximity` feature key

This is the ADR-033/042 aligned move: extend the existing weighted-feature reranker with a sixth key rather than bolt on
a separate pipeline stage. The `graph_proximity` entry already exists in the fixed-order `weighted_features` array in
`crates/khive-pack-memory/src/rerank.rs`; plumbing a pre-computed proximity score into `RerankFeatures` remains
unimplemented in recall (corrected by Amendment 1). No new verb, no new pipeline branch, no schema change.

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

**Step A: Anchor.** Derive a set of anchor entity IDs from the query. The primary path is a `search(kind=entity,
query=<recall_query>)` call on the runtime, returning the top-N entity hits. An alternative path accepts caller-supplied
anchor IDs via a `reranker_params["graph_proximity"]["anchor_ids"]` list (ADR-042 §7 `reranker_params` mechanism). Both
paths may be combined. (superseded by Amendment 1, Decision 1)

**Step B: Expand.** For each anchor, call the existing `neighbors` or `traverse` primitives in
`crates/khive-runtime/src/operations/traversal.rs:216` and `crates/khive-runtime/src/operations/traversal.rs:532`,
respectively (corrected by Amendment 1), with a configurable hop budget (default: 2 hops, direction: both). The
expansion uses the full closed edge ontology from ADR-002 with no relation filter by default; optional `relation`
filtering mirrors the existing `neighbors_with_query` interface.

**Step C: Score proximity.** Assign each expanded node a proximity score using a decaying function of hop distance and edge weight:

```text
proximity(node, hop_distance, edge_weight) = edge_weight / (1 + hop_distance)
```

When a node is reachable by multiple paths, keep the maximum proximity score. The score is 0.5 for a single-hop edge of
weight 1.0 and decays with distance and lower edge weights (corrected by Amendment 1).

Optionally, a Personalized PageRank (PPR) seeded at the anchors provides a more principled proximity measure that
accounts for graph structure (GitHub #139). PPR and the hop-decay formula are interchangeable at the config layer; the
default is hop-decay and requires a scorer over graph expansion results beyond the existing `traverse` call (corrected
by Amendment 1, Decision 3).

**Step D: Propagate to memories.** Map proximity scores from entity nodes to memory notes. A memory note that has an
`annotates` edge pointing to an entity node inherits that entity's proximity score. A memory note reachable in the
traversal via any graph path inherits the proximity score of the closest anchor-connected node on that path. Notes with
no graph connection to any anchor receive a proximity score of 0.0. (Precedence and the path maximum are stated by
Amendment 1, Decision 4.)

**Step E: Feed into weighted_rerank.** The proximity score is passed as `features.graph_proximity` into `RerankFeatures`
and handled by the existing fixed-order array entry in `weighted_rerank` (corrected by Amendment 1). The REPLACE
semantics remain: the full `reranker_weights` map determines the final score.

### 3. Measurement protocol

Graph-recall value is measured on two axes per loop iteration. Subjective assessments ("cycles saved", "felt more relevant") are not acceptable measurements.

**Axis 1: RECALL-lift.** For each actionable item identified post-session:

- `in_flat_recall`: boolean, was this item in the flat-recall top-k?
- `found_via_graph`: boolean, was this item reachable only via graph expansion?

An item where `in_flat_recall = false` and `found_via_graph = true` is a graph-exclusive recall. The running fraction of such items over total actionable items is the recall-lift figure.

**Axis 2: RANK-lift.** For actionable items already in flat recall, record the rank position with and without graph-proximity weighting. A positive rank shift (lower rank number) when graph proximity is enabled is a rank-lift event.

The lattice loop logs the boolean pair plus rank for each delta into the `khive-runs` dataset. GitHub #80 (retrieval
coverage metric) is closed and retained as historical measurement context (corrected by Amendment 1). The headline
parity number, the fraction of sessions where graph recall outperformed flat recall on at least one axis, is a TODO
pending lattice accumulation.

**Measurement evidence (accumulating).** Two discovery iterations are logged so far. They point in opposite directions, which is the intended honest signal rather than a problem.

| Delta   | Discovery sweep      | Actionable prior                                              | recall-lift                                                  | rank-lift                            | Outcome                     |
| ------- | -------------------- | ------------------------------------------------------------- | ------------------------------------------------------------ | ------------------------------------ | --------------------------- |
| delta-1 | `flash.rs` prior     | old, salience 0.72, flat rank #2 at raw 0.475                 | +1/2 (sibling cluster surfaced only via graph)               | +1 (graph promotes rank #2 to #1)    | graph fusion wins           |
| delta-2 | `standard.rs` kernel | fresh session lessons, salience 0.85, decay 0.0, flat rank #1 | low, about +0.25 (entity leg adds marginal taxonomy framing) | about 0 (flat #1 already actionable) | flat plus salience suffices |

The lift is conditional on the corpus. Graph proximity adds the most when the actionable prior is old, low-salience, or poorly ranked by raw cosine, which is exactly when flat retrieval under-ranks it. When the actionable priors are recent high-salience captures, the existing `salience` and `temporal` features already rank them first and graph proximity adds little. delta-2 is the control row that establishes this boundary, and it is logged precisely because flat recall won it.

### 4. Default-on gate

Graph fusion becomes a default recall feature only after measured recall-parity evidence at scale. The delta-1 and
delta-2 split above shows the default question is not binary: graph proximity earns default-on for corpora where stale,
low-salience priors still carry actionable weight, and adds little where recent high-salience captures dominate. A scale
measurement therefore reports the distribution of lift across corpus age and salience, not a single average. Until that
threshold is established, the feature ships opt-in: `graph_proximity` is not present in the default
`RecallConfig.reranker_weights` (which defaults to `HashMap::new()`). Callers who want graph-aware recall add the key
explicitly or configure it in pack settings. The parity evidence and maintainer approval are both required to flip the
default. (superseded by Amendment 1, Decision 2)

---

## Alternatives Considered

| Alternative                                                                 | Why rejected                                                                                                                                                                                                                                                                      |
| --------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status quo flat recall                                                      | Leaves graph proximity unused in scoring (corrected by Amendment 1); both measured gaps persist.                                                                                                                                                                                  |
| Separate `recall(mode=graph)` pipeline                                      | Duplicates the candidate fusion path; two parallel pipelines to maintain; single fusion point is cleaner and consistent with ADR-033's architecture.                                                                                                                              |
| Offline precomputed graph/node embeddings                                   | Staleness: the live KG is mutable; precomputed scores are stale by construction. Live traversal via existing `neighbors`/`traverse` is simpler and always current.                                                                                                                |
| Pure graph-only ranking                                                     | The RANK gap case showed that flat recall still surfaced the correct item; the graph signal was needed for ordering, not discovery alone. Dropping lexical/dense recall entirely would regress discovery quality.                                                                 |
| New `graph_proximity` reranker trait object (ADR-042 §7 native model class) | The hop-decay formula is arithmetic, not a model. A full `Reranker` trait object adds interface surface for a computed feature supported by the fixed-order feature array (corrected by Amendment 1). ADR-042 §7 defers the native reranker interface (corrected by Amendment 1). |

---

## Open Questions

Acceptance is pending the lattice parity measurements and maintainer approval. (superseded by Amendment 1, Decision 2)
These questions shape implementation details and are not resolved by this ADR (questions 3, 4 and 5 are answered by
Amendment 1, Decisions 3, 1 and 2).

1. **Fusion weights.** What ratio of `graph_proximity` to `relevance` performs best across the dogfood corpus? The measurement protocol accumulates this signal; the shipped default config (if any) should come from measured evidence.
2. **Hop budget.** Is a 2-hop expansion the right default? Wider expansion improves recall at the cost of proximity score dilution for distant nodes.
3. **PPR vs. hop-decay.** Personalized PageRank gives a principled global proximity measure that is resistant to hub
   nodes inflating scores. The tradeoff is implementation complexity versus the simple hop-decay formula. Decision
   deferred to when the measurement infrastructure is in place. (superseded by Amendment 1, Decision 3)
4. **Anchor source.** Should anchors come from query entity-search only, or should the `memory.recall` handler also
   accept an explicit active-context entity list from the caller? The latter is more precise but requires callers to
   maintain and pass their active context. (superseded by Amendment 1, Decision 1)
5. **Opt-in flag vs. always-on with zero default weight.** A `graph_proximity` key in `reranker_weights` at weight 0.0
   is skipped by `weighted_rerank`, but a map containing only that entry selects weighted reranking and zeroes every
   candidate score instead of preserving the empty-map scoring path (corrected by Amendment 1). Shipping the feature as
   always-present in `RerankFeatures` with zero default weight is equivalent to opt-in but makes the feature visible in
   score breakdowns at no cost. This is preferred for observability. (superseded by Amendment 1, Decision 2)

---

## Consequences

### Positive

- Graph-proximity scoring would extend the existing use of typed KG edges for suppression and source provenance
  (corrected by Amendment 1), surfacing memories that flat lexical/dense retrieval cannot reach.
- The extension uses the existing `weighted_rerank` framework (ADR-033/042) rather than introducing a new pipeline. The
  fixed-order feature entry has shipped; the remaining implementation scope is one pre-rerank step in the recall handler
  and a proximity scorer (corrected by Amendment 1).
- Measurement is defined rigorously before implementation. The recall-lift and rank-lift axes are trackable per session with no additional instrumentation beyond what the lattice loop already logs.
- The `reranker_params` mechanism from ADR-042 §7 describes a deferred configuration path for anchor IDs and PPR
  parameters that would add a field to the `RecallConfig` struct (corrected by Amendment 1). Caller-supplied anchors are
  deferred out of v1 (superseded by Amendment 1, Decision 1).

### Negative

- The pre-rerank step adds graph expansion to the recall hot path when enabled. Each `recall` call with `graph_proximity > 0` triggers at least one `neighbors` or `traverse` call. Latency impact is bounded by the hop budget but is not zero.
- Callers using `reranker_weights` must now set all desired feature weights together (REPLACE semantics). A positive
  graph-only weight map currently produces zero scores because the recall handlers supply a zero graph feature; after
  graph gathering is implemented, that map will use graph proximity alone (corrected by Amendment 1). This is not a
  behavior regression — the REPLACE contract is documented in ADR-033 §6.2 — but it increases the cognitive load on
  callers who configure the reranker.
- The default-on gate means the feature ships as opt-in for an indeterminate period until parity is established. Callers who want graph-aware recall must configure it explicitly.

### Neutral

- No schema migration. No DDL change. No new edge relation. No new entity kind. No new verb.
- The `graph_proximity` key added to the `weighted_rerank` fixed-order feature array (corrected by Amendment 1) is
  forward-compatible: callers who set the key on an older binary receive the silent-ignore behavior described in ADR-033
  §6.2 ("unknown keys are silently ignored for forward-compat").
- The proximity scorer is a pure function of the traversal results. It does not depend on embedding models, brain profiles, or any external state beyond the KG graph store.

---

## References

- [ADR-002](ADR-002-edge-ontology.md): Closed edge ontology — the 20 typed relations in the set amended through ADR-197, traversed during expansion
- [ADR-021](ADR-021-memory-pack.md): Memory pack — `annotates` edge from memory note to source entity, the primary propagation path
- [ADR-033](ADR-033-recall-pipeline.md): Recall pipeline — `RecallConfig.reranker_weights`, REPLACE semantics, six
  shipped feature keys (corrected by Amendment 1)
- [ADR-042](ADR-042-local-rerank-via-lattice-inference.md): Composable rerank pipeline — deferred `reranker_params`
  mechanism (corrected by Amendment 1), `graph_proximity` listed as a planned built-in name in §7
- GitHub #139: Personalized PageRank — alternative proximity scorer to the hop-decay formula
- GitHub #80: Retrieval coverage metric — closed issue retained as historical measurement context (corrected by
  Amendment 1)
- `crates/khive-pack-memory/src/rerank.rs:29`: `weighted_rerank` function; the `graph_proximity` array entry ships at
  `crates/khive-pack-memory/src/rerank.rs:52` (corrected by Amendment 1)
- `crates/khive-pack-memory/src/handlers/recall.rs`: the pre-rerank proximity step lands here
- `crates/khive-runtime/src/operations/traversal.rs:216`, `crates/khive-runtime/src/operations/traversal.rs:532`, and
  `crates/khive-runtime/src/operations/traversal.rs:716` (corrected by Amendment 1): `KhiveRuntime::neighbors`,
  `KhiveRuntime::traverse`, `KhiveRuntime::enrich_neighbor_hits` — the graph expansion primitives reused by the
  proximity step

---

## Amendment 1 (2026-10-23) -- Query-derived graph proximity and default-on acceptance

**Status**: Accepted 2026-10-23. It does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#5555](https://github.com/ohdearquant/khive/issues/5555),
[#5984](https://github.com/ohdearquant/khive/issues/5984).

**Trigger.** The scorer recognizes six features, including `graph_proximity`, but recall still supplies zero
(`crates/khive-pack-memory/src/rerank.rs:30`, `crates/khive-pack-memory/src/handlers/recall.rs:991`). Caller anchors
name an absent request carrier: the native interface is deferred in
`docs/adr/ADR-042-local-rerank-via-lattice-inference.md:235`, while `RecallConfig` contains only the weight map at
`crates/khive-pack-memory/src/config.rs:55`. Graph gathering, its scorer and the default-on gate therefore need distinct
contracts. Evidence locations below refer to source revision `6b3a892612b34244683974658a04070445ff6a77`.

### Decision 1 -- Query-derived anchors only in v1

Step A uses only entity IDs derived from `search(kind=entity, query=<recall_query>)`. Caller-supplied `anchor_ids` are
deferred out of v1. The caller-anchor portion of #5555, item (b), is split into #5984, which depends on the deferred
request carrier in ADR-042 section 7 (`docs/adr/ADR-042-local-rerank-via-lattice-inference.md:235`). Malformed
caller-anchor validation belongs to that deferred contract.

The reason is interface availability. `RecallParams` has no caller-anchor field
(`crates/khive-pack-memory/src/handlers/common.rs:479`), and `RecallConfig` has no `reranker_params` field
(`crates/khive-pack-memory/src/config.rs:43`). ADR-042 places that proposed field within the deferred native interface
(`docs/adr/ADR-042-local-rerank-via-lattice-inference.md:271`). V1 does not introduce that carrier or combine caller
anchors with query anchors. The top-N entity-search limit remains unspecified; this amendment does not choose it.

### Decision 2 -- Parity gates default-on, not default-off implementation

The with-graph measurement arms in section 3 are not acceptance requirements for landing the graph step with its default
weight disabled. Specification approval and the behavioral acceptance rows below govern that implementation. Turning the
default weight above zero needs the measured delta and specification-gate approval. The delta includes recall/rank lift
and its distribution across corpus age and salience specified in section 4. Historical delta-1 and delta-2 rows do not
by themselves establish this gate. No numerical parity threshold is introduced here.

Keep the default `reranker_weights` map empty, as at `crates/khive-pack-memory/src/config.rs:369`. An absent graph key
has effective weight zero. The graph-proximity step runs only for a strictly positive graph weight. Its disabled state
does not prohibit the existing supersedes and source-provenance reads
(`crates/khive-pack-memory/src/handlers/recall.rs:1094`, `crates/khive-pack-memory/src/handlers/recall.rs:1159`).

A stored zero-only map is not a substitute for the empty default. Recall selects weighted reranking on
`!cfg.reranker_weights.is_empty()` (`crates/khive-pack-memory/src/handlers/recall.rs:984`), and the scorer returns zero
when all recognized weights are zero (`crates/khive-pack-memory/src/rerank.rs:61`). The positive-weight gate applies to
the proposed graph-proximity step; this amendment does not replace the existing REPLACE dispatch rule. This distinction
preserves the default scoring path while allowing explicit weighted configurations.

### Decision 3 -- Use hop decay; separate the exported objective's retirement

The graph step uses `edge_weight / (1 + hop_distance)`. It does not reuse `GraphProximityObjective`. That objective
consumes only `graph_distance` and computes linear decay, `1.0 - d / max_distance`, with zero at and beyond the distance
limit (`crates/khive-runtime/src/objectives.rs:98`, `crates/khive-runtime/src/objectives.rs:102`,
`crates/khive-runtime/src/objectives.rs:105`). It does not consume an edge weight, so substituting it would change the
score contract.

At weight 1.0, the chosen formula yields 0.5 at hop 1 and 1/3 at hop 2. Keep the maximum score when an entity is reached
through multiple paths. The existing objective has no production caller in the repository; its concrete uses are tests,
including `graph_midpoint_scores_half` and `weighted_composition_with_graph`
(`crates/khive-runtime/src/objectives_tests.rs:90`, `crates/khive-runtime/src/objectives_tests.rs:156`). It remains a
public export at `crates/khive-runtime/src/lib.rs:208`. Retirement of `GraphProximityObjective` is a separate follow-up
change; this graph step neither removes it nor changes its public behavior.

The traversal result cannot be treated as a ready-made implementation of this formula. Storage skips repeat visits and
stores cumulative path weight (`crates/khive-db/src/stores/graph/traversal.rs:213`,
`crates/khive-db/src/stores/graph/traversal.rs:218`, `crates/khive-db/src/stores/graph/traversal.rs:226`). Those values
do not establish maximum-over-paths scoring. The definition of `edge_weight` for a multi-edge path remains open;
cumulative weight is not adopted implicitly.

### Decision 4 -- Propagation precedence and corrected implementation facts

Apply Step D in this order. First, a memory with `annotates` links to anchor-connected entities inherits the maximum of
those entities' proximity scores. Annotation inheritance takes precedence over generic reachability, even if a generic
path would assign a larger score. When no annotation inheritance applies, evaluate the generic paths described in Step D
and take the maximum inherited proximity score over those paths. A memory with no connection to any anchor receives 0.0.
Path enumeration order must not select a lower score. This makes annotation inheritance primary and resolves the earlier
conflict between "closest" and "maximum" without adding a new aggregation formula.

For a unit-weight edge from anchor A to entity B, B scores 0.5. A memory M with `M annotates B` inherits 0.5, even
though M is two graph hops from A. An otherwise equal, unconnected memory scores 0.0 for this feature. With a positive
graph weight, M must rank above that unconnected memory. The extra annotation hop does not replace B's inherited score
with M's direct two-hop score. The shipped `annotates` read is only source provenance after ranking
(`crates/khive-pack-memory/src/handlers/recall.rs:1152`); inheritance remains a required implementation behavior.

The factual corrections in the base text describe the existing scorer, not completed graph gathering. There are six keys
in the fixed-order array, not five keys or a new match arm (`crates/khive-pack-memory/src/rerank.rs:30`). Positive graph
weight enters normalization with a zero feature (`crates/khive-pack-memory/src/rerank.rs:71`). Thus relevance 0.5 with
weights `relevance: 0.6` and `graph_proximity: 0.4` yields 0.3 when graph proximity is zero, and 0.7 when it is 1.0, as
documented in `crates/khive-pack-memory/docs/api/scoring.md:83`. A graph-only positive-weight configuration returns zero
through the shipped handlers. With a zero-only map, the same zero scores reach the final ordering and are tied by UUID
(`crates/khive-pack-memory/src/handlers/recall.rs:1128`). Other eligibility and suppression filters still apply.

The graph expansion entry points are `neighbors` at `crates/khive-runtime/src/operations/traversal.rs:216`,
`neighbors_with_query` at `crates/khive-runtime/src/operations/traversal.rs:246`, and `traverse` at
`crates/khive-runtime/src/operations/traversal.rs:532`. These replace the incorrect references into `operations.rs`. The
current implementation locations for #5555 are the weighted calls at
`crates/khive-pack-memory/src/handlers/recall.rs:993` and `crates/khive-pack-memory/src/handlers/sub_handlers.rs:418`,
and the objective declaration at `crates/khive-runtime/src/objectives.rs:91`. GitHub #80 is closed; it is retained as
historical context rather than an active accumulation point.

### Acceptance

These rows distinguish existing source evidence from proposed tests. Proposed integration tests belong in
`crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs`; they are requirements, not claims of
passing tests. The default-on evidence is recorded with #5555 and section 4 before any positive default is enabled.

| ID  | Statement                                                                                                                                                  | Evidence                                                                                                                                                                                                                                                                                                                                                                                  |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1  | V1 derives anchors from the recall query without a caller-anchor carrier.                                                                                  | new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_anchors_come_from_query` (to add).                                                                                                                                                                                                                                                        |
| A2  | An absent or zero graph weight causes no proximity anchor search or expansion; existing suppression/provenance reads are permitted.                        | new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_proximity_disabled_skips_gathering` (to add), counting calls for the proximity step.                                                                                                                                                                                                      |
| A3  | A positive graph weight ranks a two-hop annotating memory above an otherwise equal unconnected memory.                                                     | new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_annotation_promotes_two_hop_memory` (to add), using A, B and M from Decision 4.                                                                                                                                                                                                           |
| A4  | Annotation inheritance takes precedence over a larger generic-path score; multiple applicable paths use their maximum independently of enumeration order.  | new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_annotation_precedence_and_path_maximum` (to add), including reversed path order and generic fallback.                                                                                                                                                                                     |
| A5  | Unit edge weight scores 0.5 at hop 1 and 1/3 at hop 2; an unconnected memory scores zero.                                                                  | new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_hop_decay_values` (to add).                                                                                                                                                                                                                                                               |
| A6  | The recognized graph feature participates in normalized weighted scoring.                                                                                  | `crates/khive-pack-memory/src/rerank_tests.rs::graph_proximity_combines_with_relevance`; new test `crates/khive-pack-memory/src/rerank_tests.rs::graph_zero_feature_changes_normalization` (to add) for the 0.3 case.                                                                                                                                                                     |
| A7  | The empty default map preserves baseline recall ordering.                                                                                                  | `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::test_recall_with_empty_reranker_weights_is_passthrough`, assertion at `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs:53`; `crates/khive-pack-memory/src/config.rs:369`.                                                                                                 |
| A8  | Zero-only weights are documented separately from an empty map and do not silently become the default.                                                      | `crates/khive-pack-memory/src/handlers/recall.rs:984`; `crates/khive-pack-memory/src/rerank_tests.rs::zero_and_unknown_weights_only_remain_zero`; new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_zero_only_map_is_not_passthrough` (to add).                                                                                             |
| A9  | The graph step uses hop decay without calling or retiring the exported linear objective.                                                                   | new test `crates/khive-pack-memory/tests/integration_tests/reranker_integration_tests.rs::graph_step_uses_hop_decay_not_linear_objective` (to add); export at `crates/khive-runtime/src/lib.rs:208`; a search for `GraphProximityObjective` under `crates/` finds no production caller outside `crates/khive-runtime/src/objectives.rs` (the other hits are tests and the public export). |
| A10 | Caller-anchor work is tracked separately from v1 in #5984 and depends on ADR-042 section 7.                                                                | #5984; `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:235`, with the deferred carrier at `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:271`.                                                                                                                                                                                                                           |
| A11 | Default-off implementation acceptance does not require the historical with-graph arms.                                                                     | A search for "The with-graph measurement arms in section 3 are not acceptance requirements" in `docs/adr/ADR-074-graph-aware-recall.md` identifies Amendment 1, Decision 2; behavioral fixtures A1-A9 define the implementation checks.                                                                                                                                                   |
| A12 | Turning the default weight above zero needs the measured delta and specification-gate approval, including recall/rank lift across corpus age and salience. | A search for "Turning the default weight above zero needs the measured delta and specification-gate approval" across wrapped prose in `docs/adr/ADR-074-graph-aware-recall.md` identifies Decision 2; #5555 records the required measurements and gate disposition before a default change at `crates/khive-pack-memory/src/config.rs:369`.                                               |
