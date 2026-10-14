use super::*;

fn rand_vecs(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut h = seed;
    (0..n)
        .map(|_| {
            (0..dims)
                .map(|_| {
                    h = h
                        .wrapping_mul(0x6c62_272e_07bb_0142)
                        .wrapping_add(0x62b8_2175_62d9_6b1a);
                    let bits = (h >> 33) as u32;
                    (bits as f32) / (u32::MAX as f32) * 2.0 - 1.0
                })
                .collect()
        })
        .collect()
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

// ── QUANT-AUD-002: validation regression tests ──────────────────────────

#[test]
fn sq8_try_train_ragged_rows_returns_error_not_panic() {
    let vecs = vec![vec![0.0], vec![1.0, 2.0]];
    let err = Sq8Codec::try_train(&vecs).expect_err("ragged rows must be rejected");
    assert_eq!(
        err,
        QuantError::RaggedRow {
            row: 1,
            expected: 1,
            got: 2
        }
    );
}

#[test]
fn gs_try_train_ragged_rows_returns_error_not_panic() {
    let vecs = vec![vec![0.0], vec![1.0, 2.0]];
    let err = GsSq8Codec::try_train(&vecs).expect_err("ragged rows must be rejected");
    assert_eq!(
        err,
        QuantError::RaggedRow {
            row: 1,
            expected: 1,
            got: 2
        }
    );
}

#[test]
fn sq8_try_train_empty_corpus_returns_error() {
    let vecs: Vec<Vec<f32>> = vec![];
    assert_eq!(
        Sq8Codec::try_train(&vecs).unwrap_err(),
        QuantError::EmptyCorpus
    );
}

#[test]
fn sq8_try_train_flat_zero_dims_returns_error() {
    assert_eq!(
        Sq8Codec::try_train_flat(&[1.0, 2.0], 0).unwrap_err(),
        QuantError::ZeroDims
    );
}

#[test]
fn gs_try_train_flat_zero_dims_returns_error() {
    assert_eq!(
        GsSq8Codec::try_train_flat(&[1.0, 2.0], 0).unwrap_err(),
        QuantError::ZeroDims
    );
}

#[test]
fn sq8_try_train_flat_remainder_returns_error() {
    // 5 elements is not a multiple of dims=2.
    let err = Sq8Codec::try_train_flat(&[1.0, 2.0, 3.0, 4.0, 5.0], 2).unwrap_err();
    assert_eq!(err, QuantError::FlatLengthNotDivisible { len: 5, dims: 2 });
}

#[test]
fn gs_try_train_flat_remainder_returns_error() {
    let err = GsSq8Codec::try_train_flat(&[1.0, 2.0, 3.0, 4.0, 5.0], 2).unwrap_err();
    assert_eq!(err, QuantError::FlatLengthNotDivisible { len: 5, dims: 2 });
}

#[test]
fn sq8_try_encode_shorter_input_returns_error_not_malformed_vector() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let err = codec.try_encode(&[0.0, 1.0]).unwrap_err();
    assert_eq!(
        err,
        QuantError::EncodeLengthMismatch {
            expected: 4,
            got: 2
        }
    );
}

#[test]
fn sq8_try_encode_longer_input_returns_error() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let err = codec.try_encode(&[0.0, 1.0, 2.0, 3.0, 4.0]).unwrap_err();
    assert_eq!(
        err,
        QuantError::EncodeLengthMismatch {
            expected: 4,
            got: 5
        }
    );
}

#[test]
fn gs_try_encode_empty_input_returns_error_not_malformed_vector() {
    // QUANT-AUD-002: previously encode(&[]) on a trained (dims=1) codec
    // silently returned an EMPTY code vector in release builds (the
    // length check was debug_assert-only), which is_in_distribution
    // vacuously accepted and l2_sq scored as 0.0. Must now be a typed
    // error, never a malformed zero-length code vector.
    let codec = GsSq8Codec::train_flat(&[1.0, 2.0, 3.0, 4.0], 1);
    let err = codec.try_encode(&[]).unwrap_err();
    assert_eq!(
        err,
        QuantError::EncodeLengthMismatch {
            expected: 1,
            got: 0
        }
    );
}

#[test]
fn sq8_try_encode_flat_par_zero_dims_returns_error() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    assert_eq!(
        codec.try_encode_flat_par(&[1.0, 2.0], 0).unwrap_err(),
        QuantError::ZeroDims
    );
}

#[test]
fn gs_try_encode_flat_par_zero_dims_returns_error() {
    let codec = GsSq8Codec::train(&rand_vecs(10, 4, 1));
    assert_eq!(
        codec.try_encode_flat_par(&[1.0, 2.0], 0).unwrap_err(),
        QuantError::ZeroDims
    );
}

#[test]
fn sq8_try_encode_flat_par_remainder_returns_error() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let flat: Vec<f32> = (0..9).map(|i| i as f32).collect(); // 9 not a multiple of 4
    assert_eq!(
        codec.try_encode_flat_par(&flat, 4).unwrap_err(),
        QuantError::FlatLengthNotDivisible { len: 9, dims: 4 }
    );
}

#[test]
fn sq8_try_encode_flat_par_dims_mismatch_returns_error() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let flat: Vec<f32> = (0..6).map(|i| i as f32).collect();
    let err = codec.try_encode_flat_par(&flat, 3).unwrap_err();
    assert_eq!(
        err,
        QuantError::EncodeLengthMismatch {
            expected: 4,
            got: 3
        }
    );
}

#[test]
#[should_panic(expected = "row 1 has length 2, expected 1")]
fn sq8_train_still_panics_with_typed_message_on_ragged_rows() {
    // The panicking convenience wrapper is preserved for existing callers,
    // but must now surface the typed QuantError message rather than
    // panicking from a raw out-of-bounds index.
    let _ = Sq8Codec::train(&[vec![0.0], vec![1.0, 2.0]]);
}

#[test]
fn sq8_try_train_and_encode_roundtrip_matches_panicking_api() {
    let vecs = rand_vecs(20, 8, 7);
    let a = Sq8Codec::train(&vecs);
    let b = Sq8Codec::try_train(&vecs).expect("valid corpus must train");
    assert_eq!(a.min, b.min);
    assert_eq!(a.scale, b.scale);
    let ea = a.encode(&vecs[0]);
    let eb = b.try_encode(&vecs[0]).expect("valid vector must encode");
    assert_eq!(ea.codes, eb.codes);
}

// ── Sq8Codec tests ──────────────────────────────────────────────────────

#[test]
fn encode_decode_roundtrip_is_bounded() {
    let vecs = rand_vecs(100, 32, 42);
    let codec = Sq8Codec::train(&vecs);
    for v in &vecs {
        let ev = codec.encode(v);
        assert_eq!(ev.codes.len(), v.len());
        for (d, &code) in ev.codes.iter().enumerate() {
            let decoded = code as f32 * codec.scale[d] + codec.min[d];
            let err = (decoded - v[d]).abs();
            assert!(
                err <= codec.scale[d] + 1e-5,
                "dim {d}: err={err} scale={}",
                codec.scale[d]
            );
        }
    }
}

#[test]
fn approx_dot_relative_error_bounded() {
    let vecs = rand_vecs(200, 64, 77);
    let codec = Sq8Codec::train(&vecs);
    let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

    let mut max_rel_err = 0.0f32;
    for i in 0..vecs.len() {
        for j in (i + 1)..vecs.len().min(i + 10) {
            let true_dot = dot_f32(&vecs[i], &vecs[j]);
            let approx = codec.approx_dot(&encoded[i], &encoded[j]);
            let denom = true_dot.abs().max(1e-3);
            let rel = (approx - true_dot).abs() / denom;
            if rel > max_rel_err {
                max_rel_err = rel;
            }
        }
    }
    assert!(
        max_rel_err < 0.15,
        "max relative dot error {max_rel_err:.4} >= 0.15"
    );
}

#[test]
fn approx_l2_sq_relative_error_bounded() {
    let vecs = rand_vecs(200, 64, 88);
    let codec = Sq8Codec::train(&vecs);
    let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

    let mut max_rel_err = 0.0f32;
    for i in 0..vecs.len() {
        for j in (i + 1)..vecs.len().min(i + 10) {
            let true_l2 = l2_sq_f32(&vecs[i], &vecs[j]);
            let approx = codec.approx_l2_sq(&encoded[i], &encoded[j]);
            let denom = true_l2.max(1e-6);
            let rel = (approx - true_l2).abs() / denom;
            if rel > max_rel_err {
                max_rel_err = rel;
            }
        }
    }
    assert!(
        max_rel_err < 0.15,
        "max relative L2² error {max_rel_err:.4} >= 0.15"
    );
}

#[test]
fn narrow_sq8_dimensions_survive_a_wide_training_dimension() {
    for dims in [2, 17, 384, 768, 1536, 3072] {
        let zero = vec![0.0_f32; dims];
        let mut upper = vec![255.0_f32 / 16384.0; dims];
        upper[0] = 255.0;
        let mut narrow = upper.clone();
        narrow[0] = 0.0;
        let codec = Sq8Codec::train(&[zero.clone(), upper]);
        let q = codec.encode(&zero);
        let b = codec.encode(&narrow);
        assert!(q.codes.iter().all(|&code| code == 0));
        assert_eq!(b.codes[0], 0);
        assert!(b.codes[1..].iter().all(|&code| code == 255));

        let expected = ((dims - 1) as f64 * 65025.0 / 268435456.0) as f32;
        let distance = codec.approx_l2_sq(&q, &b);
        assert_eq!(distance, expected, "dims={dims}");
        assert!(distance > codec.approx_l2_sq(&q, &q), "dims={dims}");
        assert_eq!(codec.approx_dot(&b, &b), expected, "dims={dims}");
        if dims == 2 {
            assert!(codec.approx_cosine_dist(&b, &b) < 1e-6);
        }
    }
}

#[test]
fn order_preservation_triplets_cosine() {
    let vecs = rand_vecs(300, 64, 99);
    let codec = Sq8Codec::train(&vecs);
    let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

    let n = vecs.len();
    let mut agree = 0usize;
    let mut total = 0usize;

    for anchor in 0..50 {
        let a = &vecs[anchor];
        let ea = &encoded[anchor];
        for b_idx in 0..n {
            for c_idx in (b_idx + 1)..n.min(b_idx + 5) {
                let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                let norm_b: f32 = vecs[b_idx].iter().map(|x| x * x).sum::<f32>().sqrt();
                let norm_c: f32 = vecs[c_idx].iter().map(|x| x * x).sum::<f32>().sqrt();

                let cos_ab = dot_f32(a, &vecs[b_idx]) / (norm_a * norm_b).max(1e-9);
                let cos_ac = dot_f32(a, &vecs[c_idx]) / (norm_a * norm_c).max(1e-9);
                let dist_ab_true = 1.0 - cos_ab;
                let dist_ac_true = 1.0 - cos_ac;

                let dist_ab_approx = codec.approx_cosine_dist(ea, &encoded[b_idx]);
                let dist_ac_approx = codec.approx_cosine_dist(ea, &encoded[c_idx]);

                if (dist_ab_true - dist_ac_true).abs() < 0.01 {
                    continue;
                }

                let true_closer_b = dist_ab_true < dist_ac_true;
                let approx_closer_b = dist_ab_approx < dist_ac_approx;
                if true_closer_b == approx_closer_b {
                    agree += 1;
                }
                total += 1;
            }
        }
    }

    let rate = agree as f64 / total.max(1) as f64;
    assert!(
        rate >= 0.95,
        "order preservation {rate:.3} < 0.95 ({agree}/{total})"
    );
}

#[test]
fn order_preservation_triplets_l2() {
    let vecs = rand_vecs(300, 64, 101);
    let codec = Sq8Codec::train(&vecs);
    let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

    let n = vecs.len();
    let mut agree = 0usize;
    let mut total = 0usize;

    for anchor in 0..50 {
        let a = &vecs[anchor];
        let ea = &encoded[anchor];
        for b_idx in 0..n {
            for c_idx in (b_idx + 1)..n.min(b_idx + 5) {
                let dist_ab_true = l2_sq_f32(a, &vecs[b_idx]);
                let dist_ac_true = l2_sq_f32(a, &vecs[c_idx]);

                let dist_ab_approx = codec.approx_l2_sq(ea, &encoded[b_idx]);
                let dist_ac_approx = codec.approx_l2_sq(ea, &encoded[c_idx]);

                if (dist_ab_true - dist_ac_true).abs() < 0.001 {
                    continue;
                }

                let true_closer_b = dist_ab_true < dist_ac_true;
                let approx_closer_b = dist_ab_approx < dist_ac_approx;
                if true_closer_b == approx_closer_b {
                    agree += 1;
                }
                total += 1;
            }
        }
    }

    let rate = agree as f64 / total.max(1) as f64;
    assert!(
        rate >= 0.95,
        "L2 order preservation {rate:.3} < 0.95 ({agree}/{total})"
    );
}

#[test]
fn train_flat_matches_train_rows() {
    let vecs = rand_vecs(50, 16, 123);
    let flat: Vec<f32> = vecs.iter().flatten().copied().collect();

    let codec_rows = Sq8Codec::train(&vecs);
    let codec_flat = Sq8Codec::train_flat(&flat, 16);

    for d in 0..16 {
        assert!((codec_rows.min[d] - codec_flat.min[d]).abs() < 1e-6);
        assert!((codec_rows.scale[d] - codec_flat.scale[d]).abs() < 1e-6);
    }
}

#[test]
fn encode_par_matches_sequential() {
    let vecs = rand_vecs(50, 32, 555);
    let codec = Sq8Codec::train(&vecs);

    let seq: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();
    let par = codec.encode_par(&vecs);

    assert_eq!(seq.len(), par.len());
    for (s, p) in seq.iter().zip(par.iter()) {
        assert_eq!(s.codes, p.codes);
        assert!((s.soc_sum - p.soc_sum).abs() < 1e-5);
    }
}

#[test]
fn sq8_try_encode_par_short_row_returns_error_not_panic() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let mut rows = rand_vecs(5, 4, 2);
    rows[3] = vec![0.0, 1.0];
    let err = codec.try_encode_par(&rows).unwrap_err();
    assert_eq!(
        err,
        QuantError::EncodeLengthMismatch {
            expected: 4,
            got: 2
        }
    );
}

#[test]
fn sq8_try_encode_par_long_row_returns_error_not_panic() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let mut rows = rand_vecs(5, 4, 2);
    rows[3] = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
    let err = codec.try_encode_par(&rows).unwrap_err();
    assert_eq!(
        err,
        QuantError::EncodeLengthMismatch {
            expected: 4,
            got: 6
        }
    );
}

#[test]
#[should_panic(expected = "vector length 2 does not match codec dims 4")]
fn sq8_encode_par_still_panics_with_typed_message_on_short_row() {
    let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
    let mut rows = rand_vecs(5, 4, 2);
    rows[3] = vec![0.0, 1.0];
    let _ = codec.encode_par(&rows);
}

#[test]
fn u8_dot_u32_matches_scalar() {
    let a: Vec<u8> = (0u8..=255).take(384).collect();
    let b: Vec<u8> = (0u8..=255).rev().take(384).collect();
    let scalar: u32 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| x as u32 * y as u32)
        .sum();
    assert_eq!(u8_dot_u32(&a, &b), scalar, "u8_dot_u32 mismatch");
}

#[test]
#[should_panic(expected = "u8_dot_u32 inputs must have equal length")]
fn u8_dot_u32_rejects_shorter_second_slice() {
    let a = [1u8; 16];
    let b = [2u8; 1];

    let _ = u8_dot_u32(&a, &b);
}

#[test]
#[should_panic(expected = "u8_dot_u32 inputs must have equal length")]
fn u8_dot_u32_rejects_longer_second_slice() {
    let a = [1u8; 1];
    let b = [2u8; 16];

    let _ = u8_dot_u32(&a, &b);
}

#[test]
#[should_panic(expected = "approx_dot input codes must match codec dims")]
fn approx_dot_rejects_mismatched_codes() {
    let vectors = rand_vecs(2, 16, 1);
    let codec = Sq8Codec::train(&vectors);
    let a = codec.encode(&vectors[0]);
    let mut b = codec.encode(&vectors[1]);
    b.codes.pop();

    let _ = codec.approx_dot(&a, &b);
}

#[test]
#[should_panic(expected = "approx_dot input codes must match codec dims")]
fn approx_dot_rejects_longer_first_codes() {
    let vectors = rand_vecs(2, 16, 1);
    let codec = Sq8Codec::train(&vectors);
    let mut a = codec.encode(&vectors[0]);
    let b = codec.encode(&vectors[1]);
    a.codes.push(0);

    let _ = codec.approx_dot(&a, &b);
}

#[test]
#[should_panic(expected = "approx_l2_sq input codes must match codec dims")]
fn approx_l2_sq_rejects_mismatched_codes() {
    let vectors = rand_vecs(2, 16, 1);
    let codec = Sq8Codec::train(&vectors);
    let a = codec.encode(&vectors[0]);
    let mut b = codec.encode(&vectors[1]);
    b.codes.pop();

    let _ = codec.approx_l2_sq(&a, &b);
}

#[test]
#[should_panic(expected = "approx_l2_sq input codes must match codec dims")]
fn approx_l2_sq_rejects_longer_first_codes() {
    let vectors = rand_vecs(2, 16, 1);
    let codec = Sq8Codec::train(&vectors);
    let mut a = codec.encode(&vectors[0]);
    let b = codec.encode(&vectors[1]);
    a.codes.push(0);

    let _ = codec.approx_l2_sq(&a, &b);
}

#[test]
fn u8_helpers_tail_path_max_diff() {
    for len in [1usize, 7, 15, 17, 100, 383] {
        let a = vec![255u8; len];
        let b = vec![0u8; len];
        assert_eq!(u8_l2sq_u32(&a, &b), len as u32 * 255 * 255, "l2 len={len}");
        assert_eq!(u8_dot_u32(&a, &a), len as u32 * 255 * 255, "dot len={len}");
    }
}

#[test]
fn u8_l2sq_u32_matches_scalar() {
    let a: Vec<u8> = (0u8..=255).take(384).collect();
    let b: Vec<u8> = (0u8..=255).rev().take(384).collect();
    let scalar: u32 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = (x as i32) - (y as i32);
            (d * d) as u32
        })
        .sum();
    assert_eq!(u8_l2sq_u32(&a, &b), scalar, "u8_l2sq_u32 mismatch");
}

#[test]
#[should_panic(expected = "u8_l2sq_u32 inputs must have equal length")]
fn u8_l2sq_u32_rejects_shorter_second_slice() {
    let a = [1u8; 16];
    let b = [2u8; 1];

    let _ = u8_l2sq_u32(&a, &b);
}

// ── GsSq8Codec tests ────────────────────────────────────────────────────

/// Regression counterexample (2026-06-12): ranges [0,1] and [0,1e6].
///
/// Without global-scale, the per-dim fast path reversed near/far ordering by >6 OOM.
/// With GsSq8Codec the global scale is dominated by the wide dim; the narrow dim
/// loses code resolution but contributes proportionally little to L2 — ordering is preserved.
#[test]
fn gs_l2_sq_anisotropic_ordering_preserved() {
    let corpus = vec![
        vec![0.0f32, 0.0f32],    // origin
        vec![1.0f32, 1.0f32],    // near: exact L2² = 2.0
        vec![1.0f32, 4001.0f32], // far: exact L2² ~ 16_000_002
    ];
    let codec = GsSq8Codec::train(&corpus);

    let enc_origin = codec.encode(&corpus[0]);
    let enc_near = codec.encode(&corpus[1]);
    let enc_far = codec.encode(&corpus[2]);

    let d_near = codec.l2_sq(&enc_origin, &enc_near);
    let d_far = codec.l2_sq(&enc_origin, &enc_far);

    assert!(
        d_near < d_far,
        "GsSq8Codec reversed near/far on anisotropic corpus: near={d_near} far={d_far} \
             (anisotropy_ratio={:.1})",
        codec.anisotropy_ratio
    );
}

#[test]
fn gs_l2_sq_isotropic_small_error() {
    let vecs = rand_vecs(200, 64, 202);
    let codec = GsSq8Codec::train(&vecs);
    let encoded: Vec<GsEncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

    let mut max_rel = 0.0f32;
    for i in 0..vecs.len() {
        for j in (i + 1)..vecs.len().min(i + 10) {
            let true_l2 = l2_sq_f32(&vecs[i], &vecs[j]);
            let approx = codec.l2_sq(&encoded[i], &encoded[j]);
            let denom = true_l2.max(1e-6);
            let rel = (approx - true_l2).abs() / denom;
            if rel > max_rel {
                max_rel = rel;
            }
        }
    }
    assert!(
        max_rel < 0.15,
        "GsSq8Codec max relative L2² error {max_rel:.4} >= 0.15"
    );
}

#[test]
fn gs_train_flat_matches_train_rows() {
    let vecs = rand_vecs(50, 16, 321);
    let flat: Vec<f32> = vecs.iter().flatten().copied().collect();

    let codec_rows = GsSq8Codec::train(&vecs);
    let codec_flat = GsSq8Codec::train_flat(&flat, 16);

    assert!((codec_rows.gs - codec_flat.gs).abs() < 1e-7);
    for d in 0..16 {
        assert!((codec_rows.min[d] - codec_flat.min[d]).abs() < 1e-6);
    }
}

#[test]
fn gs_l2_sq_order_preservation_triplets() {
    let vecs = rand_vecs(300, 64, 303);
    let codec = GsSq8Codec::train(&vecs);
    let encoded: Vec<GsEncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

    let n = vecs.len();
    let mut agree = 0usize;
    let mut total = 0usize;

    for anchor in 0..50 {
        let a = &vecs[anchor];
        let ea = &encoded[anchor];
        for b_idx in 0..n {
            for c_idx in (b_idx + 1)..n.min(b_idx + 5) {
                let dist_ab_true = l2_sq_f32(a, &vecs[b_idx]);
                let dist_ac_true = l2_sq_f32(a, &vecs[c_idx]);

                let dist_ab_approx = codec.l2_sq(ea, &encoded[b_idx]);
                let dist_ac_approx = codec.l2_sq(ea, &encoded[c_idx]);

                if (dist_ab_true - dist_ac_true).abs() < 0.001 {
                    continue;
                }

                let true_closer_b = dist_ab_true < dist_ac_true;
                let approx_closer_b = dist_ab_approx < dist_ac_approx;
                if true_closer_b == approx_closer_b {
                    agree += 1;
                }
                total += 1;
            }
        }
    }

    let rate = agree as f64 / total.max(1) as f64;
    assert!(
        rate >= 0.95,
        "GsSq8Codec L2 order preservation {rate:.3} < 0.95 ({agree}/{total})"
    );
}
