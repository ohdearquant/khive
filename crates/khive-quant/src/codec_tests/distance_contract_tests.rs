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
