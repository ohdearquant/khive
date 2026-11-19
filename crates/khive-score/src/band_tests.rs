use super::*;

#[test]
fn bands_keep_order_cardinality_and_explicit_degenerate_policies() {
    assert!(min_max_band(&[], 0.0, 1.0, 0.0, DegenerateSpan::Ceiling)
        .unwrap()
        .is_empty());
    assert_eq!(
        min_max_band(&[9.0, 1.0, 5.0], -2.0, 2.0, 0.0, DegenerateSpan::Ceiling).unwrap(),
        [2.0, -2.0, 0.0]
    );
    for policy in [DegenerateSpan::Ceiling, DegenerateSpan::UpperEndpoint] {
        assert_eq!(
            min_max_band(&[0.2, 0.2], 0.1, 0.5, 0.0, policy).unwrap(),
            [0.5, 0.5]
        );
    }
    assert_eq!(
        min_max_band(&[0.2], 0.1, 0.5, 0.0, DegenerateSpan::ClampMaximum).unwrap(),
        [0.2]
    );
    assert_eq!(
        min_max_band(
            &[0.2, 0.2 + f64::EPSILON],
            0.1,
            0.5,
            f64::EPSILON,
            DegenerateSpan::Ceiling
        )
        .unwrap(),
        [0.5, 0.5]
    );
}

#[test]
fn f32_upper_endpoint_is_not_simplified_or_widened() {
    let upper = min_max_band_f32(
        &[0.2],
        0.15,
        0.82,
        f32::EPSILON,
        DegenerateSpan::UpperEndpoint,
    )
    .unwrap();
    let ceiling =
        min_max_band_f32(&[0.2], 0.15, 0.82, f32::EPSILON, DegenerateSpan::Ceiling).unwrap();
    assert_eq!(upper[0].to_bits(), 0x3f51_eb84);
    assert_eq!(ceiling[0].to_bits(), 0x3f51_eb85);
    assert_eq!(
        min_max_band_f32(&[2.0, 1.0], -1.0, 1.0, 0.0, DegenerateSpan::Ceiling).unwrap(),
        [1.0, -1.0]
    );
    let ordinary =
        min_max_band_f32(&[0.0, 1.0, 9.0], 0.15, 0.9, 0.0, DegenerateSpan::Ceiling).unwrap();
    assert_eq!(ordinary[1].to_bits(), 0x3e6e_eef0);
    let fused = (1.0f32 / 9.0).mul_add(0.9f32 - 0.15, 0.15);
    assert_eq!(fused.to_bits(), 0x3e6e_eeef);
    assert_eq!(
        min_max_band_f32(
            &[0.01],
            0.5,
            0.82,
            f32::EPSILON,
            DegenerateSpan::ClampMaximum
        )
        .unwrap(),
        [0.5]
    );
}

#[test]
fn invalid_inputs_and_overflow_refuse_the_whole_band() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(min_max_band(&[0.0, value], 0.0, 1.0, 0.0, DegenerateSpan::Ceiling).is_err());
        assert!(min_max_band(&[], value, 1.0, 0.0, DegenerateSpan::Ceiling).is_err());
        assert!(min_max_band(&[0.0], 0.0, 1.0, value, DegenerateSpan::Ceiling).is_err());
    }
    assert_eq!(
        min_max_band(&[0.0], 1.0, 0.0, 0.0, DegenerateSpan::Ceiling),
        Err(ArithmeticError::InvalidBounds)
    );
    assert!(min_max_band(&[0.0], 0.0, 1.0, -1.0, DegenerateSpan::Ceiling).is_err());
    assert_eq!(
        min_max_band(
            &[-f64::MAX, f64::MAX],
            0.0,
            1.0,
            0.0,
            DegenerateSpan::Ceiling
        ),
        Err(ArithmeticError::Overflow {
            operation: "band span"
        })
    );
    assert_eq!(
        min_max_band(&[0.0], -f64::MAX, f64::MAX, 0.0, DegenerateSpan::Ceiling),
        Err(ArithmeticError::Overflow {
            operation: "band range"
        })
    );
    assert!(min_max_band_f32(
        &[-f32::MAX, f32::MAX],
        0.0,
        1.0,
        0.0,
        DegenerateSpan::Ceiling
    )
    .is_err());
}

#[test]
fn fixed_band_preserves_integer_division_and_extreme_raw_behavior() {
    let normalize = |raw: &[i64]| {
        min_max_band_fixed(
            &raw.iter()
                .copied()
                .map(DeterministicScore::from_raw)
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(DeterministicScore::to_raw)
        .collect::<Vec<_>>()
    };
    assert!(normalize(&[]).is_empty());
    assert_eq!(normalize(&[0, 1, 3]), [0, 1_431_655_765, 4_294_967_296]);
    assert_eq!(normalize(&[-3, -2, 0]), [0, 1_431_655_765, 4_294_967_296]);
    assert_eq!(
        normalize(&[i64::MAX, i64::MIN, i64::MIN + 1]),
        [4_294_967_296, 0, 0]
    );
    assert_eq!(normalize(&[i64::MIN]), [4_294_967_296]);
    assert_eq!(normalize(&[9, 9]), [4_294_967_296, 4_294_967_296]);
}
