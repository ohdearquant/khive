use super::*;

#[test]
fn test_config_default() {
    let config = Bm25Config::default();
    assert!((config.k1 - 1.2).abs() < f64::EPSILON);
    assert!((config.b - 0.75).abs() < f64::EPSILON);
}

#[test]
fn test_config_validation() {
    assert!(Bm25Config::new(1.2, 0.75).validate().is_ok());
    assert!(Bm25Config::new(-0.1, 0.75).validate().is_err());
    assert!(Bm25Config::new(1.2, -0.1).validate().is_err());
    assert!(Bm25Config::new(1.2, 1.5).validate().is_err());
}

#[test]
fn test_config_nan_rejected() {
    assert!(
        Bm25Config::new(f64::NAN, 0.75).validate().is_err(),
        "NaN k1 must be rejected"
    );
    assert!(
        Bm25Config::new(1.2, f64::NAN).validate().is_err(),
        "NaN b must be rejected"
    );
}

#[test]
fn test_config_inf_rejected() {
    assert!(
        Bm25Config::new(f64::INFINITY, 0.75).validate().is_err(),
        "Inf k1 must be rejected"
    );
    assert!(
        Bm25Config::new(f64::NEG_INFINITY, 0.75).validate().is_err(),
        "NegInf k1 must be rejected"
    );
    assert!(
        Bm25Config::new(1.2, f64::INFINITY).validate().is_err(),
        "Inf b must be rejected"
    );
}

#[test]
fn test_config_custom() {
    let config = Bm25Config::new(2.0, 0.5);
    assert!((config.k1 - 2.0).abs() < f64::EPSILON);
    assert!((config.b - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_serde_rejects_negative_k1() {
    let json = r#"{"k1": -0.5, "b": 0.75}"#;
    let result: Result<Bm25Config, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "negative k1 must be rejected at deserialization"
    );
}

#[test]
fn test_serde_rejects_b_above_one() {
    let json = r#"{"k1": 1.2, "b": 1.5}"#;
    let result: Result<Bm25Config, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "b > 1.0 must be rejected at deserialization"
    );
}

#[test]
fn test_serde_rejects_b_below_zero() {
    let json = r#"{"k1": 1.2, "b": -0.1}"#;
    let result: Result<Bm25Config, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "b < 0.0 must be rejected at deserialization"
    );
}

#[test]
fn test_serde_accepts_valid_config() {
    let json = r#"{"k1": 2.0, "b": 0.5}"#;
    let config: Bm25Config = serde_json::from_str(json).expect("valid config");
    assert!((config.k1 - 2.0).abs() < f64::EPSILON);
    assert!((config.b - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_serde_roundtrip_preserves_budget() {
    let config = Bm25Config::default().with_memory_budget(10_000);
    let json = serde_json::to_string(&config).unwrap();
    let restored: Bm25Config = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.memory_budget, Some(10_000));
}

#[test]
fn test_serde_accepts_boundary_values() {
    // k1=0 and b=0 are valid
    let json = r#"{"k1": 0.0, "b": 0.0}"#;
    assert!(serde_json::from_str::<Bm25Config>(json).is_ok());

    // b=1.0 is valid
    let json = r#"{"k1": 1.2, "b": 1.0}"#;
    assert!(serde_json::from_str::<Bm25Config>(json).is_ok());
}
