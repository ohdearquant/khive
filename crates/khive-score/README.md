# khive-score

Deterministic fixed-point scoring: `f64` relevance scores are converted to a
`2^32`-scaled `i64` (`DeterministicScore`) so ranking, aggregation, and RRF
fusion give bit-identical results across platforms. Also provides sum/avg/min/max
aggregation, weighted sum, distance-to-score conversion, and ID-tiebreak comparators.

Checked scalar helpers cover unit-interval parsing, terminal bounded boosts,
time decay, scaled weight normalization, keyed weighted means, and min–max bands.
They are pure arithmetic: callers choose time units, score domains, configuration
defaults and filtering. Float results do not have the fixed-point API's
cross-platform bit-reproducibility guarantee.

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

## Checked scalar arithmetic

`UnitScore::try_new` accepts finite values in `[0,1]`, while
`from_fraction_or_percent` accepts `[0,100]`: `1` means a fraction of one,
and values above one are percentages. `clamp01` clamps finite inputs but
rejects NaN and infinity. `UnitScore::boost` clamps once, after evaluating
the complete multiplication; it is a terminal operation, so inserting it
inside a longer adjustment chain can change the result.

`boost_factor` and `affine_clamped` return raw `f64` values that can exceed
one or be negative. Their `_f32` variants retain `f32` arithmetic. Invalid
inputs and nonfinite intermediates return `ArithmeticError`; the helpers
do not substitute a default or clamp overflow into a valid score.

`HalfLife` accepts every finite positive duration, including subnormals.
`Decay` supports exponential, hyperbolic, rational power-law and identity
forms. Rate and half-life constructors retain distinct evaluation forms.
Elapsed time must use the same unit as the parameters; finite nonpositive
time returns the input unchanged. Invalid time still refuses for identity
and zero-value inputs. Scalar application accepts nonunit masses and signed
values. Power-law application retains the ordered `value*h/(h+t)`
expression when its intermediates are finite, including its rounding and
underflow; multiplying by the separately exposed factor is not bit-equivalent.

`ValidatedWeights` preserves a nonempty ordered sequence of finite
nonnegative weights with positive support. Normalization scales by the
largest weight before summing, so equal maximum or subnormal weights remain
usable. `NamedWeights` also permits empty and all-zero maps. Its weighted
mean uses the caller's unique feature order, ignores unknown configured
keys and zero-weight features, and rejects active nonfinite features.
It returns raw signed values rather than a unit score.

`min_max_band` and `min_max_band_f32` retain input order and precision,
with explicit span tolerance and a `DegenerateSpan` policy. `Ceiling`
returns the upper bound, `UpperEndpoint` evaluates the separately rounded
`floor + (ceiling - floor)`, and `ClampMaximum` clamps the observed maximum.
Filters and signal-strength factors belong to callers. The separate
`min_max_band_fixed` uses integer arithmetic for `[0,1]` normalization;
equal inputs map to one without a float round trip.

Existing fixed-point operations and callers retain their original behavior.

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
