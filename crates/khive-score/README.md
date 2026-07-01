# khive-score

Deterministic fixed-point scoring: `f64` relevance scores are converted to a
`2^32`-scaled `i64` (`DeterministicScore`) so ranking, aggregation, and RRF
fusion give bit-identical results across platforms. Also provides sum/avg/min/max
aggregation, weighted sum, distance-to-score conversion, and ID-tiebreak comparators.

## Usage

```rust
use khive_score::{cmp_desc_then_id, rrf_score_one_based, DeterministicScore};
use std::num::NonZeroUsize;

let a = DeterministicScore::from_f64(0.42);
let b = DeterministicScore::from_f64(0.87);
assert!(a < b);

// Reciprocal Rank Fusion: 1-based rank, k=60 smoothing constant.
let fused = rrf_score_one_based(NonZeroUsize::new(1).unwrap(), 60);
assert!((fused.to_f64() - 1.0 / 61.0).abs() < 1e-9);

// Sort candidates by score descending, lower ID wins ties.
let mut hits = vec![(b, 2u64), (a, 1u64)];
hits.sort_by(|(sa, ia), (sb, ib)| cmp_desc_then_id(*sa, ia, *sb, ib));
```

## Semantics

- `DeterministicScore` saturates at `MAX` (`i64::MAX`) and `NEG_INF`
  (`i64::MIN + 1`) rather than overflowing. `MIN` (`i64::MIN`) is a reserved
  sentinel never produced by arithmetic, `from_f64`, or `to_f64` — only
  `from_raw` can construct it, and the `serde` `Deserialize` impl rejects it.
- `NaN` inputs map to `ZERO`; `±Infinity` map to `MAX`/`NEG_INF`.
- `sum_scores`, `avg_scores`, and `weighted_sum` clamp their result to
  `[NEG_INF, MAX]` and are order-independent regardless of input ordering.
- `try_score_from_distance` converts a raw distance plus a
  `khive_types::DistanceMetric` (`Cosine`, `Dot`, `L2`) into a score, erroring
  on non-finite or out-of-range input; `score_from_distance_lossy` is the
  infallible variant (`NEG_INF` on error). The un-prefixed `score_from_distance`
  is deprecated since 0.2.3 — it silently maps `NaN` to a perfect score.
- `Ranked<T: Ord>` is a `BinaryHeap`-ready max-heap adapter (higher score pops
  first, lower ID breaks ties). Its `Ord` impl is not a plain ascending sort
  order — use `cmp_desc_then_id` / `cmp_asc_then_id` when calling `sort()`
  directly on a `Vec`.

## Where this sits

Built on `khive-types` (for `DistanceMetric`). Consumed by every crate that
needs cross-platform-identical ranking:
[khive-bm25](https://crates.io/crates/khive-bm25),
[khive-hnsw](https://crates.io/crates/khive-hnsw),
[khive-fusion](https://crates.io/crates/khive-fusion),
[khive-fold](https://crates.io/crates/khive-fold),
[khive-storage](https://crates.io/crates/khive-storage),
[khive-retrieval](https://crates.io/crates/khive-retrieval),
[khive-db](https://crates.io/crates/khive-db), and
[khive-runtime](https://crates.io/crates/khive-runtime).
Deterministic scoring is required so that two nodes evaluating the same query
produce the same ranked order — see
[ADR-006](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-006-deterministic-scoring.md).

## License

Apache-2.0.
