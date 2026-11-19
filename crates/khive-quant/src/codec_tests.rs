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

include!("codec_tests/sq8_accuracy_tests.rs");

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

include!("codec_tests/distance_contract_tests.rs");

include!("codec_tests/global_scale_accuracy_tests.rs");
