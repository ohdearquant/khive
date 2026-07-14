//! Canonical distance-to-`DeterministicScore` conversion for all retrieval backends.

use crate::{DeterministicScore, ScoreError};
use khive_types::DistanceMetric;

/// Strict metric conversion; rejects non-finite, out-of-range, and unsupported inputs.
///
/// See `crates/khive-score/docs/api/distance-conversion.md` for formulas and error variants.
pub fn try_score_from_distance(
    dist: f32,
    metric: DistanceMetric,
) -> Result<DeterministicScore, ScoreError> {
    if !dist.is_finite() {
        return Err(ScoreError::NonFiniteDistance);
    }

    let d = dist as f64;
    let similarity = match metric {
        DistanceMetric::Cosine => {
            if !(0.0_f32..=2.0_f32).contains(&dist) {
                return Err(ScoreError::InvalidDistanceRange {
                    metric_name: "Cosine",
                    dist_bits: dist.to_bits(),
                });
            }
            1.0 - d
        }
        DistanceMetric::Dot => -d,
        DistanceMetric::L2 => {
            if dist < 0.0 {
                return Err(ScoreError::InvalidDistanceRange {
                    metric_name: "L2",
                    dist_bits: dist.to_bits(),
                });
            }
            1.0 / (1.0 + d)
        }
        _ => return Err(ScoreError::UnsupportedMetric),
    };

    Ok(DeterministicScore::from_f64(similarity))
}

/// Convert a distance, mapping every invalid input to [`DeterministicScore::NEG_INF`].
///
/// See `crates/khive-score/docs/api/distance-conversion.md`.
#[inline]
pub fn score_from_distance_lossy(dist: f32, metric: DistanceMetric) -> DeterministicScore {
    try_score_from_distance(dist, metric).unwrap_or(DeterministicScore::NEG_INF)
}

/// Legacy distance conversion; NaN silently maps to a perfect score. Use `try_score_from_distance`.
#[deprecated(
    since = "0.2.3",
    note = "NaN maps to a perfect score — use `try_score_from_distance` or `score_from_distance_lossy` instead"
)]
#[inline]
pub fn score_from_distance(dist: f32, metric: DistanceMetric) -> DeterministicScore {
    let d = if dist.is_nan() { 0.0 } else { dist } as f64;
    let similarity = match metric {
        DistanceMetric::Cosine => 1.0 - d,
        DistanceMetric::Dot => -d,
        DistanceMetric::L2 => 1.0 / (1.0 + d.max(0.0)),
        // DistanceMetric is #[non_exhaustive]; unknown future variants return
        // NEG_INF so they rank last rather than silently inheriting cosine semantics.
        _ => return DeterministicScore::NEG_INF,
    };
    DeterministicScore::from_f64(similarity)
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    /// Cosine: similarity = 1 − distance.
    #[test]
    fn cosine_basic() {
        // distance 0.2 → similarity 0.8
        let s = score_from_distance(0.2, DistanceMetric::Cosine);
        assert!((s.to_f64() - 0.8).abs() < 1e-6, "got {}", s.to_f64());
    }

    /// Dot: similarity = −distance (negated min-heap value).
    #[test]
    fn dot_basic() {
        // distance −5.0 → similarity 5.0
        let s = score_from_distance(-5.0, DistanceMetric::Dot);
        assert!((s.to_f64() - 5.0).abs() < 1e-6, "got {}", s.to_f64());
    }

    /// L2: similarity = 1 / (1 + distance).
    #[test]
    fn l2_basic() {
        // distance 1.0 → similarity 0.5
        let s = score_from_distance(1.0, DistanceMetric::L2);
        assert!((s.to_f64() - 0.5).abs() < 1e-6, "got {}", s.to_f64());
    }

    /// L2: distance 0 → similarity 1.0 (identical vectors).
    #[test]
    fn l2_zero_distance() {
        let s = score_from_distance(0.0, DistanceMetric::L2);
        assert!((s.to_f64() - 1.0).abs() < 1e-6, "got {}", s.to_f64());
    }

    /// L2: large distance → similarity approaches 0.
    #[test]
    fn l2_large_distance() {
        let s = score_from_distance(1_000_000.0_f32, DistanceMetric::L2);
        assert!(s.to_f64() < 1e-5, "got {}", s.to_f64());
        assert!(s.to_f64() >= 0.0, "similarity must be non-negative");
    }

    /// Cosine: distance 0 → similarity 1.0 (identical direction).
    #[test]
    fn cosine_zero_distance() {
        let s = score_from_distance(0.0, DistanceMetric::Cosine);
        assert!((s.to_f64() - 1.0).abs() < 1e-6, "got {}", s.to_f64());
    }

    /// Cosine: distance 2.0 → similarity −1.0 (opposite vectors).
    #[test]
    fn cosine_max_distance() {
        let s = score_from_distance(2.0, DistanceMetric::Cosine);
        assert!((s.to_f64() - (-1.0)).abs() < 1e-6, "got {}", s.to_f64());
    }

    /// NaN distance → treated as 0.0 → cosine similarity 1.0.
    /// NOTE: this is legacy behaviour preserved for parity; use try_score_from_distance
    /// or score_from_distance_lossy for new code where NaN should not be a perfect match.
    #[test]
    fn nan_maps_to_zero_distance() {
        let s = score_from_distance(f32::NAN, DistanceMetric::Cosine);
        assert!(
            (s.to_f64() - 1.0).abs() < 1e-6,
            "NaN should map to similarity 1.0, got {}",
            s.to_f64()
        );
    }

    #[test]
    fn try_score_nan_returns_error() {
        let err = try_score_from_distance(f32::NAN, DistanceMetric::Cosine).unwrap_err();
        assert!(
            matches!(err, ScoreError::NonFiniteDistance),
            "NaN should produce NonFiniteDistance error"
        );
    }

    #[test]
    fn try_score_pos_inf_returns_error() {
        let err = try_score_from_distance(f32::INFINITY, DistanceMetric::L2).unwrap_err();
        assert!(matches!(err, ScoreError::NonFiniteDistance));
    }

    #[test]
    fn try_score_neg_inf_returns_error() {
        let err = try_score_from_distance(f32::NEG_INFINITY, DistanceMetric::Cosine).unwrap_err();
        assert!(matches!(err, ScoreError::NonFiniteDistance));
    }

    #[test]
    fn try_score_cosine_out_of_range_negative() {
        let err = try_score_from_distance(-0.1, DistanceMetric::Cosine).unwrap_err();
        assert!(
            matches!(
                err,
                ScoreError::InvalidDistanceRange {
                    metric_name: "Cosine",
                    ..
                }
            ),
            "negative cosine distance should be InvalidDistanceRange"
        );
    }

    #[test]
    fn try_score_cosine_out_of_range_above_two() {
        let err = try_score_from_distance(2.1, DistanceMetric::Cosine).unwrap_err();
        assert!(matches!(
            err,
            ScoreError::InvalidDistanceRange {
                metric_name: "Cosine",
                ..
            }
        ));
    }

    #[test]
    fn try_score_l2_negative_distance_returns_error() {
        let err = try_score_from_distance(-1.0, DistanceMetric::L2).unwrap_err();
        assert!(
            matches!(
                err,
                ScoreError::InvalidDistanceRange {
                    metric_name: "L2",
                    ..
                }
            ),
            "negative L2 distance should be InvalidDistanceRange"
        );
    }

    #[test]
    fn try_score_cosine_valid_succeeds() {
        let s = try_score_from_distance(0.2, DistanceMetric::Cosine).unwrap();
        assert!((s.to_f64() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn try_score_l2_valid_succeeds() {
        let s = try_score_from_distance(1.0, DistanceMetric::L2).unwrap();
        assert!((s.to_f64() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn try_score_dot_valid_succeeds() {
        let s = try_score_from_distance(-5.0, DistanceMetric::Dot).unwrap();
        assert!((s.to_f64() - 5.0).abs() < 1e-6);
    }

    #[test]
    fn lossy_nan_maps_to_neg_inf() {
        let s = score_from_distance_lossy(f32::NAN, DistanceMetric::Cosine);
        assert_eq!(
            s,
            DeterministicScore::NEG_INF,
            "lossy NaN should be NEG_INF, not a perfect match"
        );
    }

    #[test]
    fn lossy_negative_l2_maps_to_neg_inf() {
        let s = score_from_distance_lossy(-1.0, DistanceMetric::L2);
        assert_eq!(s, DeterministicScore::NEG_INF);
    }

    #[test]
    fn lossy_valid_cosine_is_correct() {
        let s = score_from_distance_lossy(0.5, DistanceMetric::Cosine);
        assert!((s.to_f64() - 0.5).abs() < 1e-6);
    }

    /// Preserve historical khive-hnsw bytes for all metrics and legacy NaN handling.
    #[test]
    fn parity_with_hnsw_local_impl() {
        // The old khive-hnsw impl used exactly:
        //   let d = if dist.is_nan() { 0.0 } else { dist } as f64;
        //   Cosine => 1.0 - d,  Dot => -d,  L2 => 1.0/(1.0+d.max(0.0))
        // reproduced here inline to make the parity assertion explicit.
        fn reference(dist: f32, metric: DistanceMetric) -> f64 {
            let d = if dist.is_nan() { 0.0 } else { dist } as f64;
            match metric {
                DistanceMetric::Cosine => 1.0 - d,
                DistanceMetric::Dot => -d,
                DistanceMetric::L2 => 1.0 / (1.0 + d.max(0.0)),
                _ => 1.0 - d,
            }
        }

        let cases: &[(f32, DistanceMetric)] = &[
            (0.0, DistanceMetric::Cosine),
            (0.2, DistanceMetric::Cosine),
            (1.0, DistanceMetric::Cosine),
            (2.0, DistanceMetric::Cosine),
            (f32::NAN, DistanceMetric::Cosine),
            (-5.0, DistanceMetric::Dot),
            (0.0, DistanceMetric::Dot),
            (3.0, DistanceMetric::Dot),
            (0.0, DistanceMetric::L2),
            (1.0, DistanceMetric::L2),
            (4.0, DistanceMetric::L2),
            (1_000_000.0, DistanceMetric::L2),
        ];

        for &(dist, metric) in cases {
            let expected = DeterministicScore::from_f64(reference(dist, metric));
            let got = score_from_distance(dist, metric);
            assert_eq!(
                got,
                expected,
                "parity failure for dist={dist:?} metric={metric:?}: \
                 expected raw={} got raw={}",
                expected.to_raw(),
                got.to_raw()
            );
        }
    }
}
