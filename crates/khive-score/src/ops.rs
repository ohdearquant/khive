//! Aggregation and fusion operations for deterministic scores.

use crate::DeterministicScore;
use std::fmt;
use std::num::NonZeroUsize;

/// Validation errors from aggregation and distance conversion.
///
/// See `crates/khive-score/docs/api/aggregation-and-ranking.md` and
/// `crates/khive-score/docs/api/distance-conversion.md`.
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

/// Return the mean and whether absolute input mass or the result approaches saturation.
#[inline]
pub fn avg_scores_checked(scores: &[DeterministicScore]) -> (DeterministicScore, bool) {
    if scores.is_empty() {
        return (DeterministicScore::ZERO, false);
    }
    const SATURATION_THRESHOLD: i128 = (i64::MAX as i128) * 9 / 10;
    let sum: i128 = scores.iter().map(|s| s.to_raw() as i128).sum();
    let mean = sum / scores.len() as i128;
    // Absolute mass makes the warning independent of iteration order and sign cancellation.
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

/// Return a saturating weighted sum.
///
/// Errors on length mismatch or the first non-finite weight. See
/// `crates/khive-score/docs/api/aggregation-and-ranking.md`.
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
#[path = "ops_tests.rs"]
mod tests;
