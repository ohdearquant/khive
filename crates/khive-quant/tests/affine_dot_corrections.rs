//! Regression inputs for the SQ8 affine dot correction.

use khive_quant::{EncodedVector, Sq8Codec};

const NARROW: f32 = 255.0 / 16384.0;
const NARROW_SQ: f32 = 65025.0 / 268435456.0;

fn assert_exact_reconstruction(codec: &Sq8Codec, encoded: &EncodedVector, original: &[f32]) {
    for (d, &value) in original.iter().enumerate() {
        let reconstructed =
            f64::from(codec.min[d]) + f64::from(codec.scale[d]) * f64::from(encoded.codes[d]);
        assert_eq!(reconstructed, f64::from(value), "dimension {d}");
    }
}

#[test]
fn zero_endpoint_self_dot_is_zero() {
    let codec = Sq8Codec::train(&[vec![-255.0, -NARROW], vec![0.0, 0.0]]);
    let zero = codec.encode(&[0.0, 0.0]);
    assert_eq!(codec.scale, vec![1.0, 1.0 / 16384.0]);
    assert_eq!(zero.codes, vec![255, 255]);
    assert_exact_reconstruction(&codec, &zero, &[0.0, 0.0]);
    assert_eq!(
        codec.approx_dot(&zero, &zero),
        0.0,
        "exact zero endpoint must not acquire a dot-product bias"
    );
}

#[test]
fn zero_endpoint_is_zero_in_both_row_and_dimension_orders() {
    for swap_dimensions in [false, true] {
        for reverse_rows in [false, true] {
            let mut lower = vec![-255.0, -NARROW];
            if swap_dimensions {
                lower.reverse();
            }
            let mut rows = vec![lower, vec![0.0, 0.0]];
            if reverse_rows {
                rows.reverse();
            }
            let codec = Sq8Codec::train(&rows);
            let zero = codec.encode(&[0.0, 0.0]);
            assert_exact_reconstruction(&codec, &zero, &[0.0, 0.0]);
            assert_eq!(
                codec.approx_dot(&zero, &zero),
                0.0,
                "swap_dimensions={swap_dimensions}, reverse_rows={reverse_rows}"
            );
        }
    }
}

#[test]
fn flat_training_zero_endpoint_self_dot_is_zero() {
    let codec = Sq8Codec::train_flat(&[-255.0, -NARROW, 0.0, 0.0], 2);
    let zero = codec.encode(&[0.0, 0.0]);
    assert_eq!(zero.codes, vec![255, 255]);
    assert_exact_reconstruction(&codec, &zero, &[0.0, 0.0]);
    assert_eq!(codec.approx_dot(&zero, &zero), 0.0);
}

#[test]
fn large_signed_zero_endpoint_self_dot_is_zero() {
    let codec = Sq8Codec::train(&[vec![-261120.0, -255.0], vec![0.0, 0.0]]);
    let zero = codec.encode(&[0.0, 0.0]);
    assert_eq!(codec.scale, vec![1024.0, 1.0]);
    assert_eq!(zero.codes, vec![255, 255]);
    assert_exact_reconstruction(&codec, &zero, &[0.0, 0.0]);
    assert_eq!(codec.approx_dot(&zero, &zero), 0.0);
}

// The zero-valued wide coordinate leaves the narrow signed self-dot intact.
#[test]
fn signed_nonzero_endpoint_self_dot_is_preserved() {
    let codec = Sq8Codec::train(&[vec![-255.0, -NARROW], vec![0.0, 0.0]]);
    let q = codec.encode(&[0.0, -NARROW]);
    assert_eq!(q.codes, vec![255, 0]);
    assert_exact_reconstruction(&codec, &q, &[0.0, -NARROW]);
    assert_eq!(codec.approx_dot(&q, &q), NARROW_SQ);
}

#[test]
fn anisotropic_endpoint_self_dot_preserves_tiny_dimension() {
    let codec = Sq8Codec::train(&[vec![-255.0 * 1_048_576.0, -NARROW], vec![0.0, 0.0]]);
    let q = codec.encode(&[0.0, -NARROW]);
    assert_eq!(codec.scale, vec![1_048_576.0, 1.0 / 16_384.0]);
    assert_eq!(q.codes, vec![255, 0]);
    assert_exact_reconstruction(&codec, &q, &[0.0, -NARROW]);

    let dot = codec.approx_dot(&q, &q);
    let error = (dot - NARROW_SQ).abs();
    assert!(
        error <= 1e-8,
        "expected narrow self-dot {NARROW_SQ}, got {dot}, error {error}"
    );
}

#[test]
fn signed_large_offset_retains_unit_self_dot() {
    let codec = Sq8Codec::train(&[vec![-261120.0, -255.0], vec![0.0, 0.0]]);
    let q = codec.encode(&[0.0, -1.0]);
    assert_eq!(q.codes, vec![255, 254]);
    assert_exact_reconstruction(&codec, &q, &[0.0, -1.0]);
    assert_eq!(codec.approx_dot(&q, &q), 1.0);
}

#[test]
fn zero_minimum_dot_control_keeps_original_fix() {
    let codec = Sq8Codec::train(&[vec![0.0, 0.0], vec![255.0, NARROW]]);
    let q = codec.encode(&[0.0, NARROW]);
    assert_eq!(q.codes, vec![0, 255]);
    assert_exact_reconstruction(&codec, &q, &[0.0, NARROW]);
    assert_eq!(codec.approx_dot(&q, &q), NARROW_SQ);
}
