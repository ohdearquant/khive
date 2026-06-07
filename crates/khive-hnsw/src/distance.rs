//! Distance computation for HNSW — Cosine, Dot, and L2 metrics.

use super::config::DistanceMetric;
// Use the lossy variant: input vectors are validated as finite at the insert boundary,
// so NaN distances from valid inputs are not expected. `score_from_distance_lossy` maps
// NaN → 0 rather than a perfect score, which is the safer backstop for any residual case.
pub(crate) use khive_score::score_from_distance_lossy as score_from_distance;

/// Compute cosine distance from pre-computed dot product and norms.
/// Clamps to [-1, 1]; returns [0, 2]; falls back to 1.0 for zero/infinite norms.
#[inline]
pub(crate) fn cosine_distance_from_parts(dot: f32, a_norm: f32, b_norm: f32) -> f32 {
    let denom = a_norm * b_norm;
    if !denom.is_finite() || denom <= 0.0 {
        return 1.0;
    }
    let cosine = (dot / denom).clamp(-1.0, 1.0);
    if cosine.is_finite() {
        1.0 - cosine
    } else {
        1.0
    }
}

/// Compute distance between two vectors using SIMD-accelerated implementations.
/// Returns distance (lower = more similar) for heap operations.
#[inline]
pub fn compute_distance(
    a: &[f32],
    a_norm: f32,
    b: &[f32],
    b_norm: f32,
    metric: DistanceMetric,
) -> f32 {
    if !a_norm.is_finite() || !b_norm.is_finite() {
        return 1.0;
    }

    match metric {
        DistanceMetric::Cosine => {
            // **PROOF CORRESPONDENCE**: khive.Retrieval.Cosine.cosine_sim_bounded
            // Cosine similarity is bounded: -1 <= cos(x,y) <= 1 for unit vectors
            //
            // **PROOF CORRESPONDENCE**: khive.Retrieval.Cosine.cauchy_schwarz
            // Cauchy-Schwarz inequality: |<x,y>| <= ||x|| * ||y||
            let dot = lattice_embed::simd::dot_product(a, b);
            cosine_distance_from_parts(dot, a_norm, b_norm)
        }
        DistanceMetric::Dot => {
            // Negate for min-heap (higher dot = lower distance)
            -lattice_embed::simd::dot_product(a, b)
        }
        DistanceMetric::L2 => {
            // **PROOF CORRESPONDENCE**: khive.Retrieval.Distance.euclidean_nonneg
            // Euclidean distance is non-negative: d(x,y) >= 0
            //
            // **PROOF CORRESPONDENCE**: khive.Retrieval.Distance.euclidean_symm
            // Euclidean distance is symmetric: d(x,y) = d(y,x)
            //
            // **PROOF CORRESPONDENCE**: khive.Retrieval.Distance.euclidean_triangle
            // Triangle inequality: d(x,z) <= d(x,y) + d(y,z)
            lattice_embed::simd::euclidean_distance(a, b)
        }
        _ => {
            // DistanceMetric is #[non_exhaustive]; fall back to cosine for
            // any future variants until explicitly supported.
            let dot = lattice_embed::simd::dot_product(a, b);
            cosine_distance_from_parts(dot, a_norm, b_norm)
        }
    }
}

/// Monotone ordering distance for HNSW internals; not a true distance.
/// For L2 returns squared Euclidean (skips sqrt); for other metrics identical to `compute_distance`.
#[inline]
pub(crate) fn compute_ordering_distance(
    a: &[f32],
    a_norm: f32,
    b: &[f32],
    b_norm: f32,
    metric: DistanceMetric,
) -> f32 {
    match metric {
        DistanceMetric::L2 => lattice_embed::simd::squared_euclidean_distance(a, b),
        other => compute_distance(a, a_norm, b, b_norm, other),
    }
}

/// Ordered wrapper for f32; NaN treated as greater than all finite values.
#[derive(Clone, Copy, PartialEq)]
pub struct OrderedF32(pub f32);

impl Eq for OrderedF32 {}

impl PartialOrd for OrderedF32 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedF32 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Handle NaN: treat as greater than all finite values
        // This ensures fail-safe behavior: NaN results get pushed to the end
        match (self.0.is_nan(), other.0.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => {
                // SAFETY: Both values are confirmed non-NaN by the match guards above.
                // For non-NaN f32 values (including infinity), partial_cmp is total
                // and always returns Some. This is a mathematical invariant of IEEE 754.
                self.0
                    .partial_cmp(&other.0)
                    .expect("both values are non-NaN, partial_cmp should succeed")
            }
        }
    }
}

impl OrderedF32 {
    /// Check if the wrapped value is NaN.
    #[inline]
    // REASON: `is_nan` is a predicate helper for callers that need to detect
    // NaN-valued distances (e.g. debug assertions, test helpers). Not yet wired
    // into a production caller but useful as a test utility alongside `OrderedF32`.
    #[allow(dead_code)]
    pub fn is_nan(&self) -> bool {
        self.0.is_nan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // RETRIEVAL-M3: Cosine distance clamping tests
    // =========================================================================

    /// Verify that cosine_distance_from_parts clamps correctly when fp rounding
    /// produces a dot/norm ratio slightly outside [-1, 1].
    #[test]
    fn test_cosine_distance_from_parts_clamping() {
        // Normal case: identical direction => distance 0
        assert!((cosine_distance_from_parts(1.0, 1.0, 1.0) - 0.0).abs() < 1e-6);

        // Normal case: opposite direction => distance 2
        assert!((cosine_distance_from_parts(-1.0, 1.0, 1.0) - 2.0).abs() < 1e-6);

        // Rounding artifact: dot slightly > denom (should clamp cosine to 1.0 => dist 0)
        let dist = cosine_distance_from_parts(1.0000002, 1.0, 1.0);
        assert!(
            dist >= 0.0,
            "distance must be non-negative after clamping, got {dist}"
        );
        assert!(
            dist <= 2.0,
            "distance must be <= 2 after clamping, got {dist}"
        );

        // Zero norms => fallback 1.0
        assert_eq!(cosine_distance_from_parts(0.0, 0.0, 1.0), 1.0);
        assert_eq!(cosine_distance_from_parts(0.0, 1.0, 0.0), 1.0);

        // Infinite denom => fallback 1.0
        assert_eq!(
            cosine_distance_from_parts(f32::NAN, f32::INFINITY, 1.0),
            1.0
        );
    }

    #[test]
    fn test_cosine_distance() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0];
        let a_norm = 1.0;
        let b_norm = 1.0;

        let dist = compute_distance(&a, a_norm, &b, b_norm, DistanceMetric::Cosine);
        assert!(dist.abs() < 0.001); // Same vector = 0 distance

        let c = vec![0.0, 1.0];
        let dist = compute_distance(&a, a_norm, &c, 1.0, DistanceMetric::Cosine);
        assert!((dist - 1.0).abs() < 0.001); // Orthogonal = 1 distance
    }

    #[test]
    fn test_euclidean_distance() {
        let a = vec![0.0, 0.0];
        let b = vec![3.0, 4.0];

        let dist = compute_distance(&a, 0.0, &b, 5.0, DistanceMetric::L2);
        assert!((dist - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_dot_product_distance() {
        let a = vec![1.0, 2.0];
        let b = vec![2.0, 3.0];

        let dist = compute_distance(&a, 0.0, &b, 0.0, DistanceMetric::Dot);
        // dot = 1*2 + 2*3 = 8, distance = -8
        assert!((dist - (-8.0)).abs() < 0.001);
    }

    #[test]
    fn test_score_from_distance() {
        use khive_score::DeterministicScore;
        // f32 input loses precision on widening to f64; use 1e-6 tolerance.
        // Cosine: similarity = 1 - distance
        assert!((score_from_distance(0.2, DistanceMetric::Cosine).to_f64() - 0.8).abs() < 1e-6);

        // Dot: similarity = -distance
        assert!((score_from_distance(-5.0, DistanceMetric::Dot).to_f64() - 5.0).abs() < 1e-6);

        // Euclidean: similarity = 1/(1+distance)
        assert!((score_from_distance(1.0, DistanceMetric::L2).to_f64() - 0.5).abs() < 1e-6);

        // NaN input: score_from_distance_lossy maps invalid distance to NEG_INF
        // (ranks last rather than getting a spurious perfect score).
        assert_eq!(
            score_from_distance(f32::NAN, DistanceMetric::Cosine),
            DeterministicScore::NEG_INF
        );
    }

    #[test]
    fn test_ordered_f32() {
        let a = OrderedF32(1.0);
        let b = OrderedF32(2.0);
        assert!(a < b);
        assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
    }

    // =========================================================================
    // RETRIEVAL-02: NaN Handling Tests
    // =========================================================================

    #[test]
    fn test_ordered_f32_nan_handling() {
        let nan = OrderedF32(f32::NAN);
        let finite = OrderedF32(1.0);
        let infinity = OrderedF32(f32::INFINITY);
        let neg_infinity = OrderedF32(f32::NEG_INFINITY);

        // NaN is greater than all finite values
        assert!(nan > finite);
        assert!(finite < nan);

        // NaN is greater than infinity
        assert!(nan > infinity);
        assert!(infinity < nan);

        // NaN is greater than negative infinity
        assert!(nan > neg_infinity);

        // Two NaNs are equal
        let nan2 = OrderedF32(f32::NAN);
        assert_eq!(nan.cmp(&nan2), std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_ordered_f32_sorting_with_nan() {
        // When sorting distances for nearest neighbor search, NaN should end up last
        let mut distances = [
            OrderedF32(0.5),
            OrderedF32(f32::NAN),
            OrderedF32(0.1),
            OrderedF32(0.9),
            OrderedF32(f32::NAN),
            OrderedF32(0.3),
        ];

        // Sort ascending (for min-heap behavior)
        distances.sort();

        // Non-NaN values should be at the front, sorted
        assert_eq!(distances[0].0, 0.1);
        assert_eq!(distances[1].0, 0.3);
        assert_eq!(distances[2].0, 0.5);
        assert_eq!(distances[3].0, 0.9);
        // NaN values should be at the end
        assert!(distances[4].is_nan());
        assert!(distances[5].is_nan());
    }

    #[test]
    fn test_ordered_f32_deterministic_ordering() {
        // Same values should always produce same ordering
        for _ in 0..10 {
            let values = vec![OrderedF32(0.5), OrderedF32(0.5), OrderedF32(0.1)];

            let mut sorted = values.clone();
            sorted.sort();

            assert_eq!(sorted[0].0, 0.1);
            assert_eq!(sorted[1].0, 0.5);
            assert_eq!(sorted[2].0, 0.5);
        }
    }

    #[test]
    fn test_ordered_f32_infinity() {
        let a = OrderedF32(f32::INFINITY);
        let b = OrderedF32(f32::NEG_INFINITY);
        let c = OrderedF32(0.0);

        assert!(a > c);
        assert!(b < c);
        assert!(a > b);
    }
}
