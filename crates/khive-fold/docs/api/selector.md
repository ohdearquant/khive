# Selector

Technical reference for `GreedySelector` — budget-constrained pack selection over scored
candidates (`selector.rs`) — and the precision issues its scoring pipeline has to guard
against.

## Precision-weighted selection score

`SelectorWeights.epistemic_weight` adds an information-gain bonus to the pragmatic score.
Effective selection score:

$$\text{effective} = \text{pragmatic\_score} + \text{epistemic\_weight} \times \text{information\_gain}$$

`information_gain` is pre-computed by callers — the Selector is pure-math with no embedding
access, so callers with embedding models pre-compute KL-divergence proxies before calling
`select`. At `epistemic_weight = 0.0` the behavior is identical to pure pragmatic selection.

`GreedySelector` implements priority-ordered budget packing with category-weight multipliers
and deterministic tie-breaking (effective-score desc, size asc, id asc). The `diversity_bias`
field controls how aggressively the selector prefers different categories at each pick step.
At `diversity_bias = 0.0` the behavior reduces to a single-pass greedy sort
(backward-compatible).

## Rank-score precision (PR #535)

`SelectorInput.rank_score` exists because callers that compute the effective score in `f64`
(e.g. `ComposePipeline`, which multiplies an objective score by a precision weight) need more
precision than the narrowed `score: f32` field can hold — two `f64` scores that differ by
less than an `f32` ulp collapse to the same `f32` bit pattern before the selector ever
compares them. Falling back to `score as f64` for plain callers (e.g. `khive-pack-knowledge`'s
direct JSON-supplied candidates) is lossless, since those scores were never more precise than
`f32` to begin with.

Ranking runs through `DeterministicScore` (khive-score's i64 fixed-point contract), not raw
`f32::total_cmp` — this matches the objective-path ranking pattern in
`objective::traits::RankedIndex` and preserves precision that a caller-supplied `rank_score`
(f64) carries past what `f32` can hold. `category_weights` multipliers must scale
`rank_score` by the same weight applied to `score`, or rank comparisons see the unweighted
value while `min_score` filtering sees the weighted one — this silently defeated
`category_weights` for any candidate that set `rank_score` (khive PR #535).

## Test coverage

- `greedy_selector_handles_extreme_f32_products_without_overflow` — ranking in
  f64/`DeterministicScore` rather than raw f32 arithmetic means `f32::MAX * f32::MAX`
  (~1.15e77) no longer overflows to `f32::INFINITY` the way it did under the old f32-only
  computation; it becomes a large-but-finite f64 value, saturated into `DeterministicScore`
  rather than rejected. This is the precision upgrade FOLD-AUD (khive-score fixed-point
  contract) requires: a real magnitude, not a spurious f32 rounding artifact, decides the
  outcome.
- `rank_score_saturates_at_deterministic_score_max_without_panic` — two candidates whose
  `rank_score` both exceed `DeterministicScore`'s i64/2^32 representable range must both
  saturate to MAX rather than panicking or overflowing, then fall back to the deterministic
  size/id tie-break — same contract as `DeterministicScore`'s own saturating arithmetic
  (khive-score `score.rs` `from_rounded_arithmetic`).
- `rank_score_distinguishes_values_within_f32_ulp_of_one` — 1.0 and 1.00000004 collapse to
  the identical f32 bit pattern (delta is below the f32 ulp at magnitude 1.0) but are distinct
  at the khive-score 2^32 fixed-point scale; `rank_score` must be the value that decides
  ranking, not the narrowed `score` field.
- `category_weights_reorder_candidates_when_rank_score_present` — regression for PR #535:
  before the fix, rank comparisons read the unweighted `rank_score` (category weights only
  ever touched `score`), so a low-weighted candidate could win despite a high-weighted
  competitor. See "Rank-score precision" above.
