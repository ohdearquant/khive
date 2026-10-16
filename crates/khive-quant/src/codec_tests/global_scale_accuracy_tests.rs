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
