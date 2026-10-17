# Per-engine similarity calibration

`khive_retrieval::calibrate_engine_hits` is a pure foundation for proposed
ADR-031 Amendment 5, Decision A. It accepts one engine's bounded candidate list
as `(id, raw_similarity: f64)` pairs and three explicit calibration parameters.
It does not invoke an engine, read configuration, or activate a handler.

All parameters must be finite, with
`-1 <= noise_floor < max_similarity <= 1` and `0 <= threshold <= 1`.
The amendment's cosine defaults are `(-1.0, 1.0, 0.0)`; callers pass them
explicitly. Invalid parameters return `RetrievalError::Configuration` even for
an empty list.

For each candidate, the function discards raw similarities below the floor,
then computes:

```text
u = clamp((raw_similarity - noise_floor) / (max_similarity - noise_floor), 0, 1)
```

Only candidates with `u >= threshold` remain. Ordering uses descending **raw**
similarity and ascending ID, not `u`: two distinct high scores that both clamp
to 1 retain their original raw ordering. Each retained `CalibratedHit` contains
its ID, original raw similarity, normalized similarity and consecutive one-based
rank. The function does not truncate the result or convert scores to fixed point.

## Proposed API input and duplicate contract

The following details define this additive API proposal; the amendment does not
itself specify nonfinite-input handling. Every raw input must be finite. A NaN
or infinity anywhere in the input refuses the whole call with
`RetrievalError::InvalidQuery`, naming its zero-based input position. Parameters
are validated before hits, and all hits are validated before filtering or
sorting. Both error variants have permanent classification.

Signed zeros tie numerically and use the ID tie-break. Repeated IDs retain their
highest raw similarity; an exact numeric score/ID tie retains the first input
occurrence. The raw floating-point bits of the retained occurrence are preserved,
including the sign of zero. Filtering and deduplication happen before ranks are
assigned, so retained distinct IDs receive ranks `1..n` without gaps.

The function adds no raw finite-score range restriction beyond the stated
formula. Callers must establish the adapter's agreed cosine/similarity contract;
this API does not attest that an arbitrary vendor score is cosine. Input bounds
remain caller-owned. No historical calibration values are supplied.

## Integration remains separate

This foundation does not add calibration fields to runtime `EngineConfig`,
validate them during configuration loading, wire the executor or change handlers.
Those parts of issue #4644 remain pending. The existing checked two-stage fusion
API is unchanged. Amendment 5 remains Proposed; this API does not declare it
accepted or activate multi-engine retrieval.
