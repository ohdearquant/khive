use super::*;

#[test]
fn roundtrip_f64() {
    let s = DeterministicScore::from_f64(0.5);
    assert!((s.to_f64() - 0.5).abs() < 1e-9);
}

#[test]
fn nan_maps_to_zero() {
    let s = DeterministicScore::from_f64(f64::NAN);
    assert_eq!(s, DeterministicScore::ZERO);
    assert!((s.to_f64() - 0.0).abs() < 1e-15);
}

#[test]
fn infinity() {
    assert_eq!(
        DeterministicScore::from_f64(f64::INFINITY),
        DeterministicScore::MAX
    );
    assert_eq!(
        DeterministicScore::from_f64(f64::NEG_INFINITY),
        DeterministicScore::NEG_INF
    );
}

#[test]
fn ordering() {
    let a = DeterministicScore::from_f64(0.1);
    let b = DeterministicScore::from_f64(0.5);
    assert!(a < b);
}

#[test]
fn arithmetic() {
    let a = DeterministicScore::from_f64(0.3);
    let b = DeterministicScore::from_f64(0.4);
    let sum = a + b;
    assert!((sum.to_f64() - 0.7).abs() < 1e-9);
}

#[test]
fn scaling_by_f64() {
    let s = DeterministicScore::from_f64(0.5);
    let scaled = s * 2.0;
    assert!((scaled.to_f64() - 1.0).abs() < 1e-9);
}

#[test]
fn div_by_zero() {
    let pos = DeterministicScore::from_f64(0.5);
    assert_eq!(pos / 0, DeterministicScore::MAX);
    let neg = DeterministicScore::from_f64(-0.5);
    assert_eq!(neg / 0, DeterministicScore::NEG_INF);
    assert_eq!(DeterministicScore::ZERO / 0, DeterministicScore::ZERO);
}

#[test]
fn raw_scale_known_value() {
    assert_eq!(
        DeterministicScore::from_f64(1.0).to_raw(),
        4_294_967_296_i64
    );
}

#[test]
fn display_formatting() {
    let s = format!("{}", DeterministicScore::from_f64(0.1234567));
    assert_eq!(s, "0.123457");
    assert_eq!(format!("{}", DeterministicScore::MAX), "+Inf");
    assert_eq!(format!("{}", DeterministicScore::NEG_INF), "-Inf");
}

#[test]
fn debug_formatting() {
    assert_eq!(
        format!("{:?}", DeterministicScore::MAX),
        "DeterministicScore(+Inf)"
    );
    assert_eq!(
        format!("{:?}", DeterministicScore::NEG_INF),
        "DeterministicScore(-Inf)"
    );
    let s = format!("{:?}", DeterministicScore::from_f64(0.5));
    assert!(
        s.starts_with("DeterministicScore(0.5"),
        "unexpected debug output: {s}"
    );
}

#[test]
fn add_saturates_at_max() {
    assert_eq!(
        DeterministicScore::MAX + DeterministicScore::from_raw(1),
        DeterministicScore::MAX
    );
}

#[test]
fn sub_saturates_at_neg_inf() {
    assert_eq!(
        DeterministicScore::NEG_INF - DeterministicScore::from_raw(1),
        DeterministicScore::NEG_INF
    );
}

#[test]
fn mul_i64_saturates_at_max() {
    let large = DeterministicScore::from_raw(i64::MAX / 2);
    assert_eq!(large * 3_i64, DeterministicScore::MAX);
}

#[test]
fn mul_f64_nan_yields_zero() {
    let s = DeterministicScore::from_f64(1.0);
    assert_eq!(s * f64::NAN, DeterministicScore::ZERO);
}

// NEG_INF = i64::MIN + 1; MIN (i64::MIN) is reserved sentinel (Lean: `MIN`)
#[test]
fn neg_inf_is_i64_min_plus_one() {
    assert_eq!(DeterministicScore::NEG_INF.to_raw(), i64::MIN + 1);
}

#[test]
fn min_sentinel_is_i64_min() {
    assert_eq!(DeterministicScore::MIN.to_raw(), i64::MIN);
}

#[test]
fn min_sentinel_distinct_from_neg_inf() {
    assert_ne!(DeterministicScore::MIN, DeterministicScore::NEG_INF);
    assert!(DeterministicScore::MIN < DeterministicScore::NEG_INF);
}

#[test]
fn neg_infinity_maps_to_neg_inf() {
    assert_eq!(
        DeterministicScore::from_f64(f64::NEG_INFINITY),
        DeterministicScore::NEG_INF
    );
}

#[test]
fn underflow_clamps_to_neg_inf_not_min() {
    // Arithmetic must clamp at NEG_INF (= i64::MIN + 1), never produce MIN.
    let result = DeterministicScore::from_raw(i64::MIN + 1) - DeterministicScore::from_raw(1);
    assert_eq!(result, DeterministicScore::NEG_INF);
    assert_ne!(result, DeterministicScore::MIN);
}

#[test]
fn from_raw_saturating_min_maps_to_neg_inf() {
    let s = DeterministicScore::from_raw_saturating(i64::MIN);
    assert_eq!(
        s,
        DeterministicScore::NEG_INF,
        "i64::MIN must saturate to NEG_INF, not the reserved MIN sentinel"
    );
}

#[test]
fn from_raw_saturating_neg_inf_raw_is_identity() {
    let s = DeterministicScore::from_raw_saturating(i64::MIN + 1);
    assert_eq!(s, DeterministicScore::NEG_INF);
}

#[test]
fn from_raw_saturating_normal_value_is_identity() {
    let s = DeterministicScore::from_raw_saturating(42);
    assert_eq!(s.to_raw(), 42);
}

#[test]
fn from_raw_checked_min_returns_none() {
    assert!(
        DeterministicScore::from_raw_checked(i64::MIN).is_none(),
        "i64::MIN should be rejected by from_raw_checked"
    );
}

#[test]
fn from_raw_checked_valid_value_returns_some() {
    let s = DeterministicScore::from_raw_checked(i64::MIN + 1).unwrap();
    assert_eq!(s, DeterministicScore::NEG_INF);
}

#[test]
fn from_raw_checked_zero_returns_some() {
    let s = DeterministicScore::from_raw_checked(0).unwrap();
    assert_eq!(s, DeterministicScore::ZERO);
}

#[cfg(feature = "serde")]
#[test]
fn serde_deserialize_rejects_i64_min() {
    let raw_json = format!("{}", i64::MIN);
    let result: Result<DeterministicScore, _> = serde_json::from_str(&raw_json);
    assert!(
        result.is_err(),
        "deserializing i64::MIN must fail: got {:?}",
        result
    );
}

#[cfg(feature = "serde")]
#[test]
fn serde_deserialize_accepts_neg_inf_raw() {
    let raw_json = format!("{}", i64::MIN + 1);
    let s: DeterministicScore = serde_json::from_str(&raw_json).unwrap();
    assert_eq!(s, DeterministicScore::NEG_INF);
}

#[cfg(feature = "serde")]
#[test]
fn serde_roundtrip_zero() {
    let original = DeterministicScore::ZERO;
    let json = serde_json::to_string(&original).unwrap();
    let restored: DeterministicScore = serde_json::from_str(&json).unwrap();
    assert_eq!(original, restored);
}
