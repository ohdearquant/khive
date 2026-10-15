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

/// Canonical cosine conversion with the f32 endpoint tolerance used by
/// sqlite-vec's distance arithmetic. ANN consumers pass cosine distance here
/// as well, so both routes produce the same fixed-point score contract.
pub fn try_cosine_score_with_f32_tolerance(
    distance: f64,
) -> Result<DeterministicScore, ScoreError> {
    const BOUNDARY_EPSILON: f64 = 8.0 * f32::EPSILON as f64;
    if !distance.is_finite() {
        return Err(ScoreError::NonFiniteDistance);
    }
    if !(-BOUNDARY_EPSILON..=2.0 + BOUNDARY_EPSILON).contains(&distance) {
        return Err(ScoreError::InvalidDistanceRange {
            metric_name: "Cosine",
            dist_bits: (distance as f32).to_bits(),
        });
    }
    try_score_from_distance(distance.clamp(0.0, 2.0) as f32, DistanceMetric::Cosine)
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
#[path = "distance_tests.rs"]
mod tests;
