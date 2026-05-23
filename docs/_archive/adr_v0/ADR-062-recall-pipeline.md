# ADR-062: Recall Pipeline — Configurable Multi-Stage Memory Retrieval

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-036 (Memory Pack Semantics), ADR-058 (Fold Cognitive Primitives),
ADR-061 (Retrieval Infrastructure)

## Context

The memory pack (ADR-036) exposes two verbs: `remember` and `recall`. The recall handler
(`khive-pack-memory/src/handlers.rs`) implements a single-pass pipeline:

```
embed query → FTS search → vector search → pre-filter to memory kind → RRF fusion
→ score: rrf * 0.70 + salience * exp(-decay * age_days) * 0.20 + exp(-age_days/30) * 0.10
→ filter min_score → sort → truncate
```

Three problems:

1. **All weights are hardcoded.** The 0.70/0.20/0.10 split, the 30-day temporal half-life, the
   decay formula `salience * exp(-decay_factor * age_days)` — none are configurable. To try a
   different weighting, you edit Rust source and recompile.

2. **The pipeline is a black box.** The `recall` verb returns final results. There's no way to
   inspect intermediate states: what did FTS find? What did vector search find? What did RRF
   produce before the importance/temporal weighting? Without intermediates, calibration is
   guesswork.

3. **No fold integration.** The scoring formula is ad-hoc arithmetic, not an Objective
   composition (ADR-058). This means it can't benefit from precision-weighting (ADR-059),
   epistemic selection, or the ComposePipeline (ADR-058 §7).

Ocean's directive: expose a set of configurable handlers (not all as verbs) so we can try
different parameters and calibrate the recall pipeline empirically.

## Decision

### 1. RecallConfig: all weights are parameters

```rust
/// Configuration for the recall scoring pipeline.
/// All fields have sensible defaults matching current behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallConfig {
    // --- Fusion weights ---
    pub relevance_weight: f64,    // default 0.70 — weight of RRF/fusion score
    pub importance_weight: f64,   // default 0.20 — weight of decay-adjusted salience
    pub temporal_weight: f64,     // default 0.10 — weight of pure recency

    // --- Temporal parameters ---
    pub temporal_half_life_days: f64,  // default 30.0 — days for temporal score to halve
    pub decay_model: DecayModel,       // default Exponential

    // --- Retrieval parameters ---
    pub candidate_multiplier: u32,    // default 20 — how many candidates per path before fusion
    pub fusion_strategy: FusionStrategy,  // default RRF { k: 60 }
    pub min_score: f64,               // default 0.0
    pub min_salience: f64,            // default 0.0

    // --- Selector parameters ---
    pub diversity_bias: f32,          // default 0.0 — category diversity in selection
    pub budget: Option<usize>,        // default None — no budget (return up to limit)
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

Defaults reproduce current behavior exactly. Changing any field is a backward-compatible
parameter shift, not a code change.

### 2. Pipeline stages as handlers

The recall pipeline decomposes into 5 stages. Each stage is an independently callable handler.
The `recall` verb runs all 5 in sequence. Individual handlers are available for calibration.

| Handler             | Verb?   | Input                                            | Output                               | Purpose                                                    |
| ------------------- | ------- | ------------------------------------------------ | ------------------------------------ | ---------------------------------------------------------- |
| `recall.embed`      | No      | `{query: str}`                                   | `{embedding: [f32]}`                 | Generate query embedding                                   |
| `recall.candidates` | No      | `{query, namespace, limit}`                      | `{text_hits, vector_hits}`           | Broad recall from FTS + vector                             |
| `recall.fuse`       | No      | `{text_hits, vector_hits, strategy}`             | `{fused_hits}`                       | Apply fusion strategy                                      |
| `recall.score`      | No      | `{fused_hits, config}`                           | `{scored: [{id, score, breakdown}]}` | Apply importance/temporal/relevance scoring with breakdown |
| `recall`            | **Yes** | `{query, namespace, limit, ...config_overrides}` | `{results}`                          | Full pipeline (all 5 stages)                               |

The `.score` handler returns a **breakdown** per result:

```json
{
  "note_id": "abc...",
  "score": 0.42,
  "breakdown": {
    "relevance": 0.35,
    "importance_raw": 0.80,
    "importance_decayed": 0.62,
    "temporal": 0.15,
    "weighted": {
      "relevance_contribution": 0.245,
      "importance_contribution": 0.124,
      "temporal_contribution": 0.015
    }
  }
}
```

This is what makes calibration possible: see exactly which component dominates, adjust weights,
re-run.

### 3. Handler dispatch via pack internal routing

Handlers are NOT MCP verbs. They're pack-internal functions accessible via the `request` DSL's
dotted notation for packs that opt in:

```
request("recall.score(fused_hits=..., config=...)")
```

The pack's `dispatch` method routes dotted verbs:

```rust
async fn dispatch(&self, verb: &str, params: Value, registry: &VerbRegistry) -> Result<Value, RuntimeError> {
    match verb {
        "remember" => self.handle_remember(params).await,
        "recall" => self.handle_recall(params, registry).await,
        "recall.embed" => self.handle_recall_embed(params).await,
        "recall.candidates" => self.handle_recall_candidates(params).await,
        "recall.fuse" => self.handle_recall_fuse(params).await,
        "recall.score" => self.handle_recall_score(params).await,
        _ => Err(RuntimeError::InvalidInput(format!(
            "memory pack does not handle verb {verb:?}"
        ))),
    }
}
```

This avoids bloating the verb surface (ADR-060: only 15 product verbs) while exposing internals
for calibration. Agents and developers use `recall.score` to debug; users use `recall`.

### 4. Recall as fold pipeline (ADR-058 integration)

The recall pipeline maps to a ComposePipeline:

```rust
impl MemoryPack {
    fn build_recall_pipeline(&self, config: &RecallConfig) -> ComposePipeline<NoteCandidate> {
        let relevance = WeightedObjective::new(vec![
            (config.relevance_weight, Box::new(RrfFusionObjective)),
            (config.importance_weight, Box::new(DecayAwareImportanceObjective {
                decay_model: config.decay_model.clone(),
            })),
            (config.temporal_weight, Box::new(TemporalRecencyObjective {
                half_life_days: config.temporal_half_life_days,
            })),
        ]);

        ComposePipeline {
            anchor: Box::new(NoAnchor),  // recall is unanchored by default
            objective: Box::new(relevance),
            selector: Box::new(GreedySelector),
        }
    }
}
```

Three new Objective implementations specific to memory:

```rust
/// Scores by RRF-fused retrieval relevance. Pure-math: receives
/// pre-computed RRF score via NoteCandidate.rrf_score.
pub struct RrfFusionObjective;

/// Scores by salience with configurable decay model.
pub struct DecayAwareImportanceObjective {
    pub decay_model: DecayModel,
}

/// Scores by pure temporal recency with configurable half-life.
pub struct TemporalRecencyObjective {
    pub half_life_days: f64,
}
```

These are Objective implementations (ADR-058 §2), not ad-hoc scoring functions. They compose
via WeightedObjective, PriorityObjective, etc. They participate in the Hoare triple.
Precision-weighting (ADR-059) applies to each.

### 5. Recall config as a `recall` verb parameter

The `recall` verb accepts optional config overrides:

```json
{
  "query": "what did we discuss about auth?",
  "limit": 10,
  "config": {
    "relevance_weight": 0.50,
    "importance_weight": 0.30,
    "temporal_weight": 0.20,
    "temporal_half_life_days": 7.0,
    "decay_model": "hyperbolic"
  }
}
```

Missing fields use defaults. This lets agents tune recall on-the-fly without server restarts.

### 6. Calibration protocol

To calibrate recall parameters:

1. **Baseline**: `recall(query="...", limit=20)` — default weights.
2. **Inspect**: `recall.score(fused_hits=..., config={...})` — see breakdown per result.
3. **Adjust**: change `relevance_weight` / `importance_weight` / `temporal_weight`.
4. **Compare**: run same query with different configs, compare result orderings.
5. **Evaluate**: are the top results what you expected? If not, adjust.
6. **Lock**: once calibrated, set the config as pack default in `settings.json`.

This is an empirical loop, not an automated optimization. The handlers expose the knobs; the
human (or agent) turns them.

### 7. Recall Hoare triple

| Component         | Recall instantiation                                                                                                                                                                                                                                                         |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Precondition**  | Query string provided. Namespace has memory-kind notes. Optionally: embedding model configured for vector path. RecallConfig valid (weights non-negative, sum > 0).                                                                                                          |
| **Program**       | Stage 1: broad recall (FTS + vector, candidate_multiplier × limit). Stage 2: pre-filter to memory kind. Stage 3: fuse (strategy from config). Stage 4: score (WeightedObjective with 3 components). Stage 5: select (truncate to limit, optional budget via GreedySelector). |
| **Postcondition** | Output is a deterministic list of memory notes, ordered by composite score, within limit. All returned notes are alive and kind=memory. Score breakdown available via `recall.score` handler.                                                                                |

## Alternatives Considered

### A. Add config as compile-time constants with feature flags

Pros: no runtime overhead. Cons: recompile to change a weight. Kills empirical calibration.

Rejected.

### B. Expose all handlers as top-level verbs

Pros: simpler dispatch. Cons: bloats the verb surface from 15 to ~20. Agents must learn 5 new
verbs that most users never need. ADR-060 cautions against verb sprawl.

Rejected. Dotted handlers (recall.score, recall.fuse) keep the verb surface clean.

### C. Use a config file instead of per-call overrides

Pros: one place to change. Cons: can't A/B test two configs in the same session. Per-call
overrides with file-based defaults gives both.

Both. File-based defaults in `settings.json`, per-call overrides in the `config` field.

### D. Automated hyperparameter optimization (grid search, Bayesian optimization)

Pros: finds optimal weights without manual tuning. Cons: requires an evaluation function
(ground-truth relevance labels) that doesn't exist yet for most users. The handlers expose
the search space; automated optimization is a future feature, not a prerequisite.

Deferred. The handlers are the necessary prerequisite.

## Consequences

### Positive

- **All scoring weights are tunable**: no recompile to change recall behavior
- **Score breakdowns enable calibration**: see exactly which component dominates
- **Fold integration**: recall scoring uses Objective composition, benefits from ADR-059
  precision-weighting and future extensions
- **Clean verb surface**: only `recall` is a verb; pipeline internals are dotted handlers
- **Multiple decay models**: exponential, hyperbolic, power-law, none — try all four

### Negative

- **More surface area in the pack**: 4 new handlers + RecallConfig type
- **Config validation burden**: invalid configs (negative weights, zero-sum) must be caught
  and reported clearly
- **Dotted verb convention is new**: packs haven't used `verb.subverb` notation before. This
  ADR establishes the convention; other packs may follow (e.g., `kg.validate`, `gtd.schedule`)

## Open Questions

1. **Decay model evaluation**: which decay model (exponential, hyperbolic, power-law) performs
   best for typical research KG usage? Needs empirical data from the calibration protocol.
2. **Per-namespace config**: should RecallConfig be namespace-scoped (different projects get
   different recall tuning)? Or global?
3. **Learned weights**: when ground-truth labels exist, should the pack support a `recall.train`
   handler that optimizes weights via gradient-free optimization?
4. **Anchored recall**: should `recall` accept an anchor set (related entities) to bias results
   toward graph-proximate memories? Natural extension via the ComposePipeline's Anchor slot.

## References

- ADR-036: Memory Pack Semantics — `remember` / `recall` verbs, memory note kind
- ADR-058: Fold Cognitive Primitives — Objective, Selector, ComposePipeline
- ADR-059: Bayesian Fold Extensions — precision-weighted objectives
- ADR-061: Retrieval Infrastructure — retrieval as fold pipeline
- ADR-006: Deterministic Scoring — `DeterministicScore` for reproducible ordering
- `khive-pack-memory/src/handlers.rs` — current recall implementation (267 LOC)
- `khive-runtime/src/fusion.rs` — FusionStrategy (RRF, Weighted, Union, VectorOnly)
