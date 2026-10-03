use super::l2_normalize;

// Literal pre-migration pack algorithm; intentionally independent of the export.
fn original_pack_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

#[test]
fn normalize_preserves_small_norm_and_exact_boundary() {
    let boundary = [1e-8_f32];
    assert_eq!(boundary.iter().map(|x| x * x).sum::<f32>().sqrt(), 1e-8);
    for input in [vec![], vec![0.0, -0.0], vec![1e-9], boundary.to_vec()] {
        let mut actual = input.clone();
        l2_normalize(&mut actual);
        assert_eq!(bits(&actual), bits(&input));
    }
    let mut above = vec![2e-8];
    l2_normalize(&mut above);
    assert_eq!(bits(&above), bits(&[1.0]));
}

#[test]
fn normalize_preserves_sequential_f32_precision_and_order() {
    let input = vec![4096.0_f32, 1.0, 1.0, 1.0, 1.0];
    let forward = input.iter().map(|x| x * x).sum::<f32>().sqrt();
    let reversed = input.iter().rev().map(|x| x * x).sum::<f32>().sqrt();
    let widened = input
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt() as f32;
    assert_ne!(forward.to_bits(), reversed.to_bits(), "order premise");
    assert_ne!(forward.to_bits(), widened.to_bits(), "precision premise");
    for input in [input, vec![3.0, 4.0], vec![-3.0, 4.0, -0.0]] {
        let mut expected = input.clone();
        original_pack_normalize(&mut expected);
        let mut actual = input;
        l2_normalize(&mut actual);
        assert_eq!(bits(&actual), bits(&expected));
    }
}

#[test]
fn normalize_retains_nonfinite_and_overflow_arithmetic() {
    for input in [
        vec![f32::from_bits(0x7fc1_2345), 1.0],
        vec![f32::INFINITY, 1.0],
        vec![f32::NEG_INFINITY, -1.0],
        vec![f32::MAX, f32::MAX],
    ] {
        let mut expected = input.clone();
        original_pack_normalize(&mut expected);
        let mut actual = input.clone();
        l2_normalize(&mut actual);
        assert_eq!(bits(&actual), bits(&expected));
        if input[0].is_infinite() {
            assert!(actual[0].is_nan());
            assert_eq!(actual[1], 0.0, "infinite norm still divides finite lanes");
        }
    }
}
