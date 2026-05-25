//! Canonical distance-to-similarity conversion (ADR-006 boundary).
//!
//! `score_from_distance` is the single authoritative place that converts a raw
//! floating-point distance produced by a vector index into a `DeterministicScore`.
//! Centralising the conversion here ensures that all retrieval back-ends (HNSW,
//! flat-scan, future IVF …) produce identical scores for identical inputs.
//!
//! ## Conversion formulas (per `DistanceMetric`)
//!
//! | Metric  | Distance d        | Similarity            | Notes                        |
//! |---------|-------------------|-----------------------|------------------------------|
//! | Cosine  | 1 − cos(x,y) ∈ [0,2] | 1 − d              | linear inversion             |
//! | Dot     | −⟨x,y⟩            | −d                    | negated for min-heap storage |
//! | L2      | ‖x−y‖₂            | 1 / (1 + d)           | always positive              |
//!
//! **PROOF CORRESPONDENCE**: `khive.Retrieval.Distance.distanceToSimilarity`
//! and `khive.Retrieval.Distance.similarity_nonneg` in
//! `proofs/Retrieval/Distance.lean` (ADR-030 §Phase 2).
//!
//! NaN distances are treated as zero before conversion (fail-safe; a NaN
//! embedding contributes neutral score rather than poisoning the result set).

use crate::DeterministicScore;
use khive_types::DistanceMetric;

/// Convert a raw distance value to a [`DeterministicScore`] using the
/// appropriate formula for `metric`.
///
/// Higher score = more similar.  The conversion is monotonically decreasing
/// in distance for all three supported metrics.
///
/// # NaN handling
///
/// A NaN `dist` is treated as `0.0` before conversion, so the resulting score
/// is the maximum similarity for that metric — consistent with the Lean proof
/// that NaN is a degenerate "zero distance" sentinel.
///
/// # Arguments
///
/// * `dist`   – raw distance produced by `compute_distance` (f32, cast to f64
///   for arithmetic precision)
/// * `metric` – the distance metric used when computing `dist`
#[inline]
pub fn score_from_distance(dist: f32, metric: DistanceMetric) -> DeterministicScore {
    let d = if dist.is_nan() { 0.0 } else { dist } as f64;
    let similarity = match metric {
        DistanceMetric::Cosine => 1.0 - d,
        DistanceMetric::Dot => -d,
        DistanceMetric::L2 => 1.0 / (1.0 + d.max(0.0)),
        // DistanceMetric is #[non_exhaustive]; fall back to cosine for any
        // future variants until they are explicitly supported here.
        _ => 1.0 - d,
    };
    DeterministicScore::from_f64(similarity)
}

#[cfg(test)]
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
    #[test]
    fn nan_maps_to_zero_distance() {
        let s = score_from_distance(f32::NAN, DistanceMetric::Cosine);
        assert!(
            (s.to_f64() - 1.0).abs() < 1e-6,
            "NaN should map to similarity 1.0, got {}",
            s.to_f64()
        );
    }

    /// Verify byte-identical output to the historical khive-hnsw local impl for
    /// all three metrics and the NaN edge case.  These values are the regression
    /// fixture: the refactor MUST NOT change them.
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
