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
