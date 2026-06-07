# Distance-to-Similarity Conversion

**Scope:** Canonical formulas used by `khive-score` to convert raw vector distances to
[`DeterministicScore`](../src/score.rs) values consumed by all retrieval backends (HNSW,
Vamana, flat-scan).

**ADRs:** [ADR-006 Deterministic Scoring](../../../../docs/adr/ADR-006-deterministic-scoring.md) |
[ADR-012 Retrieval Composition](../../../../docs/adr/ADR-012-retrieval-composition.md) |
ADR-030 Formal verification (Lean proofs, Phase 2)

**Source:** [`crates/khive-score/src/distance.rs`](../src/distance.rs)
**Tests:** inline `#[cfg(test)] mod tests` in `distance.rs`
**Bench:** [`crates/khive-score/benches/score_ops.rs`](../benches/score_ops.rs) — targets
`distance_cosine`, `distance_l2`, `distance_dot`

---

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

## Proof correspondence

`khive.Retrieval.Distance.distanceToSimilarity` and `khive.Retrieval.Distance.similarity_nonneg`
in `proofs/Retrieval/Distance.lean` (ADR-030 §Phase 2).

## Commands

```bash
# Run all distance conversion tests
cargo test -p khive-score distance

# Run benchmarks
cargo bench -p khive-score --bench score_ops
```

Last reviewed: 2026-06-06 (v0.2.3, deprecation of `score_from_distance`)
