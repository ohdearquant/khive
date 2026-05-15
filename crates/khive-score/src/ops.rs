//! Aggregation and fusion operations for deterministic scores.

use crate::DeterministicScore;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreError {
    LengthMismatch {
        expected_desc: &'static str,
        first_len: usize,
        second_len: usize,
    },
    NonFiniteWeight {
        index: usize,
    },
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
        }
    }
}

impl std::error::Error for ScoreError {}

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

#[inline]
pub fn avg_scores_checked(scores: &[DeterministicScore]) -> (DeterministicScore, bool) {
    if scores.is_empty() {
        return (DeterministicScore::ZERO, false);
    }
    const SATURATION_THRESHOLD: i128 = (i64::MAX as i128) * 9 / 10;
    let mut sum = 0i128;
    let mut near_saturation = false;
    for score in scores {
        sum += score.to_raw() as i128;
        near_saturation |= sum.abs() > SATURATION_THRESHOLD;
    }
    let mean = sum / scores.len() as i128;
    near_saturation |= mean.abs() > SATURATION_THRESHOLD;
    let result = DeterministicScore::from_raw(mean.clamp(
        DeterministicScore::NEG_INF.to_raw() as i128,
        i64::MAX as i128,
    ) as i64);
    (result, near_saturation)
}

#[inline]
pub fn max_score(scores: &[DeterministicScore]) -> DeterministicScore {
    scores
        .iter()
        .copied()
        .max()
        .unwrap_or(DeterministicScore::NEG_INF)
}

#[inline]
pub fn min_score(scores: &[DeterministicScore]) -> DeterministicScore {
    scores
        .iter()
        .copied()
        .min()
        .unwrap_or(DeterministicScore::MAX)
}

/// Reciprocal Rank Fusion score: `1 / (k + rank)`.
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
    let mut acc = DeterministicScore::ZERO;
    for (index, (&score, &weight)) in scores.iter().zip(weights.iter()).enumerate() {
        if !weight.is_finite() {
            return Err(ScoreError::NonFiniteWeight { index });
        }
        acc = acc + score * weight;
    }
    Ok(acc)
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
}
