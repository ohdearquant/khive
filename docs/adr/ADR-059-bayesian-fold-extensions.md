# ADR-059: Bayesian Fold Extensions — Precision-Weighted Objectives and Epistemic Selection

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-058 (Fold Cognitive Primitives)

## Context

ADR-058 established the four cognitive primitives (Fold, Anchor, Objective, Selector) and their
Hoare structure. It explicitly deferred two extensions (§D and §E) that add formal grounding from
neuroscience and information theory:

1. **Objective scores are flat.** The `Objective<T>` trait returns `f64` scores and ranks by raw
   magnitude. Predictive Coding (Rao & Ballard 1999, Friston 2005) formalizes that not all inputs
   are equally reliable — precision (inverse variance) weights each signal. A high-scoring but
   unreliable candidate should lose to a lower-scoring but precise one.

2. **Selector selection is purely pragmatic.** The `Selector<T>` trait selects by score under a
   budget constraint. Active Inference (Friston et al. 2015) formalizes that intelligent selection
   balances two objectives: pragmatic value (reaching preferred states) and epistemic value
   (reducing uncertainty). The current Selector has no epistemic axis — every selection is
   pragmatic-only.

Both extensions are single-field additions, backwards-compatible, and grounded in the same
Bayesian Brain framework already represented in the KG (Active Inference `extends` Free Energy
Principle `composed_with` Predictive Coding, all through the Bayesian Brain hub).

## Decision

### 1. Precision-weighted Objective output

Add a `precision` field to `Selection<T>`:

```rust
pub struct Selection<T> {
    pub item: T,
    pub score: f64,
    pub precision: f64,  // NEW: reliability estimate, default 1.0
    pub index: usize,
    // ... existing stats fields
}
```

The effective ranking score becomes `score * precision`. When `precision = 1.0` (the default),
behavior is identical to ADR-058.

**Where precision comes from**: the `Objective<T>` implementation provides it. A memory recall
objective might set precision based on embedding model confidence. A retrieval objective might
derive it from source reliability. The trait gains an optional method:

```rust
pub trait Objective<T>: Send + Sync {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64;

    /// Precision estimate for the score. Default: 1.0 (fully trusted).
    /// Implementations override when score reliability varies across candidates.
    fn precision(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        1.0
    }

    // ... existing methods unchanged
}
```

The `select` and `select_top` default implementations use `score * precision` for ranking.
`DeterministicObjective` tie-breaking order: `score * precision` descending, then UUID ascending.

### 2. Epistemic Selector weight

Add an `epistemic_weight` field to `SelectorWeights`:

```rust
pub struct SelectorWeights {
    pub category_weights: BTreeMap<String, f32>,
    pub min_score: f32,
    pub diversity_bias: f32,
    pub epistemic_weight: f32,  // NEW: 0.0 = pure pragmatic (default), higher = prefer uncertainty-reducing candidates
}
```

The effective selection score becomes:

```
effective_score = pragmatic_score + epistemic_weight * information_gain
```

Where `information_gain` is the KL divergence between the prior and posterior after including the
candidate. When `epistemic_weight = 0.0` (the default), behavior is identical to ADR-058.

**`SelectorInput` gains an optional field**:

```rust
pub struct SelectorInput<T> {
    pub id: String,
    pub content: T,
    pub size: usize,
    pub score: f32,
    pub category: Option<String>,
    pub information_gain: Option<f32>,  // NEW: pre-computed by caller, None = 0.0
}
```

The caller pre-computes `information_gain` because the Selector is pure-math and has no access to
the embedding space needed to estimate KL divergence. This preserves the no-IO invariant.

### 3. Hoare-structure consequences

With these extensions, the fold Hoare triple (ADR-058 §Context) becomes:

| Hoare component | Without extensions | With extensions |
|-----------------|-------------------|-----------------|
| **Precondition** | Anchor state | Anchor state + precision estimates + information-gain estimates |
| **Program** | Score + rank | Score × precision + rank; epistemic-weighted selection |
| **Postcondition** | Deterministic ranked output | Deterministic ranked output where ranking is Bayes-optimal given precision estimates |

The Bayes-optimality claim: if precision estimates are accurate (calibrated), then
`score * precision` ranking minimizes expected loss under the Predictive Coding framework. This
is provable for single-layer hierarchies; multi-layer hierarchies are deferred.

### 4. Backwards compatibility

Both extensions default to identity behavior:
- `precision: 1.0` → `score * 1.0 = score` (no change)
- `epistemic_weight: 0.0` → `score + 0.0 * gain = score` (no change)
- `information_gain: None` → treated as `0.0`

No existing code needs to change. Existing `Objective` implementations inherit `precision() → 1.0`
from the default method. Existing `SelectorWeights` gain a new field with `Default` at `0.0`.

## Alternatives Considered

### A. Make precision a separate wrapper type instead of a trait method

Pros: no trait change. Cons: every caller must manually multiply `score * precision` before
passing to Selector, losing the integration. The whole point is that precision-weighting is built
into the ranking pipeline, not bolted on outside.

Rejected.

### B. Compute information_gain inside Selector (not pre-computed by caller)

Pros: cleaner API — caller doesn't need to estimate KL. Cons: computing KL divergence requires
access to the embedding space, violating the no-IO, pure-math invariant of foundation/fold.
The Selector would need an `EmbeddingService` dependency, which belongs in the service layer.

Rejected. Caller pre-computes; Selector consumes.

### C. Use Wasserstein distance instead of KL for information gain

Pros: Wasserstein is a proper metric (triangle inequality); KL is not. The lattice-transport
crate already implements Sinkhorn-regularized OT. Cons: Wasserstein requires full distribution
access (not just a scalar gain per candidate). The `SelectorInput` would need an embedding
vector field, bloating the type. KL-gain per candidate is a scalar that fits the existing
architecture.

Deferred. A Wasserstein-scored variant belongs in the service layer where distribution access
exists, not in the foundation primitive.

### D. Add a full Predictive Coding hierarchy (multiple layers)

Pros: more faithful to the neuroscience. Cons: massive scope increase. Single-layer
precision-weighting captures the key insight (reliability-adjusted scoring) with a one-field
addition. Multi-layer hierarchies require message-passing infrastructure that doesn't exist yet.

Deferred to a future ADR when Brain Phase 7 compose pipeline demonstrates the need.

## Consequences

### Positive

- **Objective scoring becomes reliability-aware**: unreliable high-scores don't dominate
- **Selector selection becomes information-seeking**: agents can prefer uncertainty-reducing candidates
- **Formal grounding**: fold operations are now approximate Bayesian inference (single-layer)
- **Zero breaking changes**: all defaults preserve existing behavior exactly

### Negative

- **Calibration burden on callers**: precision estimates are only useful if accurate. Miscalibrated
  precision (always 1.0, or random) degrades ranking. No enforcement mechanism.
- **information_gain pre-computation**: callers must estimate KL-gain externally. This is the
  correct architectural boundary (no IO in fold) but adds complexity to the service layer.

## Open Questions

1. **Precision calibration**: should there be a standard calibration protocol (e.g., holdout
   validation set) that `Objective` implementations should follow? Or leave entirely to callers?
2. **Multi-layer Predictive Coding**: when Brain Phase 7 compose pipeline matures, should
   precision propagate across compose stages (fold₁'s precision feeds fold₂'s prior)?
3. **information_gain estimation**: what's the cheapest good-enough KL estimate for memory recall?
   Embedding cosine distance as proxy? Fisher information approximation?

## References

- ADR-058: Fold Cognitive Primitives — Hoare-Structured Decisions
- Predictive Coding: Rao & Ballard, "Predictive coding in the visual cortex" (1999)
- Free Energy Principle: Friston, "A theory of cortical responses" (2005)
- Active Inference: Friston, K. et al., "Active inference and epistemic value" (2015)
- Bayesian Brain framework — Predictive Coding, Free Energy Principle, Active Inference
  as three lenses on the same Bayesian inference substrate
