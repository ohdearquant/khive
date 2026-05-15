# ADR-006: Deterministic Scoring (i64 Fixed-Point with 2^32 Scale)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

Hybrid search combines vector similarity, BM25 keyword scores, and reranking weights. Each subsystem
returns `f64` scores. We then need to:

1. Sort results by combined score.
2. Reproduce the same ordering across platforms (different CPUs, different OSes, different runs).
3. Cache scores in SQL columns and recover them with bit-exact equality.

Three things break determinism with raw `f64`:

1. **NaN ordering is undefined.** Sorting NaN values gives implementation-defined results. Different
   sorts on the same data can return different orders.
2. **Floating-point arithmetic is non-associative.** Summing `0.1 + 0.2 + 0.3` can give different
   results depending on order — and parallel reduction trees give different results than sequential
   sums.
3. **SQLite stores f64 as REAL but bit-conversion is unreliable across drivers.** Round-tripping a
   score through `f64 → SQL REAL → f64` doesn't always produce bit-identical results.

For a hybrid search system to be reproducible (essential for evaluation, debugging, and caching),
scores need a deterministic representation.

## Decision

**`DeterministicScore`: i64 fixed-point with 2^32 scale.**

```rust
pub struct DeterministicScore(i64);

const SCALE: i64 = 1 << 32; // 4,294,967,296

impl DeterministicScore {
    pub fn from_f64(x: f64) -> Self { ... }  // NaN → 0, ±Inf → ±i64::MAX
    pub fn to_f64(self) -> f64 { (self.0 as f64) / SCALE as f64 }
}
```

Where:

- The i64 value is `round(score * 2^32)`.
- Range: roughly `±2.1 billion` (i64::MAX / 2^32).
- Precision: ~9 decimal digits.
- NaN maps to 0 (so NaN scores deterministically rank as "neutral").
- ±Infinity maps to `±i64::MAX` (so infinite scores rank as max/min).

Used in:

- `VectorSearchHit.score: DeterministicScore`
- `TextSearchHit.score: DeterministicScore`
- Any cached score in SQL (column type: `INTEGER`, not `REAL`).

## Rationale

### Why i64 (not i32 or i128)?

- **i32 + 2^32 scale** = ±0.5 range. Too narrow — scores like RRF accumulators can exceed 1.0.
- **i64 + 2^32 scale** = ±2.1 billion range, ~9 decimal digits precision. Plenty for normalized
  scores.
- **i128** = unnecessary precision, doesn't fit cleanly into SQLite INTEGER (8-byte limit).

### Why 2^32 (not 2^16 or 2^48)?

- **2^16** = 4 decimal digits. Too coarse for cosine similarity (need 6+ digits).
- **2^32** = ~9 decimal digits. Matches f64's natural precision for values in [0, 1].
- **2^48** = wastes range; very few use cases need 14-digit precision.

### Why this is deterministic

1. **Integer sort is unambiguous.** No NaN comparison issues.
2. **Integer arithmetic is associative.** `a + b + c == c + b + a` always.
3. **SQL INTEGER round-trips bit-exact.** Same byte representation in/out.
4. **NaN/Inf are mapped to fixed values.** No platform-specific NaN bit patterns leak through.

### Why store as INTEGER in SQL

SQLite stores INTEGER as int64 with bit-exact roundtrip. Storing f64 as REAL involves IEEE-754
encoding/decoding with edge-case behavior in different drivers. INTEGER is unambiguous.

### Why round (not truncate)?

Rounding minimizes representation error. Truncating biases scores toward zero. Banker's rounding
(round-half-to-even) avoids systemic bias for ties, which matters in evaluation.

## Alternatives Considered

| Alternative             | Pros                   | Cons                                                         | Why rejected        |
| ----------------------- | ---------------------- | ------------------------------------------------------------ | ------------------- |
| Raw `f64`               | Simple, fast           | NaN sorts, non-associative arithmetic, REAL roundtrip issues | Determinism failure |
| `OrderedFloat<f64>`     | Resolves NaN ordering  | Doesn't fix arithmetic non-associativity or REAL roundtrip   | Half-measure        |
| `rust_decimal::Decimal` | Exact decimal math     | 16-byte, slow, doesn't fit SQL INTEGER                       | Overkill            |
| i32 with 2^16 scale     | Smaller representation | Precision too low for similarity scores                      | Insufficient        |

## Consequences

### Positive

- Bit-exact reproducibility across platforms and runs.
- Sorting is unambiguous (no NaN edge cases).
- SQL caching round-trips perfectly.
- Comparison is just integer comparison — fast.

### Negative

- Conversion between `f64` and `DeterministicScore` at every boundary. Mitigated: only at
  search/rank boundaries, not in hot loops.
- ~9 digit precision limit. Mitigated: matches f64's effective precision for normalized similarity
  scores.
- Range limit (±2.1 billion). Mitigated: well above any realistic score range.

### Neutral

- Developers must remember to convert at boundaries. Mitigated: trait return types use
  `DeterministicScore` directly.

## QuantKey: 8-byte sort key

For sort-only operations (no need for the score value, just the order), `QuantKey<T>` packs the
deterministic score + a tiebreaker UUID prefix into 8 bytes. Allows hash-map-friendly sort without
full DeterministicScore comparison.

```rust
pub struct QuantKey<T> { /* 8 bytes: top 4 = score, bottom 4 = id prefix */ }
```

Used in hot loops where sort throughput matters.

## Implementation

In `khive-score`:

```
crates/khive-score/src/
├── score.rs         // DeterministicScore + ops
├── comparator.rs    // Ranked<T>, cmp_desc_then_id, cmp_asc_then_id
├── ops.rs           // sum_scores, avg_scores, rrf_score, weighted_sum
└── quantkey.rs      // QuantKey<T>
```

20 tests verifying:

- NaN → 0
- ±Inf → ±i64::MAX
- f64 round-trip within precision tolerance
- Sort orderings
- RRF and weighted-sum determinism

## References

- ADR-005: Storage Capability Traits (uses DeterministicScore in search hits)
- `crates/khive-score/`: implementation
