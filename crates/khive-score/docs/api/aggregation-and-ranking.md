# Score aggregation and deterministic ranking

The aggregation helpers combine fixed-point scores without reintroducing platform-dependent float
ordering, and the comparator helpers define a stable ID tie-break.

## Sum and averages

`sum_scores` and `avg_scores` accumulate through `i128`, clamp to `[NEG_INF, MAX]`, and return
`ZERO` for an empty slice. `avg_scores_checked` also returns a conservative saturation flag when
the order-independent absolute input mass or final mean exceeds 90% of `i64::MAX`.

`max_score` returns `NEG_INF` for an empty slice and `min_score` returns `MAX`, preserving useful
identity values for reductions.

## Reciprocal-rank helpers

`rrf_score(rank, k)` computes `1 / (k + rank)` but accepts an ambiguous `usize` rank and returns
`ZERO` on addition overflow or a zero denominator. Prefer `rrf_score_one_based(NonZeroUsize, k)`
for standard RRF or `rrf_score_zero_based(index, k)` for `enumerate()` indexes; overflow also maps
to `ZERO`.

## `weighted_sum`

Scores and weights must have equal lengths, otherwise `ScoreError::LengthMismatch` reports both
lengths. Each weight must be finite; `ScoreError::NonFiniteWeight` names the offending index.
Weights are converted to the same fixed-point scale, products accumulate in `i128`, and the result
saturates to the reachable score range. Empty equal-length slices return `ZERO`.

## `Ranked<T>` and comparators

`Ranked<T>` implements `Ord` for a max heap: higher score wins, and lower ID wins an exact tie.
Consequently `BinaryHeap::pop` yields best-first results, while ordinary `Vec::sort()` yields the
lowest score first. Use `cmp_desc_then_id` for ranking-order vectors and `cmp_asc_then_id` for the
opposite direction; both use lower ID as the tie-break.
