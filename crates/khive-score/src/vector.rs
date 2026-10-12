//! Vector similarity with sequential f32 arithmetic.

const NEAR_ZERO_EPSILON: f32 = 1e-8;

/// Returns zero for empty or unequal vectors, or a product of norms below `1e-8`.
/// `clamp` bounds the quotient to [-1, 1]; nonfinite arithmetic is left unchanged.
pub fn cosine_similarity(a: &[f32], b: &[f32], clamp: bool) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < NEAR_ZERO_EPSILON {
        0.0
    } else if clamp {
        (dot / denom).clamp(-1.0, 1.0)
    } else {
        dot / denom
    }
}

/// Normalize a vector in place using sequential `f32` squared-sum arithmetic.
///
/// Vectors whose norm is at most `1e-8` remain unchanged. This helper does not
/// validate finite inputs; callers retain their existing validation policy.
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > NEAR_ZERO_EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cosine_similarity, l2_normalize};

    #[test]
    fn empty_mismatched_and_zero_vectors_return_zero() {
        for clamp in [false, true] {
            assert_eq!(cosine_similarity(&[], &[], clamp), 0.0);
            assert_eq!(cosine_similarity(&[], &[1.0], clamp), 0.0);
            assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0], clamp), 0.0);
            assert_eq!(cosine_similarity(&[0.0, 0.0], &[3.0, 4.0], clamp), 0.0);
            assert_eq!(cosine_similarity(&[0.0], &[0.0], clamp), 0.0);
        }
    }

    #[test]
    fn analytic_pairs_preserve_direction_and_sign() {
        for clamp in [false, true] {
            let score = cosine_similarity(&[3.0, 4.0], &[4.0, 0.0], clamp);
            assert!((score - 0.6).abs() <= f32::EPSILON);
            assert_eq!(cosine_similarity(&[3.0, 4.0], &[-4.0, 3.0], clamp), 0.0);
            assert_eq!(cosine_similarity(&[3.0, 4.0], &[-3.0, -4.0], clamp), -1.0);
        }
    }

    #[test]
    fn clamp_is_observable_on_f32_self_comparison() {
        let vector = [1.0, 1.0];
        let unclamped = cosine_similarity(&vector, &vector, false);
        assert!(unclamped > 1.0, "sequential f32 self-score was {unclamped}");
        assert_eq!(cosine_similarity(&vector, &vector, true), 1.0);
    }

    #[test]
    fn near_zero_guard_uses_the_product_of_norms() {
        for clamp in [false, true] {
            assert_eq!(cosine_similarity(&[5e-9], &[1.0], clamp), 0.0);
            assert_eq!(cosine_similarity(&[2e-5], &[2e-5], clamp), 0.0);
            assert_eq!(cosine_similarity(&[2e-8], &[1.0], clamp), 1.0);
        }
    }

    #[test]
    fn nonfinite_and_overflow_arithmetic_remain_visible() {
        for clamp in [false, true] {
            assert!(cosine_similarity(&[f32::NAN], &[1.0], clamp).is_nan());
            assert!(cosine_similarity(&[f32::INFINITY], &[1.0], clamp).is_nan());
            assert!(cosine_similarity(&[f32::MAX], &[f32::MAX], clamp).is_nan());
            assert_eq!(cosine_similarity(&[f32::MAX], &[1.0], clamp), 0.0);
        }
    }

    #[test]
    fn normalization_pins_tiny_and_boundary_vectors() {
        for input in [vec![], vec![0.0, 0.0], vec![5e-9, 0.0], vec![1e-8, 0.0]] {
            let mut actual = input.clone();
            l2_normalize(&mut actual);
            assert_eq!(actual, input);
        }
        let mut above = vec![2e-8, 0.0];
        l2_normalize(&mut above);
        assert_eq!(above, vec![1.0, 0.0]);
    }

    #[test]
    fn normalization_has_analytic_unit_length_and_direction() {
        let mut actual = vec![3.0, -4.0];
        l2_normalize(&mut actual);
        assert!((actual[0] - 0.6).abs() < 1e-6);
        assert!((actual[1] + 0.8).abs() < 1e-6);
        assert!((actual[0] * actual[0] + actual[1] * actual[1] - 1.0).abs() < 1e-6);
    }
}
