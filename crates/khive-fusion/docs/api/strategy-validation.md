# Fusion strategy validation

`FusionStrategy` is validated at construction and deserialization so invalid configuration cannot
bypass the public builders through JSON.

## Reciprocal rank fusion

`rrf()` uses `DEFAULT_RRF_K` (60). `try_rrf(k)` rejects zero with
`FusionStrategyError::RrfKZero`; `rrf_with_k(k)` is a lossy convenience that clamps zero to one.

## Weighted fusion

`try_weighted(weights)` rejects NaN and infinity, distinguishing the two error variants and naming
the offending index. It preserves finite negative values for the execution layer, which treats them
as zero. `weighted` panics on non-finite input and is intended for trusted literals.

## Union and custom strategies

`union()` has no parameters. `try_custom(name, params)` requires a non-empty name and stores opaque
JSON for the runtime executor; the crate-level `fuse` function cannot execute it.

## Pass-through modes

`VectorOnly` and `KeywordOnly` select a single retrieval source without score transformation. They
are enum variants rather than builders because they carry no configuration.
