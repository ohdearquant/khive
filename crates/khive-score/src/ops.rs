//! Aggregation and fusion operations for deterministic scores.

use crate::DeterministicScore;
use std::fmt;
use std::num::NonZeroUsize;

/// Errors produced by score aggregation and distance conversion operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreError {
    /// The two input slices have different lengths.
    LengthMismatch {
        /// Human-readable description of which two slices were compared.
        expected_desc: &'static str,
        /// Length of the first slice.
        first_len: usize,
        /// Length of the second slice.
        second_len: usize,
    },
    /// A weight at the given index is NaN or infinite.
    NonFiniteWeight {
        /// Zero-based index of the offending weight.
        index: usize,
    },
    /// The distance value is NaN, `+Inf`, or `-Inf`.
    NonFiniteDistance,
    /// The distance is finite but outside the valid range for the metric.
    InvalidDistanceRange {
        /// The metric whose range was violated.
        metric_name: &'static str,
        /// Bit-representation of the out-of-range distance for diagnostics.
        dist_bits: u32,
    },
    /// The distance metric is not one of the three currently supported variants.
    UnsupportedMetric,
}

impl fmt::Display for ScoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScoreError::LengthMismatch {
                expected_desc,
                first_len,
                second_len,
            } => write!(
                f,
                "{expected_desc}: first has {first_len} elements, second has {second_len}"
            ),
            ScoreError::NonFiniteWeight { index } => {
                write!(f, "weight at index {index} must be finite")
            }
            ScoreError::NonFiniteDistance => {
                write!(f, "distance must be finite (not NaN or infinity)")
            }
            ScoreError::InvalidDistanceRange {
                metric_name,
                dist_bits,
            } => write!(
                f,
                "distance value (bits=0x{dist_bits:08x}) is out of valid range for metric {metric_name}"
            ),
            ScoreError::UnsupportedMetric => {
                write!(f, "unsupported distance metric")
            }
        }
    }
}

impl std::error::Error for ScoreError {}

/// Return the saturating sum of `scores`, clamped to `[NEG_INF, MAX]`.
#[inline]
pub fn sum_scores(scores: &[DeterministicScore]) -> DeterministicScore {
    if scores.is_empty() {
        return DeterministicScore::ZERO;
    }
    let sum: i128 = scores.iter().map(|s| s.to_raw() as i128).sum();
    DeterministicScore::from_raw(sum.clamp(
        DeterministicScore::NEG_INF.to_raw() as i128,
        i64::MAX as i128,
    ) as i64)
}

/// Return the arithmetic mean of `scores`, clamped to `[NEG_INF, MAX]`.
#[inline]
pub fn avg_scores(scores: &[DeterministicScore]) -> DeterministicScore {
    if scores.is_empty() {
        return DeterministicScore::ZERO;
    }
    let sum: i128 = scores.iter().map(|s| s.to_raw() as i128).sum();
    let mean = sum / scores.len() as i128;
    DeterministicScore::from_raw(mean.clamp(
        DeterministicScore::NEG_INF.to_raw() as i128,
        i64::MAX as i128,
    ) as i64)
}

/// Return the mean of `scores` and a boolean saturation flag.
#[inline]
pub fn avg_scores_checked(scores: &[DeterministicScore]) -> (DeterministicScore, bool) {
    if scores.is_empty() {
        return (DeterministicScore::ZERO, false);
    }
    const SATURATION_THRESHOLD: i128 = (i64::MAX as i128) * 9 / 10;
    let sum: i128 = scores.iter().map(|s| s.to_raw() as i128).sum();
    let mean = sum / scores.len() as i128;
    // Use order-independent measures: check the absolute sum of all input
    // magnitudes (independent of sign cancellation order) and the final mean.
    let abs_mass: i128 = scores
        .iter()
        .map(|s| (s.to_raw() as i128).unsigned_abs() as i128)
        .sum();
    let near_saturation =
        abs_mass > SATURATION_THRESHOLD || mean.unsigned_abs() as i128 > SATURATION_THRESHOLD;
    let result = DeterministicScore::from_raw(mean.clamp(
        DeterministicScore::NEG_INF.to_raw() as i128,
        i64::MAX as i128,
    ) as i64);
    (result, near_saturation)
}

/// Return the maximum score, or [`DeterministicScore::NEG_INF`] for an empty slice.
#[inline]
pub fn max_score(scores: &[DeterministicScore]) -> DeterministicScore {
    scores
        .iter()
        .copied()
        .max()
        .unwrap_or(DeterministicScore::NEG_INF)
}

/// Return the minimum score, or [`DeterministicScore::MAX`] for an empty slice.
#[inline]
pub fn min_score(scores: &[DeterministicScore]) -> DeterministicScore {
    scores
        .iter()
        .copied()
        .min()
        .unwrap_or(DeterministicScore::MAX)
}

/// RRF score `1 / (k + rank)`. Rank is 1-based; prefer `rrf_score_one_based` or `rrf_score_zero_based`.
#[inline]
pub fn rrf_score(rank: usize, k: usize) -> DeterministicScore {
    let Some(denominator) = k.checked_add(rank) else {
        return DeterministicScore::ZERO;
    };
    if denominator == 0 {
        return DeterministicScore::ZERO;
    }
    DeterministicScore::from_f64(1.0 / (denominator as f64))
}

/// RRF score with 1-based rank (first result = rank 1). `k` is the smoothing constant.
#[inline]
pub fn rrf_score_one_based(rank: NonZeroUsize, k: usize) -> DeterministicScore {
    let Some(denominator) = k.checked_add(rank.get()) else {
        return DeterministicScore::ZERO;
    };
    DeterministicScore::from_f64(1.0 / denominator as f64)
}

/// RRF score with 0-based index (index 0 → rank 1 internally).
#[inline]
pub fn rrf_score_zero_based(index: usize, k: usize) -> DeterministicScore {
    let Some(rank) = index.checked_add(1).and_then(NonZeroUsize::new) else {
        return DeterministicScore::ZERO;
    };
    rrf_score_one_based(rank, k)
}

const SCALE_RAW: i128 = 4_294_967_296; // 2^32 — matches DeterministicScore::SCALE

/// Weighted sum of `scores`. Errors on length mismatch or non-finite weights.
#[inline]
pub fn weighted_sum(
    scores: &[DeterministicScore],
    weights: &[f64],
) -> Result<DeterministicScore, ScoreError> {
    if scores.len() != weights.len() {
        return Err(ScoreError::LengthMismatch {
            expected_desc: "scores and weights must have same length",
            first_len: scores.len(),
            second_len: weights.len(),
        });
    }
    let mut acc = 0i128;
    for (index, (&score, &weight)) in scores.iter().zip(weights.iter()).enumerate() {
        if !weight.is_finite() {
            return Err(ScoreError::NonFiniteWeight { index });
        }
        let w = DeterministicScore::from_f64(weight);
        acc += (score.to_raw() as i128 * w.to_raw() as i128) / SCALE_RAW;
    }
    Ok(DeterministicScore::from_raw(acc.clamp(
        DeterministicScore::NEG_INF.to_raw() as i128,
        DeterministicScore::MAX.to_raw() as i128,
    ) as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: f64) -> DeterministicScore {
        DeterministicScore::from_f64(v)
    }

    #[test]
    fn sum_basic() {
        let scores = [s(0.1), s(0.2), s(0.3)];
        let result = sum_scores(&scores);
        assert!((result.to_f64() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn sum_empty() {
        let result = sum_scores(&[]);
        assert_eq!(result, DeterministicScore::ZERO);
    }

    #[test]
    fn avg_basic() {
        let scores = [s(0.1), s(0.2), s(0.3)];
        let result = avg_scores(&scores);
        assert!((result.to_f64() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn rrf_basic() {
        let r1 = rrf_score(1, 60);
        let r2 = rrf_score(2, 60);
        assert!(r1 > r2);
        assert!((r1.to_f64() - 1.0 / 61.0).abs() < 1e-9);
    }

    #[test]
    fn weighted_sum_basic() {
        let scores = [s(0.5), s(1.0)];
        let weights = [0.4, 0.6];
        let result = weighted_sum(&scores, &weights).unwrap();
        assert!((result.to_f64() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn weighted_sum_length_mismatch() {
        let err = weighted_sum(&[s(0.1)], &[0.5, 0.5]).unwrap_err();
        assert!(matches!(err, ScoreError::LengthMismatch { .. }));
    }

    #[test]
    fn weighted_sum_rejects_nan() {
        let err = weighted_sum(&[s(0.1)], &[f64::NAN]).unwrap_err();
        assert!(matches!(err, ScoreError::NonFiniteWeight { index: 0 }));
    }

    #[test]
    fn sum_negative_saturation_clamps_to_neg_inf() {
        let big_neg = DeterministicScore::NEG_INF;
        let result = sum_scores(&[big_neg, big_neg, big_neg]);
        assert_eq!(result, DeterministicScore::NEG_INF);
        assert!(result.is_infinite());
        assert_eq!(result.to_f64(), f64::NEG_INFINITY);
    }

    #[test]
    fn avg_negative_saturation_clamps_to_neg_inf() {
        let big_neg = DeterministicScore::NEG_INF;
        let result = avg_scores(&[big_neg, big_neg]);
        assert_eq!(result, DeterministicScore::NEG_INF);
    }

    #[test]
    fn sum_order_independent() {
        let a = DeterministicScore::from_f64(1e9);
        let b = DeterministicScore::from_f64(-1e9);
        let c = DeterministicScore::from_f64(0.5);
        let r1 = sum_scores(&[a, b, c]);
        let r2 = sum_scores(&[c, a, b]);
        let r3 = sum_scores(&[b, c, a]);
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
    }

    #[test]
    fn avg_scores_checked_empty_returns_zero_no_flag() {
        let (mean, flag) = avg_scores_checked(&[]);
        assert_eq!(mean, DeterministicScore::ZERO);
        assert!(!flag);
    }

    #[test]
    fn avg_scores_checked_near_saturation_sets_flag() {
        let (_, flag) = avg_scores_checked(&[DeterministicScore::MAX, DeterministicScore::MAX]);
        assert!(flag);
    }

    #[test]
    fn max_score_empty_returns_neg_inf() {
        assert_eq!(max_score(&[]), DeterministicScore::NEG_INF);
    }

    #[test]
    fn min_score_empty_returns_max() {
        assert_eq!(min_score(&[]), DeterministicScore::MAX);
    }

    #[test]
    fn rrf_score_zero_denominator_returns_zero() {
        assert_eq!(rrf_score(0, 0), DeterministicScore::ZERO);
    }

    #[test]
    fn rrf_score_overflow_returns_zero() {
        assert_eq!(rrf_score(usize::MAX, 1), DeterministicScore::ZERO);
    }

    #[test]
    fn weighted_sum_empty_returns_zero() {
        assert_eq!(weighted_sum(&[], &[]).unwrap(), DeterministicScore::ZERO);
    }

    #[test]
    fn weighted_sum_rejects_infinite_weight() {
        let err = weighted_sum(&[s(1.0)], &[f64::INFINITY]).unwrap_err();
        assert_eq!(err, ScoreError::NonFiniteWeight { index: 0 });
    }

    // ── rrf_score_one_based / rrf_score_zero_based ────────────────────────────

    #[test]
    fn rrf_one_based_rank_1_equals_legacy_rank_1() {
        use std::num::NonZeroUsize;
        let one_based = rrf_score_one_based(NonZeroUsize::new(1).unwrap(), 60);
        let legacy = rrf_score(1, 60);
        assert_eq!(
            one_based, legacy,
            "rrf_score_one_based(1,60) must match rrf_score(1,60)"
        );
    }

    #[test]
    fn rrf_zero_based_index_0_equals_one_based_rank_1() {
        use std::num::NonZeroUsize;
        let zero_based = rrf_score_zero_based(0, 60);
        let one_based = rrf_score_one_based(NonZeroUsize::new(1).unwrap(), 60);
        assert_eq!(zero_based, one_based);
    }

    #[test]
    fn rrf_one_based_monotone_decreasing() {
        use std::num::NonZeroUsize;
        let r1 = rrf_score_one_based(NonZeroUsize::new(1).unwrap(), 60);
        let r2 = rrf_score_one_based(NonZeroUsize::new(2).unwrap(), 60);
        let r10 = rrf_score_one_based(NonZeroUsize::new(10).unwrap(), 60);
        assert!(r1 > r2);
        assert!(r2 > r10);
    }

    #[test]
    fn rrf_one_based_value_matches_formula() {
        use std::num::NonZeroUsize;
        let score = rrf_score_one_based(NonZeroUsize::new(1).unwrap(), 60);
        assert!((score.to_f64() - 1.0 / 61.0).abs() < 1e-9);
    }

    #[test]
    fn rrf_one_based_overflow_returns_zero() {
        use std::num::NonZeroUsize;
        let score = rrf_score_one_based(NonZeroUsize::new(usize::MAX).unwrap(), 1);
        assert_eq!(score, DeterministicScore::ZERO);
    }

    #[test]
    fn rrf_zero_based_overflow_returns_zero() {
        let score = rrf_score_zero_based(usize::MAX, 1);
        assert_eq!(score, DeterministicScore::ZERO);
    }

    // ── avg_scores_checked order-independence ─────────────────────────────────

    #[test]
    fn avg_scores_checked_saturation_flag_is_order_independent() {
        // Build two orderings of the same multiset.
        // The near_saturation flag must be the same regardless of order.
        let big = DeterministicScore::from_raw(i64::MAX / 2);
        let neg = DeterministicScore::from_raw(i64::MIN / 2 + 1);
        let scores_order_a = [big, neg, big, neg];
        let scores_order_b = [big, big, neg, neg];
        let (_, flag_a) = avg_scores_checked(&scores_order_a);
        let (_, flag_b) = avg_scores_checked(&scores_order_b);
        assert_eq!(
            flag_a, flag_b,
            "near_saturation flag must be order-independent: order_a={flag_a}, order_b={flag_b}"
        );
    }

    #[test]
    fn avg_scores_checked_normal_values_no_saturation_flag() {
        let scores = [s(0.1), s(0.2), s(-0.1), s(0.3)];
        let (_, flag) = avg_scores_checked(&scores);
        assert!(!flag, "small scores should not trigger near_saturation");
    }
}
