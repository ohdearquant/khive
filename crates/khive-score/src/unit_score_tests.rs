use super::*;

#[test]
fn constructors_preserve_fraction_percent_boundary_and_signed_zero() {
    for (input, expected) in [
        (0.0, 0.0),
        (0.5, 0.5),
        (1.0, 1.0),
        (50.0, 0.5),
        (100.0, 1.0),
    ] {
        assert_eq!(
            UnitScore::from_fraction_or_percent(input).unwrap().value(),
            expected
        );
    }
    let above_one = f64::from_bits(1.0f64.to_bits() + 1);
    assert_eq!(
        UnitScore::from_fraction_or_percent(above_one)
            .unwrap()
            .value(),
        above_one / 100.0
    );
    for score in [
        UnitScore::try_new(-0.0),
        UnitScore::from_fraction_or_percent(-0.0),
        UnitScore::clamp01(-0.0),
    ] {
        assert_eq!(score.unwrap().value().to_bits(), (-0.0f64).to_bits());
    }
    assert_eq!(UnitScore::clamp01(-2.0).unwrap(), UnitScore::ZERO);
    assert_eq!(UnitScore::clamp01(2.0).unwrap(), UnitScore::ONE);
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            UnitScore::try_new(invalid),
            Err(ArithmeticError::NonFinite { .. })
        ));
        assert!(UnitScore::from_fraction_or_percent(invalid).is_err());
        assert!(UnitScore::clamp01(invalid).is_err());
    }
    assert!(UnitScore::try_new(1.5).is_err());
    assert!(UnitScore::try_new(-0.1).is_err());
    assert!(UnitScore::from_fraction_or_percent(100.1).is_err());
    assert!(UnitScore::from_fraction_or_percent(-0.1).is_err());
}

#[test]
fn factors_remain_raw_and_boost_is_only_a_terminal_clamp() {
    assert_eq!(boost_factor(0.5, 1.0).unwrap(), 1.5);
    assert_eq!(boost_factor(-3.0, 1.0).unwrap(), -2.0);
    assert_eq!(
        UnitScore::try_new(0.8).unwrap().boost(1.0, 1.0).unwrap(),
        UnitScore::ONE
    );
    assert_eq!(UnitScore::ONE.boost(-3.0, 1.0).unwrap(), UnitScore::ZERO);
    let terminal = UnitScore::clamp01(0.8 * boost_factor(1.0, 1.0).unwrap() * 0.25).unwrap();
    let premature = UnitScore::try_new(0.8)
        .unwrap()
        .boost(1.0, 1.0)
        .unwrap()
        .value()
        * 0.25;
    assert_eq!(terminal.value(), 0.4);
    assert_eq!(premature, 0.25);
    assert!(UnitScore::ZERO.boost(f64::NAN, 0.0).is_err());
    assert!(matches!(
        UnitScore::ZERO.boost(f64::MAX, 2.0),
        Err(ArithmeticError::Overflow { .. })
    ));
    assert!(boost_factor_f32(f32::MAX, 2.0).is_err());
    assert!(boost_factor(f64::from(f32::MAX), 2.0).is_ok());
}

#[test]
fn affine_checks_intermediates_before_clamping_in_both_precisions() {
    assert_eq!(affine_clamped(1.0, 0.3, 0.85, 1.15).unwrap(), 1.15);
    assert_eq!(affine_clamped(-2.0, 2.0, -5.0, 2.0).unwrap(), -4.0);
    assert_eq!(affine_clamped_f32(0.5, 0.3, 0.85, 1.15).unwrap(), 1.0);
    assert_eq!(
        affine_clamped(0.5, 1.0, 2.0, 1.0),
        Err(ArithmeticError::InvalidBounds)
    );
    assert!(affine_clamped(f64::INFINITY, 0.0, 0.0, 1.0).is_err());
    assert!(affine_clamped(0.5, 0.0, f64::NAN, 1.0).is_err());
    assert!(matches!(
        affine_clamped(2.0, f64::MAX, 0.0, 1.0),
        Err(ArithmeticError::Overflow { .. })
    ));
    assert!(affine_clamped_f32(2.0, f32::MAX, 0.0, 1.0).is_err());
}

#[test]
fn fixed_conversion_uses_existing_rounding_and_refuses_nonunit_raw_values() {
    let half_step = UnitScore::try_new(2.0f64.powi(-33)).unwrap();
    assert_eq!(DeterministicScore::from(half_step).to_raw(), 1);
    for value in [0.0, 0.1, 0.5, 0.9, 1.0] {
        let score = UnitScore::try_new(value).unwrap();
        let restored = UnitScore::try_from(DeterministicScore::from(score)).unwrap();
        assert!((restored.value() - value).abs() <= 2.0f64.powi(-33));
    }
    for raw in [-1, 4_294_967_297, i64::MIN, i64::MIN + 1, i64::MAX] {
        assert!(UnitScore::try_from(DeterministicScore::from_raw(raw)).is_err());
    }
}

#[cfg(feature = "serde")]
#[test]
fn deserialization_cannot_bypass_the_unit_invariant() {
    for input in ["-0.1", "1.1", "null", "\"NaN\""] {
        assert!(serde_json::from_str::<UnitScore>(input).is_err());
    }
    let score = serde_json::from_str::<UnitScore>("-0.0").unwrap();
    assert_eq!(score.value().to_bits(), (-0.0f64).to_bits());
    assert_eq!(serde_json::to_string(&UnitScore::ONE).unwrap(), "1.0");
}
