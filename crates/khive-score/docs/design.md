# khive-score Design

## ADR Compliance

### ADR-006: Deterministic Scoring

This crate implements the deterministic fixed-point scoring contract from ADR-006.

Key design decisions and constraints:

- `DeterministicScore` wraps `i64` scaled by $2^{32}$ (`4_294_967_296`). Integer arithmetic
  produces identical bit-patterns on x86_64, ARM64, and WASM — eliminating float non-determinism
  in ranking.
- The reserved sentinel `i64::MIN` (`MIN`) must never appear as a runtime value. All arithmetic
  and float-conversion paths clamp to `[NEG_INF, MAX]` where `NEG_INF = i64::MIN + 1`. This
  invariant is proven in `proofs/` (Lean).
- The distinction between `MIN` (reserved), `NEG_INF` (lowest reachable runtime value), and `ZERO`
  is a hard protocol boundary. `from_raw` accepts `i64::MIN` for internal use; `from_raw_checked`
  and `from_raw_saturating` are the safe constructors for untrusted data.
- Public arithmetic operators (`Add`, `Sub`, `Mul`, `Div`) all saturate to `[NEG_INF, MAX]` via
  `from_arithmetic_raw` / `from_rounded_arithmetic`. The reserved `MIN` sentinel is never produced.
- The custom `Deserialize` impl rejects `i64::MIN` with an error, enforcing the proof boundary at
  the serialization boundary.

### ADR-012: Retrieval Composition (High-Level Composition Layer) - Retrieval Composition

This crate provides the scoring primitives consumed by all retrieval backends (HNSW, Vamana,
flat-scan). The distance-to-similarity conversion functions (`try_score_from_distance`,
`score_from_distance_lossy`) are the canonical entry points; all backends must use them rather
than rolling their own conversions.

### ADR-024: Fold Cognitive Primitives

`sum_scores`, `avg_scores`, `weighted_sum`, and `rrf_score` are the aggregation primitives used
by the fold layer. These functions must remain saturation-safe and deterministic.

## Consistency Notes

- `score_from_distance` (deprecated since v0.2.3) maps NaN to a perfect score (`0.0` distance).
  This is a known semantic defect preserved for backwards compatibility. New callers must use
  `try_score_from_distance` (strict) or `score_from_distance_lossy` (fail-soft to `NEG_INF`).
- The `rrf_score` function is documented as treating `rank` as 1-based, but callers passing a
  raw `enumerate()` index of 0 will get an inflated score. The named variants
  `rrf_score_one_based` and `rrf_score_zero_based` enforce the convention at the type level.
