# Distance-to-Similarity Conversion

The distance helpers convert raw vector distances into the [`DeterministicScore`](../../src/score.rs)
values shared by HNSW, Vamana, and flat-scan retrieval. Their formulas and failure behavior are the
canonical boundary between vector backends and deterministic ranking.

## Formulas

| Metric | Distance d | Similarity | Notes |
| ------ | ---------- | ---------- | ----- |
| Cosine | $1 - \cos(x,y) \in [0, 2]$ | $1 - d$ | linear inversion |
| Dot | $-\langle x,y\rangle$ | $-d$ | negated for min-heap storage |
| L2 | $\|x-y\|_2$ | $\frac{1}{1+d}$ | always positive |

Higher score = more similar. The conversion is monotonically decreasing in distance for all three
metrics.

## Invariants and failure modes

- **Monotone:** similarity decreases strictly as distance increases (proven in Lean — see proof
  correspondence below).
- **Non-NaN input required:** `try_score_from_distance` returns `Err(NonFiniteDistance)` for NaN
  or non-finite inputs; `score_from_distance_lossy` maps them to `NEG_INF`.
- **Range validation:** Cosine distance must be in `[0.0, 2.0]`; negative L2 distance is rejected.
- **Unknown metrics:** produce `NEG_INF` (rank last) via `score_from_distance_lossy` and an
  `Err(UnsupportedMetric)` from `try_score_from_distance`.

## NaN / non-finite handling

| Function | NaN input | Out-of-range | Unknown metric |
| -------- | --------- | ------------ | -------------- |
| `try_score_from_distance` | `Err(NonFiniteDistance)` | `Err(InvalidDistanceRange)` | `Err(UnsupportedMetric)` |
| `score_from_distance_lossy` | `NEG_INF` | `NEG_INF` | `NEG_INF` |
| ~~`score_from_distance`~~ (deprecated) | mapped to `0.0` → perfect score | allowed | `NEG_INF` |

New callers **must** use `try_score_from_distance` or `score_from_distance_lossy`.

## `try_score_from_distance`

The strict API returns a score for supported, valid inputs. It reports `NonFiniteDistance` for NaN
or infinity, `InvalidDistanceRange` for cosine values outside `[0, 2]` and negative L2 values, and
`UnsupportedMetric` for future or otherwise unsupported metrics.

## `score_from_distance_lossy`

The lossy API applies the same validation and maps every error to `DeterministicScore::NEG_INF` so
invalid candidates rank last. It is appropriate only when callers intentionally prefer a sentinel
over inspecting the error taxonomy.

## Deprecated `score_from_distance`

The legacy API is retained for compatibility. It maps NaN to distance zero, permits ranges rejected
by the strict API, clamps negative L2 distance to zero, and ranks unknown metrics last. New code
must not rely on those behaviors.

## Proof correspondence

`khive.Retrieval.Distance.distanceToSimilarity` and `khive.Retrieval.Distance.similarity_nonneg`
in `proofs/Retrieval/Distance.lean` cover the ADR-030 phase-2 formalization. The API also implements
[ADR-006 deterministic scoring](../../../../docs/adr/ADR-006-deterministic-scoring.md) and
[ADR-012 retrieval composition](../../../../docs/adr/ADR-012-retrieval-composition.md).

## Verification and benchmarks

The conversion tests are the inline `#[cfg(test)]` module in
[`distance.rs`](../../src/distance.rs). Benchmarks in
[`benches/score_ops.rs`](../../benches/score_ops.rs) cover the `distance_cosine`, `distance_l2`,
and `distance_dot` targets.

```bash
# Run all distance conversion tests
cargo test -p khive-score distance

# Run benchmarks
cargo bench -p khive-score --bench score_ops
```

This contract was last reviewed on 2026-06-06 for v0.2.3, when `score_from_distance` was
deprecated.
