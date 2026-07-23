# ADR-006: Deterministic Scoring

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

khive ranks search results, fuses retrieval signals, and caches scores in SQL. Every ranking
decision must be deterministic: the same inputs produce the same output on every platform,
every run, every CPU architecture. Floating-point arithmetic does not guarantee this — IEEE
754 allows intermediate precision, fused multiply-add reordering, and platform-specific
rounding.

The scoring system must satisfy:

1. **Bit-exact reproducibility.** Two runs of the same query over the same data produce the
   same ranked output, byte-for-byte.
2. **SQL round-trip.** Scores cached as `INTEGER` in SQLite recover the exact original value.
   No lossy float→int→float conversion.
3. **Cross-backend comparability.** Scores from different backends (hot, cold, lore) are
   comparable without re-normalization when fused by the SubstrateCoordinator.
4. **Metric-aware conversion.** Vector indexes compute distances in f32. The scoring contract
   must define how distances become similarity scores deterministically, per distance metric.

## Decision

### `DeterministicScore`: i64 fixed-point

`DeterministicScore` is a 64-bit signed integer with a fixed scale factor of 2^32.

```text
DeterministicScore(raw: i64)

Logical value = raw / 2^32
Range: approximately [-2^31, +2^31) with 2^-32 precision
SQL storage: INTEGER (i64, native SQLite affinity)
Ordering: standard integer comparison (no float comparison edge cases)
```

Arithmetic is saturating: overflow clamps to `MAX` (= `i64::MAX`), underflow clamps to
`NEG_INF` (= `i64::MIN + 1`). The raw value `i64::MIN` is a reserved sentinel (`MIN`)
that is not produced by any public arithmetic or float-conversion path. This makes
runtime-reachable scores disjoint from the sentinel.

NaN and infinity inputs to `from_f32`/`from_f64` are mapped to deterministic sentinel
values (NaN → `ZERO`, `+∞` → `MAX`, `-∞` → `NEG_INF`).

### Canonical implementation

`khive-score` is the canonical owner of `DeterministicScore` and the related
deterministic scoring primitives (`rrf_score`, `weighted_sum`, `Ranked<T>`,
`DistanceMetric`, `similarity_from_distance`). It is a self-contained Rust crate.
The formal contract is the set of normative invariants defined in this ADR.

### Normative invariants

The implementation MUST satisfy:

1. **Total order**: antisymmetry, transitivity, totality over all `DeterministicScore` values.
2. **Saturating arithmetic**: add, subtract, and accumulation saturate at `NEG_INF`
   (= `i64::MIN + 1`) and `MAX` (= `i64::MAX`). No wrapping, no panic. The reserved
   `MIN` (= `i64::MIN`) sentinel is never produced by public arithmetic.
3. **Deterministic NaN/infinity handling**: `from_f32(NaN) == from_f64(NaN) == ZERO`.
   `+∞` maps to `MAX`, `-∞` maps to `NEG_INF`. `MIN` is never produced.
4. **SQL INTEGER bit-exact round-trip**: `DeterministicScore(x).to_sql().from_sql() == DeterministicScore(x)`.
5. **Metric-aware f32 conversion**: distance-to-similarity conversion at vector search result
   boundaries uses the metric-specific monotonic transform defined below.

If the implementation changes representation, arithmetic strategy, or conversion semantics,
it must preserve these invariants or amend this ADR.

### f32 boundary: metric-aware conversion

Vector indexes compute distances in f32. Those distances are not exposed as khive scores.
At the search result boundary, the backend converts `(distance, metric)` into a
similarity-valued `DeterministicScore`:

```rust
pub enum DistanceMetric {
    Cosine,
    Dot,
    Euclidean,
    Manhattan,
}

impl DeterministicScore {
    pub fn similarity_from_distance(distance: f32, metric: DistanceMetric) -> Self {
        let d = sanitize_distance(distance) as f64;
        let similarity = match metric {
            DistanceMetric::Cosine => 1.0 - d,
            DistanceMetric::Dot => -d,
            DistanceMetric::Euclidean | DistanceMetric::Manhattan => {
                1.0 / (1.0 + d.max(0.0))
            }
        };
        Self::from_f64(similarity)
    }
}
```

This prevents each caller from inventing its own conversion rule. The conversion is the
single boundary where f32 enters the deterministic scoring world.

### RRF fusion: K = 60

Reciprocal Rank Fusion uses K = 60 as the standard default (from the original Cormack
et al. paper). The fusion API takes K explicitly:

```rust
pub fn reciprocal_rank_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    k: usize,
) -> Vec<(Id, DeterministicScore)> {
    // per-source dedup, deterministic rrf_score accumulation,
    // descending-score / ascending-ID total order
}
```

Callers pass K = 60 unless they document a workload-specific override. Overrides must
be documented because they change ranking behavior and evaluation
comparability. Silent drift between K values across retrieval surfaces is a correctness bug.

RRF fusion is commutative with respect to source-list order: the output is the same
regardless of the order in which source lists are provided.

### Normalization contract

`DeterministicScore` is a dimensionless fixed-point carrier. It can represent raw BM25,
cosine similarity, dot products, RRF scores, normalized weights, or any other scalar. The
type does not imply normalization.

Fusion functions have their own contracts:

- `weighted_sum` requires normalized, comparable inputs — typically in `[0, 1]` — unless
  the caller documents another shared scale. Mixing raw BM25 scores with cosine similarities
  in a weighted sum produces nonsense.
- `reciprocal_rank_fusion` is rank-based and does not require score normalization. It
  consumes position ordinals, not raw score magnitudes.

Raw score storage as `DeterministicScore` is allowed. Callers must not mix raw incomparable
score domains in weighted arithmetic.

### i128 intermediates

The Rust reference implementation uses i128 intermediates to implement saturating
add/subtract/accumulation safely. This is an implementation detail, not a normative
requirement. Other implementations may use another method if they preserve the same
saturating semantics.

### `QuantKey` removal

`QuantKey` was an 8-byte packed sort-key optimization (i32 quantized score + u32 ID prefix)
intended for hot-loop sorting. It is **not** part of the deterministic scoring contract
(different scale, lossy precision, not safe for storage or cross-backend exchange).

`QuantKey` has been **removed entirely** from `khive-score`. There is no deprecation
period. If a future workload demonstrates a material speedup over `Ranked<T>` /
`DeterministicScore` sorting on representative retrieval traces, a new optimization can
be introduced behind a fresh ADR.

## Rationale

### Why fixed-point (not floating-point)?

IEEE 754 float arithmetic is not associative. `(a + b) + c != a + (b + c)` in general.
Different compilers, optimization levels, and CPU architectures produce different results
for the same computation. A score computed on one machine may not equal the same score
computed on another. Fixed-point integer arithmetic is fully deterministic.

### Why i64 with 2^32 scale?

i64 provides ~9.2 quintillion distinct values. 2^32 scale gives ~32 bits of integer range
and ~32 bits of fractional precision — sufficient for score magnitudes used in retrieval
ranking. SQL `INTEGER` is native i64 in SQLite, so no type conversion is needed.

### Why a single canonical crate?

Every future change to a scoring primitive must be applied twice if two copies exist
independently. `DeterministicScore` is the foundation of deterministic ranking —
divergence between two copies is a correctness risk, not a convenience issue. All scalar
scoring primitives therefore live in `khive-score` and nowhere else.

### `khive-fusion` disposition

`khive-fusion` implements list-fusion strategies (reciprocal rank fusion, weighted
combination, union) on top of the `khive-score` primitives. It does not duplicate
scalar scoring arithmetic. If a new scalar primitive is needed, it belongs in
`khive-score` (canonical) and is consumed by `khive-fusion`.

### Why metric-aware conversion?

HNSW returns distances. BM25 returns relevance scores. Cosine distance and Euclidean distance
require different monotonic transforms to become similarity scores. If each caller invents its
own transform, the same raw distance produces different `DeterministicScore` values depending
on the code path. The `similarity_from_distance` function is the single conversion point.

### Why K = 60?

K = 60 is the standard RRF default from the original Cormack et al. paper. The explicit
`k` parameter on `reciprocal_rank_fusion` allows tuning for specific workloads. Callers
experimenting with alternative K values must document the rationale.

### Why remove QuantKey?

`QuantKey` was a relative-order optimization for hot-loop sorting. It did not preserve
absolute score values and used a different scale than `DeterministicScore`. Keeping it as
deprecated code added a second sort-key concept readers had to learn before reaching for
the one that matters. khive is early enough that a clean delete is preferable to a
deprecation period; reintroduce as a private optimization (or a new ADR) only if a real
workload demonstrates need.

## Consequences

### Positive

- Bit-exact reproducibility across platforms and runs.
- SQL `INTEGER` caching with zero-loss round-trip.
- Single conversion point for f32 distances → deterministic scores.
- Single canonical implementation in `khive-score`.
- Fusion contracts (RRF rank-based, weighted_sum requires normalization) prevent misuse.

### Negative

- `QuantKey` was removed; any hot-path retrieval sort that used it now uses `Ranked<T>`
  / `DeterministicScore` ordering directly.
- K = 60 is the standard default. Callers who need a different K pass it explicitly and
  document the rationale.

### Neutral

- `DeterministicScore` representation (i64, 2^32 scale) is unchanged.
- The RRF accumulation algorithm is unchanged.
- Score values stored in existing SQLite databases remain valid.

## Implementation

- `khive-score`: self-contained canonical implementation of `DeterministicScore`,
  `rrf_score`, `weighted_sum`, `Ranked<T>`, `DistanceMetric`, and
  `similarity_from_distance`; `khive-fusion` builds `reciprocal_rank_fusion` on top. Constants: `MAX` (i64::MAX), `NEG_INF`
  (i64::MIN + 1), `ZERO` (0), `MIN` (i64::MIN, reserved sentinel).
- SQL column type: `INTEGER` (i64). No schema migration needed.
- `QuantKey`: removed (file deleted, all re-exports dropped). Use `Ranked<T>` and
  `DeterministicScore` ordering for sort hot paths.
- Future changes must preserve the normative invariants above or amend this ADR in
  the same PR.
