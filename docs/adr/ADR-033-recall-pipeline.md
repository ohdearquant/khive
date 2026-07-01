# ADR-033: Recall Pipeline — Configurable Multi-Stage Memory Retrieval

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive

## Context

The memory pack ([ADR-021](ADR-021-memory-pack.md)) exposes two verbs: `remember` and `recall`.
ADR-021 §5 specifies the recall verb's scoring formula as a v1 starting value:

```
score = rrf_score * 0.70 + effective_salience * 0.20 + temporal * 0.10
```

and explicitly defers recall-weight tuning to a separate ADR when research informs it. This
ADR is that follow-on work. Three problems motivate it:

**1. All weights are hardcoded.** The 0.70 / 0.20 / 0.10 split, the 30-day temporal half-life,
and the decay formula `salience * exp(-decay_factor * age_days)` are wired into handler logic.
Changing any weight requires editing Rust source and recompiling.

**2. The pipeline is opaque.** The `recall` verb returns final results. There is no way to
inspect intermediate states: what did FTS find? What did vector search find? What did RRF
produce before salience and temporal weighting were applied? Without intermediates,
calibration is guesswork.

**3. No fold integration.** The scoring formula is ad-hoc arithmetic, not an Objective
composition ([ADR-024](ADR-024-fold-cognitive-primitives.md)). This means it cannot benefit
from precision-weighting (ADR-024 §Bayesian extensions), epistemic selector weight, or the
`ComposePipeline` structure. The Hoare triple for recall is undocumented.

Ocean's directive: expose a set of configurable handlers so recall behavior can be tuned and
calibrated empirically without recompilation.

## Decision

### 1. RecallConfig — all weights are parameters

```rust
/// Per-call configuration for the recall scoring pipeline.
/// All fields have defaults that reproduce current shipped behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallConfig {
    // Fusion weights — must be non-negative; sum must be > 0
    pub relevance_weight: f64,       // default 0.70
    pub salience_weight: f64,        // default 0.20
    pub temporal_weight: f64,        // default 0.10

    // Weighted feature rerank. Keys are feature names, not model reranker names.
    // Supported shipped keys: "relevance", "salience", "temporal",
    // "text_match", "vector_match". Missing keys are disabled.
    pub reranker_weights: HashMap<String, f64>,

    // Temporal parameters
    pub temporal_half_life_days: f64, // default 30.0
    pub decay_model: DecayModel,      // default Exponential

    // Retrieval parameters
    pub candidate_multiplier: u32,    // default 20 — candidates per path before fusion
    pub candidate_limit: Option<u32>, // explicit cap; None preserves multiplier behavior
    pub fuse_strategy: FusionStrategy, // default Weighted { weights: [0.7, 0.3] }
    pub min_score: f64,               // default 0.0
    pub min_salience: f64,            // default 0.0
    pub include_breakdown: bool,      // default false

    // Archive scoring override and brain-profile hint.
    pub scoring: Option<ScoringConfig>,
    pub brain_profile: Option<BrainProfileHint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecayModel {
    /// salience * exp(-decay_factor * age_days)
    Exponential,
    /// salience * (1 / (1 + decay_factor * age_days))
    Hyperbolic,
    /// salience * (half_life / (half_life + age_days))
    PowerLaw { half_life_days: f64 },
    /// No decay — salience used as-is
    None,
}
```

Defaults reproduce current behavior. Changing any field is a backward-compatible parameter
shift. Invalid configs (negative weights, zero-sum weights) are caught at handler entry and
return a per-op `{ok: false, error: "..."}` response; the batch does not abort.

The shipped pack keeps an in-process active `RecallConfig` in `MemoryPack`.
`PackTunable::apply_config` validates and updates that active config. Per-call overrides in
the `config` field take precedence for one call. File-backed persistence of calibrated
defaults is not shipped in ADR-033.

### 2. Six pipeline stages

The recall pipeline decomposes into shipped subhandlers. `memory.recall` is the public verb.
The debug/calibration handlers are registered as `Visibility::Subhandler`; MCP blocks normal
calls to subhandlers and exposes help only.

| Handler                    | Visibility | Input                                  | Output                                                              |
| -------------------------- | ---------- | -------------------------------------- | ------------------------------------------------------------------- |
| `memory.recall_embed`      | Subhandler | `{query: str}`                         | `{embeddings: [{engine_id, model_id, vector: [f32]}]}`              |
| `memory.recall_candidates` | Subhandler | `{query, namespace, limit}`            | `{text_hits, vector_hits_by_engine}`                                |
| `memory.recall_fuse`       | Subhandler | `{text_hits, vector_hits, strategy}`   | `{fused_hits}`                                                      |
| `memory.recall_rerank`     | Subhandler | `{candidates, config?}`                | `{reranked: [{id, rerank_scores, rerank_score}], active_rerankers}` |
| `memory.recall_score`      | Subhandler | `{fused_hits, reranked?, config}`      | `{scored: [{id, score, breakdown}]}`                                |
| `memory.recall`            | **Verb**   | `{query, namespace?, limit?, config?}` | `{results}`                                                         |

`memory.recall_embed` generates one embedding **per active engine** — the multi-engine
fan-out from ADR-031. The output is a list of `{engine_id, model_id, vector}` triples,
one per engine, in deterministic order (engine_id ASC). Single-engine deployments
return a one-element list — the shape is uniform regardless of engine count. It is
`Internal` because agents have no reason to see the raw embedding vectors (token
waste); operators inspecting an engine use
`kkernel exec 'memory.recall_embed(query="...")'`.

The `memory.recall_score` handler returns a score breakdown per result:

```json
{
  "id": "abc...",
  "score": 0.42,
  "breakdown": {
    "relevance": 0.35,
    "salience_raw": 0.80,
    "salience_decayed": 0.62,
    "temporal": 0.15,
    "weighted": {
      "relevance_contribution": 0.245,
      "salience_contribution": 0.124,
      "temporal_contribution": 0.015
    }
  }
}
```

The breakdown is what makes calibration actionable: see exactly which component dominates,
adjust the offending weight, re-run.

### 3. Handler dispatch

All handlers are owned by the memory pack and follow [ADR-023](ADR-023-declarative-pack-format.md)
verb-naming rules: pack-prefixed with one dot, sub-variants as snake_case under the namespace.

```rust
async fn dispatch(
    &self,
    verb: &str,
    params: Value,
    registry: &VerbRegistry,
) -> Result<Value, RuntimeError> {
    match verb {
        "remember"          => self.handle_remember(params).await,
        "recall"            => self.handle_recall(params, registry).await,
        "recall_embed"      => self.handle_recall_embed(params).await,
        "recall_candidates" => self.handle_recall_candidates(params).await,
        "recall_fuse"       => self.handle_recall_fuse(params).await,
        "recall_rerank"     => self.handle_recall_rerank(params, registry).await,
        "recall_score"      => self.handle_recall_score(params).await,
        _ => Err(RuntimeError::InvalidInput(format!(
            "memory pack does not handle verb {verb:?}"
        ))),
    }
}
```

The verb name passed to `dispatch` is already pack-stripped — the runtime routes
`memory.recall_embed` from the wire to the memory pack's dispatch as `"recall_embed"`.

This avoids bloating the agent-facing surface (only `memory.remember` and `memory.recall`
carry `Visibility::Verb`) while
exposing pipeline internals for calibration. Agents and developers use `memory.recall_score`
to inspect intermediate state; end users use `memory.recall`.

Dotted handlers establish a convention: packs may expose sub-verb handlers at
`verbname.subname` without registering them as product verbs. Other packs may follow this
pattern (e.g., `kg.validate`, `gtd.schedule`) but each pack must document its dotted surface
in its pack manifest.

### 4. Shipped scoring and rerank behavior

The shipped `memory.recall` path computes the three base score components
(`relevance`, `salience`, `temporal`) and then chooses one final score path:

1. When `RecallConfig.reranker_weights` is empty, use the default/archive scoring path.
2. When `RecallConfig.reranker_weights` is non-empty, build the five rerank features
   (`relevance`, `salience`, `temporal`, `text_match`, `vector_match`) and call
   `weighted_rerank`; the returned value becomes the final `rank_score`.

This is a REPLACE strategy, not an additive `WeightedObjective` blend with named
model rerankers. `RerankerObjective`, `cross_encoder`, and `graph_proximity` remain
deferred native-rerank design owned by ADR-042.

### 5. Config as a recall verb parameter

The `recall` verb accepts an optional `config` object. Missing fields use pack defaults:

```json
{
  "query": "what did we discuss about auth?",
  "limit": 10,
  "config": {
    "relevance_weight": 0.50,
    "salience_weight": 0.30,
    "temporal_weight": 0.20,
    "temporal_half_life_days": 7.0,
    "decay_model": "hyperbolic"
  }
}
```

Per-call config does not persist.

### 6. Recall Hoare triple

Per the ADR-024 documentation requirement, every domain-specific fold implementation must
document its Hoare triple:

| Component         | Recall instantiation                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Precondition**  | Query string is non-empty. Namespace contains memory-kind notes. RecallConfig is valid: all weights non-negative, `relevance_weight + salience_weight + temporal_weight > 0`. Embedding model is configured if the vector path is active.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **Program**       | Stage 1 (`memory.recall_embed`): query → embedding via multi-engine fan-out. Stage 2 (`memory.recall_candidates`): broad recall from FTS5 + vector, `candidate_multiplier × limit` candidates per path. Stage 3 (`memory.recall_fuse`): apply `fusion_strategy` (default RRF) to produce fused hits. Stage 4 (`memory.recall_rerank`): **REPLACE strategy** — if `reranker_weights` is non-empty, build the five rerank features per candidate (`relevance`, `salience`, `temporal`, `text_match`, `vector_match`) and call `weighted_rerank`; the normalized weighted score becomes the final score directly, replacing `compute_score`. If `reranker_weights` is empty (the default), this stage is skipped. Stage 5 (`memory.recall_score`): **only when `reranker_weights` is empty** — apply `compute_score` using the three base weights (`relevance_weight`, `salience_weight`, `temporal_weight`). When Stage 4 ran, Stage 5 is a no-op (final scores are already set). Stage 6 (select): truncate to `limit`; apply `budget` via `GreedySelector` if set. |
| **Postcondition** | Output is a deterministic list of memory notes ordered by composite score, within `limit`. All returned notes are alive (not soft-deleted) and `kind = memory`. Score breakdown is available on request via `memory.recall_score`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

### 6.1 Per-request knobs (ADR-033 §6 addendum)

The `recall` verb accepts three optional per-request knobs that override the pack-level
`RecallConfig` for a single call. All knobs are optional; absent or `null` preserves the
current default behavior.

| Parameter         | Type             | Default             | Semantics                                                                                                                                                         |
| ----------------- | ---------------- | ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `top_k`           | `usize` \| null  | `limit` or `10`     | Maximum number of results to return. Overrides `limit` when set. Capped at `100`.                                                                                 |
| `fusion_strategy` | `string` \| null | `"weighted"` (CC-6) | Fusion algorithm for candidate merging. Must be one of `"rrf"`, `"weighted"`, `"union"`, `"vector_only"`, `"keyword_only"`. Returns an error for any other value. |
| `score_floor`     | `f32` \| null    | `0.0` (no floor)    | Minimum composite score threshold applied after `compute_score`. Results below this floor are excluded. `0.0` or `null` = no filtering.                           |

**`fusion_strategy` details:**

- `"rrf"` — Reciprocal Rank Fusion with k=60. Robust across query types.
- `"weighted"` — Weighted linear combination (default; CC-6). Vector weight 0.7, text weight 0.3
  from pack-level config (`RecallConfig.fuse_strategy`). The request cannot override weights.
- `"union"` — Max-score per candidate ID. Inclusive but may surface low-quality text-only hits.
- `"vector_only"` — Skip text search entirely; rank by vector similarity alone. Useful when the
  query is a raw embedding or when FTS noise degrades results.
- `"keyword_only"` — Skip vector search entirely; rank by FTS score alone. Useful for exact-match
  recall or when no embedding model is configured.

**Example request DSL:**

```json
{
  "query": "attention mechanism in transformers",
  "top_k": 5,
  "fusion_strategy": "union",
  "score_floor": 0.3
}
```

This returns at most 5 results, fused via union strategy, with composite score ≥ 0.3.

**Interaction with `RecallConfig`:** Per-request knobs have higher precedence than `config`
and pack-level tuning. Resolution order: `top_k`/`fusion_strategy`/`score_floor` (request)

> `config` object (per-call) > pack active config (tunable) > `RecallConfig::default()`.

### 6.2 Weighted feature-combination reranker (PR #375)

The first concrete reranker ships in PR #375 as `crates/khive-pack-memory/src/rerank.rs`.
It does not depend on a cross-encoder model. It is purely arithmetic: a weighted sum of
pre-computed candidate features.

**Strategy: REPLACE.** When `RecallConfig.reranker_weights` is non-empty, the reranker
score replaces the `compute_score` output as the final score. When `reranker_weights` is
empty (the default), the reranker stage is skipped and `compute_score` runs as before.
Rationale: the five reranker features cover the same axes as `compute_score` (relevance,
salience, temporal) plus retrieval-source bonuses. A caller who configures
`reranker_weights` is explicitly taking over scoring — blending via a hidden α would
require a sixth config knob and make the weighting opaque.

**Five supported feature names** (keys in `reranker_weights`):

| Feature name   | Source                                                     | Range  |
| -------------- | ---------------------------------------------------------- | ------ |
| `relevance`    | Fused retrieval score (RRF/weighted fusion output)         | [0, 1] |
| `salience`     | Note salience after applying configured decay model        | [0, 1] |
| `temporal`     | `exp(-ln2/half_life × age_days)` — recency half-life score | (0, 1] |
| `text_match`   | 1.0 when candidate appeared in FTS text results, else 0.0  | {0, 1} |
| `vector_match` | 1.0 when candidate appeared in vector results, else 0.0    | {0, 1} |

**Formula:** `rerank_score = Σ(weights[feature] × feature_value) / Σ(weights[feature])` over
recognized feature names with positive weight. Normalizing by the positive weight sum makes
the score scale-invariant: doubling all weights leaves ranks unchanged. Unknown keys are
silently ignored for forward-compat and do not contribute to the weight sum.

**Opt-in / passthrough:** `RecallConfig.reranker_weights` defaults to `HashMap::new()`
(empty). Deployments that do not configure reranker weights see no behavior change.

**Extension surface:** New feature names can be added to `rerank.rs` as new match arms
in `weighted_rerank` — no schema change, no ADR amendment required for features. A new
_model class_ (e.g., cross-encoder, graph-proximate) would require a new ADR.

**`recall.rerank` subhandler:** Accepts fused candidates (with optional `fused_score`,
`salience`, `age_days`, `decay_factor`, `temporal`, `source` fields) plus a `config`
override. Returns `{reranked: [{id, rerank_scores, rerank_score}], active_rerankers}`.

Amendment (2026-06-14): the recall/rerank hit identity field was renamed `note_id` → `id` for
cross-verb coherence — `create`, `remember`, and `recall` all return `id` for the record. No
field collision (annotation-edge ids remain `edge_id`). Clean break, no dual-emit.
When weights are empty, returns candidates with empty `rerank_scores` (pass-through).

Amendment (2026-06-14): proposal lifecycle identity key rename — `proposal_id` → `id` in all
verb wire responses and input params. `propose` result, `review` result and `id` input param,
`withdraw` result and `id` input param, `list(kind=proposal)` row identity, and
`get(id=<proposal_uuid>)` result all use `id`. Internal struct fields (`ProposalCreatedPayload.proposal_id`),
DB columns (`proposals_open.proposal_id`), and event filter fields (`EventFilter.payload_proposal_id`)
are unchanged permanently. Clean break per PR #109 discipline.

### 6.3 Multi-model vector fusion (v024/multi-vector-fusion)

When multiple embedding models are registered in the runtime (via `RuntimeConfig.embedding_model`
plus `additional_embedding_models`, or via `KhiveRuntime::register_embedder`), the recall
pipeline fans out across all registered models for both `remember` and `recall`:

**`remember`-side (write fan-out):**

When `remember` is called without an explicit `embedding_model` parameter:

1. `registered_embedding_model_names()` enumerates all models in the `EmbedderRegistry`.
2. Embedding runs in parallel via `tokio::spawn` — one task per model.
3. One `VectorRecord` is inserted per model into its own vector store partition.

When `embedding_model` is explicitly set, only that model's VectorRecord is written
(single-model path, backward-compatible).

**`recall`-side (multi-source fusion):**

When `recall` is called without an explicit `embedding_model` parameter:

1. The query is embedded in parallel with each registered model.
2. Each model's vector store is queried with its corresponding query embedding.
3. `RecallCandidateSet.vector_hits_per_model` collects `(model_name, Vec<VectorSearchHit>)`
   tuples — one per model.
4. `fuse_candidates` builds N vector sources (one per model) plus 1 text source and passes all
   sources to `khive_retrieval::fuse_search_results`. For N=0 models an empty placeholder
   source is prepended to maintain the 2-source layout required for consistent fusion
   strategy behavior (RRF and Weighted do not apply their full algorithm to a single-source
   input — `fuse_search_results` returns raw scores when `sources.len() == 1`).

**Wire shape:** `recall` always returns `[{id, score, ...}]`. The
per-model vector breakdown is only exposed via the `recall.candidates` sub-handler's
`vector_candidates_per_model` field, which appears only when two or more models are active.

**Explicit-model scoping:** passing `embedding_model` to `recall` queries only that model's
vector store (single-model path). `recall.candidates` will not include
`vector_candidates_per_model` in this case.

**Strategy guidance for N > 1 models:** The shipped weighted fusion path first unions
per-model vector hits into one combined vector source, then fuses `[combined_vector, text]`
with the configured vector/text weights. `"rrf"` and `"union"` still accept N separate
vector sources; `"weighted"` operates on the combined-vector plus text layout.

### 7. Calibration protocol

To calibrate recall parameters for a deployment:

1. **Baseline**: `recall(query="...", limit=20)` — observe default-weight results.
2. **Inspect**: `memory.recall_score(fused_hits=..., config={...})` — read the per-result breakdown
   to see which component dominates.
3. **Adjust**: modify `relevance_weight`, `salience_weight`, or `temporal_weight` in the
   config.
4. **Compare**: run the same query with two configs; compare result orderings.
5. **Evaluate**: are the top results what was expected? If not, identify which breakdown
   field is pulling the wrong result to the top, and reduce its weight.
6. **Lock**: once calibrated, write the config to `settings.json` as pack defaults.

This is an empirical loop, not automated optimization. The handlers expose the knobs; the
operator turns them. Automated hyperparameter search over the weight space is deferred to
when ground-truth relevance labels exist (Brain ADR-032 accumulates this signal via
feedback events — see below).

### 8. Brain integration

The Brain pack ([ADR-032]) tracks feedback events that carry recall score breakdowns. When a
recalled memory is confirmed useful (e.g., an agent acts on it and the action succeeds), the
brain emits a positive feedback event carrying the breakdown. When a memory is surfaced but
ignored, the brain emits a negative event.

The brain's Bayesian update loop treats each weight in `RecallConfig` as a parameter with a
prior distribution. Posterior updates from confirmed feedback events shift the weight
estimates. The brain exposes its learned config via `brain.config(param="recall.*")`.

#### 8.1 Shipped brain integration status

Shipped brain integration for memory recall is limited to `PackTunable` projection and
application of `BalancedRecallState` into `RecallConfig`. `brain_profile` is a config
hint; handler-level cross-pack lookup is still TODO.

Runtime `brain.resolve` at recall call entry, rerank-stage profile resolution, LoRA hook
routing, and target-model matching are deferred to the native rerank work in ADR-042.
The shipped weighted feature rerank path does not consume brain LoRA adapters.

## Rationale

### Why configurable weights rather than compile-time constants

ADR-021 explicitly flagged the 0.70 / 0.20 / 0.10 split as starting values, not invariants.
Different deployment contexts (long-term research archive vs. daily session memory vs.
episodic log) want different balances between retrieval relevance, salience-weighted
memory, and temporal freshness. No single hardcoded setting is correct for all contexts.
Compile-time constants block empirical calibration entirely.

### Why dotted handlers rather than top-level verbs

The product verb surface is 15 verbs ([ADR-025](ADR-025-verb-speech-acts.md)). Promoting
`memory.recall_embed`, `memory.recall_candidates`, `memory.recall_fuse`, and
`memory.recall_score` to product verbs would add four verbs that 99% of callers never use.
ADR-025 establishes verbs as speech acts with illocutionary force — `memory.recall_score`
has no force on its own (it scores, it does not commit), which makes it unsuitable as a
first-class verb. Dotted handlers are a natural extension of the DSL dispatch mechanism
and keep the product surface clean.

### Why three Objectives rather than one scoring function

The v1 handler uses a single inline formula. Extracting three `Objective` implementations
has two concrete benefits: (1) each objective's precision can be independently calibrated
and fed to `WeightedObjective`; (2) any of the composition combinators from ADR-024 can
replace `WeightedObjective` without rewriting scoring logic. `PriorityObjective` (lexicographic
fallback) and `ConsensusObjective` (geometric mean) are both plausible alternatives for
specific use cases. One formula cannot be swapped without rewriting.

### Why `NoAnchor` rather than graph-proximate anchoring

Recall is unanchored in v1: it searches all memory notes in the namespace without bias toward
graph-proximate notes. Graph-proximate anchoring (bias results toward memories connected to
currently active entities) is a valid v2 extension — the `ComposePipeline`'s `anchor` slot
exists precisely for this. The `NoAnchor` implementation is a deliberate placeholder, not an
oversight.

### Why Brain integration is one-way

The brain adjusting recall weights during a live recall call would introduce a feedback loop
with ordering effects (the recall result influences the next brain update, which influences
the next recall result). The separation keeps each component deterministic in isolation.
Brain reads recall breakdowns post-hoc; recall reads brain config at call setup time, not
during scoring.

## Alternatives Considered

### A. Add config as compile-time constants with feature flags

Pros: no runtime overhead. Cons: recompile to change a weight; impossible to A/B test two
configs in the same session. Rejected.

### B. Expose all pipeline stages as top-level verbs

Pros: simpler dispatch. Cons: bloats the verb surface from 15 to ~19. Agents must learn four
new verbs that most callers never invoke. ADR-025 notes verb surface creep as a pathology.
Rejected in favor of dotted handlers.

### C. Config file only, no per-call override

Pros: one canonical place to change settings. Cons: cannot A/B two configs in one session;
calibration requires restarting the server to pick up file changes. The two mechanisms
compose cleanly — file for persistence, per-call for experimentation. Both implemented.

### D. Automated hyperparameter optimization as the primary interface

Pros: finds optimal weights without manual tuning. Cons: requires ground-truth relevance
labels that do not exist for most users at deployment time. The calibration protocol plus
brain feedback accumulates the necessary signal. Automated optimization is a future feature
built on top of this infrastructure, not a replacement for it. Deferred.

### E. Inline scoring in memory.recall_score, skip fold integration

Pros: less indirection. Cons: scores cannot benefit from precision-weighting; no path to
formal verification via Hoare triple; scoring logic cannot be composed or swapped without
rewriting. The fold integration is what makes recall scoring principled rather than ad-hoc.
Rejected.

## Consequences

### Positive

- All scoring weights are tunable at call time and at pack config time; no recompile needed
  to change recall behavior in any deployment.
- Score breakdowns make calibration actionable: the exact per-component contribution is
  visible for each recalled result.
- Recall scoring participates in the fold algebra (ADR-024): precision-weighting, composition
  combinators, and the Hoare triple all apply.
- The dotted handler convention is established and reusable by other packs.
- Four decay models (Exponential, Hyperbolic, PowerLaw, None) are supported without
  branching at the call site.
- Brain integration has a clean, one-way coupling via breakdowns.

### Negative

- Four new handlers in the memory pack increase its surface area and test burden.
- Config validation must be thorough: negative weights, zero-sum configs, and
  `temporal_half_life_days <= 0` must all return clear error messages. Silent coercion is
  forbidden ([ADR-017](ADR-017-pack-standard.md)).
- The dotted verb convention is novel — packs that add dotted handlers must document their
  sub-verb surface explicitly; absent documentation, the surface is invisible to callers.

### Neutral

- The `recall` verb signature gains an optional `config` field; callers that omit it see
  identical behavior to v1.
- No schema migration, no DDL change, no new entity kind, no new edge relation.
- `memory.recall_embed` uses the multi-engine fan-out from ADR-031 — the embedding
  infrastructure is not changed by this ADR, only consumed.

## Open Questions

1. **Decay model empirical evaluation.** Which decay model performs best for typical research
   KG usage? The calibration protocol provides the tooling; the data accumulates over time.
2. **Per-namespace config.** Should `RecallConfig` be namespace-scoped (different projects
   get different recall tuning) or global to the pack? The current design is global; per-
   namespace config is a natural extension once the multi-namespace use case is established.
3. **Anchored recall.** Should `recall` accept an anchor set (related entities) to bias
   results toward graph-proximate memories? The `ComposePipeline.anchor` slot exists for
   exactly this. Deferred to when a concrete use case emerges.
4. **memory.recall_train.** When brain feedback has accumulated sufficient labeled examples,
   should the pack support a `memory.recall_train` handler that runs gradient-free
   optimization over the weight space? The infrastructure is in place; the training signal
   is the open question.

## References

- [ADR-006](ADR-006-deterministic-scoring.md) — `DeterministicScore` for reproducible ordering
- [ADR-016](ADR-016-request-dsl.md) — dotted verb dispatch convention
- [ADR-021](ADR-021-memory-pack.md) — memory pack; this ADR extends recall verb semantics
- [ADR-024](ADR-024-fold-cognitive-primitives.md) — `Objective`, `Selector`, `ComposePipeline`
- [ADR-025](ADR-025-verb-speech-acts.md) — recall is an assertive verb; memory.recall_* handlers inherit
- ADR-031 — multi-engine retrieval; `memory.recall_embed` uses its fan-out
- ADR-032 — brain profile orchestration; brain tunes recall config posteriors via feedback events
- `crates/khive-pack-memory/src/handlers.rs` — current recall implementation
- `crates/khive-runtime/src/fusion.rs` — `FusionStrategy` (RRF, Weighted, Union, VectorOnly)
