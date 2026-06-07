//! Distance primitives for the Vamana ANN index.

use crate::error::{Result, VamanaError};

/// Squared L2 distance (hot path). Panics if lengths differ; callers must guarantee equality.
#[inline]
pub(crate) fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "l2_squared requires equal-length slices");

    let mut s0 = 0.0f32;
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    let mut s3 = 0.0f32;
    let mut s4 = 0.0f32;
    let mut s5 = 0.0f32;
    let mut s6 = 0.0f32;
    let mut s7 = 0.0f32;

    let chunks_a = a.chunks_exact(8);
    let chunks_b = b.chunks_exact(8);
    let rem_a = chunks_a.remainder();
    let rem_b = chunks_b.remainder();

    for (ca, cb) in chunks_a.zip(chunks_b) {
        let d0 = ca[0] - cb[0];
        let d1 = ca[1] - cb[1];
        let d2 = ca[2] - cb[2];
        let d3 = ca[3] - cb[3];
        let d4 = ca[4] - cb[4];
        let d5 = ca[5] - cb[5];
        let d6 = ca[6] - cb[6];
        let d7 = ca[7] - cb[7];
        s0 += d0 * d0;
        s1 += d1 * d1;
        s2 += d2 * d2;
        s3 += d3 * d3;
        s4 += d4 * d4;
        s5 += d5 * d5;
        s6 += d6 * d6;
        s7 += d7 * d7;
    }

    let mut sum = s0 + s1 + s2 + s3 + s4 + s5 + s6 + s7;
    for (x, y) in rem_a.iter().zip(rem_b.iter()) {
        let d = x - y;
        sum += d * d;
    }
    sum
}

/// Compute squared L2 distance between two slices, returning an error on length mismatch.
#[inline]
pub fn try_l2_squared(a: &[f32], b: &[f32]) -> Result<f32> {
    if a.len() != b.len() {
        return Err(VamanaError::DimensionMismatch {
            expected: a.len(),
            actual: b.len(),
        });
    }
    Ok(l2_squared(a, b))
}

/// Convert a squared L2 distance to cosine similarity for unit-normalized vectors.
#[inline]
pub fn cosine_from_l2sq(l2sq: f32) -> f32 {
    1.0 - (0.5 * l2sq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_squared_matches_naive_loop() {
        let a: Vec<f32> = (0..13).map(|i| i as f32 * 0.1).collect();
        let b: Vec<f32> = (0..13).map(|i| i as f32 * 0.15 + 0.03).collect();

        let naive: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum();
        let fast = l2_squared(&a, &b);
        assert!((fast - naive).abs() < 1e-5, "fast={fast} naive={naive}");
    }

    #[test]
    fn l2_squared_identical_vectors_is_zero() {
        let a = vec![0.5f32, 0.3, 0.8, 0.1, 0.4, 0.9, 0.2, 0.7, 0.6];
        assert_eq!(l2_squared(&a, &a), 0.0);
    }

    #[test]
    fn l2_squared_orthogonal_unit_vectors_is_two() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let d = l2_squared(&a, &b);
        assert!((d - 2.0).abs() < 1e-6, "got {d}");
    }

    #[test]
    fn cosine_from_l2sq_matches_unit_dot_product() {
        let a = vec![1.0f32 / 2.0f32.sqrt(), 1.0f32 / 2.0f32.sqrt()];
        let b = vec![1.0f32 / 2.0f32.sqrt(), -1.0f32 / 2.0f32.sqrt()];
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let cosine_via_l2sq = cosine_from_l2sq(l2_squared(&a, &b));
        assert!(
            (cosine_via_l2sq - dot).abs() < 1e-5,
            "cosine_via_l2sq={cosine_via_l2sq} dot={dot}"
        );
    }

    #[test]
    fn try_l2_squared_returns_err_on_length_mismatch() {
        assert!(matches!(
            try_l2_squared(&[1.0], &[1.0, 2.0]),
            Err(VamanaError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn try_l2_squared_returns_err_empty_vs_nonempty() {
        assert!(matches!(
            try_l2_squared(&[1.0], &[]),
            Err(VamanaError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn try_l2_squared_matches_naive_loop() {
        let a: Vec<f32> = (0..8).map(|i| i as f32 * 0.1).collect();
        let b: Vec<f32> = (0..8).map(|i| i as f32 * 0.15 + 0.03).collect();
        let naive: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum();
        let fast = try_l2_squared(&a, &b).unwrap();
        assert!((fast - naive).abs() < 1e-5, "fast={fast} naive={naive}");
    }
}
