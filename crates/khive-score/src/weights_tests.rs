use super::*;

#[test]
fn ordered_weights_reject_invalid_support_and_preserve_raw_bits() {
    assert_eq!(
        ValidatedWeights::try_new(vec![]).unwrap_err(),
        ArithmeticError::EmptyWeights
    );
    assert_eq!(
        ValidatedWeights::try_new(vec![0.0, -0.0]).unwrap_err(),
        ArithmeticError::NoPositiveWeight
    );
    assert_eq!(
        ValidatedWeights::try_new(vec![1.0, -1.0]).unwrap_err(),
        ArithmeticError::OutOfRange {
            parameter: "weight"
        }
    );
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            ValidatedWeights::try_new(vec![1.0, invalid]).unwrap_err(),
            ArithmeticError::NonFinite {
                parameter: "weight"
            }
        );
    }
    let raw = vec![3.0, -0.0, 1.0];
    let weights = ValidatedWeights::try_new(raw.clone()).unwrap();
    assert_eq!(
        weights
            .as_slice()
            .iter()
            .map(|x| x.to_bits())
            .collect::<Vec<_>>(),
        raw.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(weights.normalized(), vec![0.75, -0.0, 0.25]);
    assert_eq!(weights.as_slice()[1].to_bits(), (-0.0_f64).to_bits());
}

#[test]
fn normalization_preserves_extreme_positive_support() {
    for weight in [f64::MAX, f64::from_bits(1)] {
        let weights = ValidatedWeights::try_new(vec![weight, weight]).unwrap();
        assert_eq!(weights.normalized(), vec![0.5, 0.5]);
    }
    let raw = [f64::MAX, f64::MAX];
    let naive_sum: f64 = raw.iter().sum();
    assert!(naive_sum.is_infinite());
    assert_eq!(raw[0] / naive_sum, 0.0);
}

#[test]
fn named_empty_zero_and_missing_weights_ignore_feature_values() {
    for configured in [HashMap::new(), HashMap::from([("a", -0.0)])] {
        let weights = NamedWeights::try_new(configured).unwrap();
        assert_eq!(weights.weighted_mean(&[]).unwrap(), 0.0);
        assert_eq!(
            weights
                .weighted_mean(&[("a", f64::NAN), ("missing", f64::INFINITY)])
                .unwrap(),
            0.0
        );
    }
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        assert!(NamedWeights::try_new(HashMap::from([("unknown", invalid)])).is_err());
    }
}

#[test]
fn unknown_configured_keys_do_not_set_the_weight_scale() {
    let tiny = f64::from_bits(1);
    let weights = NamedWeights::try_new(HashMap::from([
        ("a", tiny),
        ("b", tiny),
        ("unknown", f64::MAX),
    ]))
    .unwrap();
    let actual = weights.weighted_mean(&[("a", 0.6), ("b", 0.8)]).unwrap();
    assert!((actual - 0.7).abs() <= f64::EPSILON);
    assert_eq!(tiny / f64::MAX, 0.0);
}

#[test]
fn duplicate_declared_keys_refuse_even_without_active_weights() {
    for configured in [HashMap::new(), HashMap::from([("a", 0.0)])] {
        let weights = NamedWeights::try_new(configured).unwrap();
        assert_eq!(
            weights.weighted_mean(&[("a", f64::NAN), ("a", 1.0)]),
            Err(ArithmeticError::DuplicateKey)
        );
    }
    let weights = NamedWeights::try_new(HashMap::from([("a", 1.0), ("b", 1.0)])).unwrap();
    assert_eq!(
        weights.weighted_mean(&[("a", 0.5), ("b", 0.5)]).unwrap(),
        0.5
    );
}

#[test]
fn active_nonfinite_features_refuse_before_weight_ratio_underflow() {
    let weights = NamedWeights::try_new(HashMap::from([
        ("large", f64::MAX),
        ("tiny", f64::from_bits(1)),
        ("zero", 0.0),
    ]))
    .unwrap();
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            weights.weighted_mean(&[("large", 0.5), ("tiny", invalid)]),
            Err(ArithmeticError::NonFinite {
                parameter: "feature"
            })
        );
        assert_eq!(
            weights
                .weighted_mean(&[("large", 0.5), ("zero", invalid)])
                .unwrap(),
            0.5
        );
    }
}

#[test]
fn finite_extreme_features_remain_finite_without_unit_clamping() {
    let weights = NamedWeights::try_new(HashMap::from([("a", f64::MAX), ("b", f64::MAX)])).unwrap();
    for value in [f64::MAX, -f64::MAX, 4.0, -4.0] {
        assert_eq!(weights.weighted_mean(&[("a", value)]).unwrap(), value);
    }
    assert_eq!(
        weights
            .weighted_mean(&[("a", f64::MAX), ("b", -f64::MAX)])
            .unwrap(),
        0.0
    );
    assert_eq!(
        weights
            .weighted_mean(&[("a", f64::MAX), ("b", f64::MAX)])
            .unwrap(),
        f64::MAX
    );
}

#[test]
fn declared_order_controls_bits_and_finite_weight_scaling_preserves_mean() {
    let entries = [("a", 1.0), ("b", 2.0), ("c", 3.0)];
    let features = [("c", -3.0), ("a", 0.5), ("b", 4.0)];
    let forward = NamedWeights::try_new(entries.into_iter().collect()).unwrap();
    let reverse = NamedWeights::try_new(entries.into_iter().rev().collect()).unwrap();
    let scaled = NamedWeights::try_new(
        entries
            .into_iter()
            .map(|(key, weight)| (key, weight * 1e100))
            .collect(),
    )
    .unwrap();
    let actual = forward.weighted_mean(&features).unwrap();
    assert_eq!(
        actual.to_bits(),
        reverse.weighted_mean(&features).unwrap().to_bits()
    );
    assert!((actual + 1.0 / 12.0).abs() <= f64::EPSILON);
    assert!((actual - scaled.weighted_mean(&features).unwrap()).abs() <= f64::EPSILON);

    let mut numerator: f64 = 0.0;
    let mut denominator: f64 = 0.0;
    for (weight, feature) in [(3.0, -3.0), (1.0, 0.5), (2.0, 4.0)] {
        let scaled_weight = weight / 3.0;
        numerator += scaled_weight * (feature / 4.0);
        denominator += scaled_weight;
    }
    let expected = (numerator / denominator).clamp(-1.0_f64, 1.0) * 4.0;
    assert_eq!(actual.to_bits(), expected.to_bits());
}

#[test]
fn named_keys_need_no_clone_bound() {
    #[derive(Eq, PartialEq, Hash)]
    struct Key(u8);

    let weights = NamedWeights::try_new(HashMap::from([(Key(1), 2.0)])).unwrap();
    assert_eq!(weights.weighted_mean(&[(Key(1), 3.0)]).unwrap(), 3.0);
}

#[test]
fn duplicate_keys_precede_active_nonfinite_feature_errors() {
    let weights = NamedWeights::try_new(HashMap::from([("a", 1.0), ("b", 1.0)])).unwrap();
    assert_eq!(
        weights.weighted_mean(&[("a", f64::NAN), ("b", 0.5), ("b", 1.0)]),
        Err(ArithmeticError::DuplicateKey)
    );
}
