# khive-fold Design

**Scope:** Cognitive primitives — Fold, Anchor, Objective, Selector.

**Last reviewed:** 2026-06-06

---

## Modules

| Module       | Purpose                                                              |
| ------------ | -------------------------------------------------------------------- |
| `fold`       | Deterministic reduce: entries → derived state                        |
| `anchor`     | Causal graph traversal (provenance chains)                           |
| `objective`  | Score candidates and select best (precision-weighted scoring)        |
| `selector`   | Budget-constrained pack: many → subset                               |
| `ordering`   | Deterministic IEEE-754 ordering primitives                           |
| `checkpoint` | Generic snapshot envelope + in-memory store for fold-managed indexes |
| `compose`    | Composition combinators: filter, map, sequential, dual               |
| `pipeline`   | ComposePipeline: objective scoring + selector budget packing         |

## Key Invariants

- No clock calls (`Utc::now`). Callers supply `as_of` timestamps explicitly. The foundation
  layer defaults `as_of` to the Unix epoch so contexts are safe to construct without
  knowing the wall-clock time.
- Non-finite scores are rejected at every selection boundary (`passes_score`).
- Non-finite precision falls back to 1.0 (full trust) rather than propagating NaN into ranking.
- Deterministic tie-breaking: UUID ascending after score descending everywhere.

## Dependency Boundary

`khive-fold` is a foundation-layer crate. Accepted direct dependencies:
`khive-types`, `khive-score`, `serde`/`serde_json` (optional feature), `uuid`, `chrono`
(DateTime type only, no clock feature), `thiserror`, `blake3` (checkpoint hashing).

## ADR Compliance

### Fold Cognitive Primitives (no-clock rule) (ADR-024)

`FoldContext` and `ObjectiveContext` both default `as_of` to `DateTime::<Utc>::default()`
(Unix epoch) rather than calling `Utc::now()`. This is deliberate: the foundation layer must
be clock-free so that fold operations are deterministic and testable without time injection.
Callers that need the current time must use `FoldContext::at(Utc::now())` or
`ObjectiveContext::at(Utc::now())` explicitly.

The same rule applies to `Checkpoint::new` and `Checkpoint::with_hash`: `created_at` is
set to the epoch on construction. Callers that need a real wall-clock timestamp should set
`checkpoint.created_at = Utc::now()` after construction.

### ADR-024 §"Bayesian extensions": Selector Budget Packing and Precision-Weighted Scoring

These behaviors are both specified in ADR-024 ("Fold Cognitive Primitives") under the
§"Bayesian extensions" section. There are no separate ADR-058 or ADR-059 documents.

**`GreedySelector`** implements priority-ordered budget packing with category-weight multipliers
and deterministic tie-breaking (effective-score desc, size asc, id asc). The `diversity_bias`
field controls how aggressively the selector prefers different categories at each pick step.
At `diversity_bias = 0.0` the behavior reduces to a single-pass greedy sort (backward-compatible).

**Precision-weighted scoring** — Both the `Objective` trait and the `Selector` implement
precision-weighted scoring as specified in ADR-024:

_Objective_: The `precision()` hook returns an inverse-variance estimate in $(0, 1]$ for
each candidate's score. The default is 1.0 (fully trusted). The effective ranking score is

$$\text{effective} = \text{score} \times \text{precision}$$

Non-finite precision falls back to 1.0. This allows objectives derived from uncertain models
(e.g., embedding similarity) to discount their own scores.

_Selector_: The `SelectorWeights.epistemic_weight` field adds an information-gain bonus to
the pragmatic score. Effective selection score:

$$\text{effective} = \text{pragmatic\_score} + \text{epistemic\_weight} \times \text{information\_gain}$$

`information_gain` is pre-computed by callers (the Selector has no access to embedding space).
At `epistemic_weight = 0.0` the behavior is identical to pure pragmatic selection.

## Consistency Notes

- `SelectorInput.information_gain` is caller-supplied because the Selector is pure-math with
  no embedding access. This is an intentional design boundary: callers that have embedding
  models pre-compute KL divergence proxies before calling `select`.
- `ConsensusObjective` uses the geometric mean of sub-objective scores:
  $\text{score} = \exp\!\left(\frac{1}{n}\sum_{i=1}^{n}\ln s_i\right)$.
  Any sub-score at or below zero causes the consensus to return 0.0 (not an error). Callers
  relying on ConsensusObjective should ensure sub-objectives return strictly positive scores
  for passing candidates.
- `InMemoryCheckpointStore.load_latest` breaks `created_at` ties by `uuid` (lexicographic
  ascending). Callers should not rely on ordering when `created_at` values are equal.

## Testing

Inline test sections exceed 300 lines in `selector.rs`, `objective/mod.rs`, and
`ordering/mod.rs` because they exercise private helpers or pub(crate) constants.
See `// INLINE TEST JUSTIFICATION` comments in each file for specifics.

## Rank-score precision (PR #535)

`SelectorInput.rank_score` exists because callers that compute the effective score in
`f64` (e.g. `ComposePipeline`, which multiplies an objective score by a precision weight)
need more precision than the narrowed `score: f32` field can hold — two `f64` scores that
differ by less than an `f32` ulp collapse to the same `f32` bit pattern before the selector
ever compares them. Falling back to `score as f64` for plain callers (e.g.
`khive-pack-knowledge`'s direct JSON-supplied candidates) is lossless, since those scores
were never more precise than `f32` to begin with.

Ranking runs through `DeterministicScore` (khive-score's i64 fixed-point contract), not raw
`f32::total_cmp` — this matches the objective-path ranking pattern in
`objective::traits::RankedIndex` and preserves precision that a caller-supplied `rank_score`
(f64) carries past what `f32` can hold. `category_weights` multipliers must scale
`rank_score` by the same weight applied to `score`, or rank comparisons see the unweighted
value while `min_score` filtering sees the weighted one — this silently defeated
`category_weights` for any candidate that set `rank_score` (khive PR #535).

## Test rationale notes

- `selector.rs::greedy_selector_handles_extreme_f32_products_without_overflow` — ranking in
  f64/`DeterministicScore` rather than raw f32 arithmetic means `f32::MAX * f32::MAX`
  (~1.15e77) no longer overflows to `f32::INFINITY` the way it did under the old f32-only
  computation; it becomes a large-but-finite f64 value, saturated into `DeterministicScore`
  rather than rejected. This is the precision upgrade FOLD-AUD (khive-score fixed-point
  contract) requires: a real magnitude, not a spurious f32 rounding artifact, decides the
  outcome.
- `selector.rs::rank_score_saturates_at_deterministic_score_max_without_panic` — two
  candidates whose `rank_score` both exceed `DeterministicScore`'s i64/2^32 representable
  range must both saturate to MAX rather than panicking or overflowing, then fall back to
  the deterministic size/id tie-break — same contract as `DeterministicScore`'s own
  saturating arithmetic (khive-score `score.rs` `from_rounded_arithmetic`).
- `selector.rs::rank_score_distinguishes_values_within_f32_ulp_of_one` — 1.0 and
  1.00000004 collapse to the identical f32 bit pattern (delta is below the f32 ulp at
  magnitude 1.0) but are distinct at the khive-score 2^32 fixed-point scale; `rank_score`
  must be the value that decides ranking, not the narrowed `score` field.
- `selector.rs::category_weights_reorder_candidates_when_rank_score_present` — regression
  for PR #535: before the fix, rank comparisons read the unweighted `rank_score` (category
  weights only ever touched `score`), so a low-weighted candidate could win despite a
  high-weighted competitor. See "Rank-score precision" above.

- `checkpoint.rs::sort_checkpoint_keys` is extracted as a standalone helper so it can be
  unit-tested with intentionally unsorted input, giving fail-before/pass-after coverage
  independent of `HashMap` randomization. The old `HashMap.keys().cloned().collect()` path
  returned keys in HashMap iteration order (non-deterministic); the regression test
  `sort_checkpoint_keys_produces_lexicographic_order` passes a reverse-sorted vector — the
  worst case for an unsorted implementation — to guarantee it fails against any
  implementation that skips the sort step.

## Failure Modes

- `FoldError::Serialization` — state serialization failed during checkpoint save.
- `FoldError::IntegrityMismatch` — stored BLAKE3 hash does not match recomputed hash on load.
- `FoldError::CheckpointNotFound` — delete or load of a non-existent checkpoint ID.
- `FoldError::LockPoisoned` — RwLock poisoned (thread panic while holding write lock).
- `ObjectiveError::NoCandidates` — `select_deterministic` called with empty slice.
- `ObjectiveError::NoMatch` — no candidate passes the minimum score threshold.
