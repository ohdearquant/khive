# Fusion function contracts

The free functions combine ranked `(ID, DeterministicScore)` lists and always use ascending ID to
break exact score ties.

## `reciprocal_rank_fusion`

RRF ignores source score magnitudes and adds `1 / (max(k, 1) + rank)` for each source containing an
ID, with rank starting at one. Duplicate IDs within one source contribute only at their first,
best rank, preventing one retriever from voting twice. The function returns every unique ID sorted
by fused score; it does not apply top-k truncation.

## `weighted_fusion`

Each source is independently min-max normalized to `[0, 1]`; an equal-valued or single-item source
maps every member to `1.0`. Non-finite and negative weights become zero. Positive weights for
actual sources are normalized to sum to one; if all effective weights are zero, sources receive
equal weight. Extra weights do not steal mass, and extra sources beyond the weight list receive zero
weight and inject no IDs.

Within a source, duplicate IDs keep their maximum normalized value. Fixed-point weighted products
then accumulate deterministically. The full unique result set is returned in ranking order.

`try_normalize_weights` rejects the first non-finite value by index. `normalize_weights` keeps
only finite, strictly positive weights (others become zero, all-zero input becomes equal weights),
matching `weighted_fusion`'s sanitization. `weights_are_normalized` checks whether the positive sum
is within a caller-supplied tolerance of one.

## `union_fusion`

Union keeps the maximum original score for each ID across all sources and returns the full sorted
set. It performs no score normalization.

## `fuse`

The dispatcher selects the algorithm from `FusionStrategy` and truncates its result to `top_k`.
`top_k == 0` returns no results. Vector-only and keyword-only modes select one source; when only one
source exists it is authoritative regardless of the nominal index. These pass-through modes
preserve that source's order. A custom strategy returns `FuseError::CustomRequiresRuntime` because
custom executors live in the runtime registry.
