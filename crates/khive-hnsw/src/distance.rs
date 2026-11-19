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
#[path = "distance/tests.rs"]
mod tests;
