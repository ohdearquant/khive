use super::*;

#[test]
fn test_fusion_strategy_default() {
    let default = FusionStrategy::default();
    assert_eq!(default, FusionStrategy::Rrf { k: 60 });
}

#[test]
fn test_fusion_strategy_builders() {
    assert_eq!(FusionStrategy::rrf(), FusionStrategy::Rrf { k: 60 });
    assert_eq!(
        FusionStrategy::rrf_with_k(20),
        FusionStrategy::Rrf { k: 20 }
    );
    assert_eq!(FusionStrategy::rrf_with_k(0), FusionStrategy::Rrf { k: 1 });
    assert_eq!(
        FusionStrategy::weighted(vec![0.5, 0.5]),
        FusionStrategy::Weighted {
            weights: vec![0.5, 0.5]
        }
    );
    assert_eq!(FusionStrategy::union(), FusionStrategy::Union);
}

#[test]
fn test_try_rrf_rejects_zero() {
    assert_eq!(
        FusionStrategy::try_rrf(0),
        Err(FusionStrategyError::RrfKZero)
    );
    assert!(FusionStrategy::try_rrf(1).is_ok());
    assert!(FusionStrategy::try_rrf(60).is_ok());
}

#[test]
fn test_try_weighted_rejects_nan() {
    assert_eq!(
        FusionStrategy::try_weighted(vec![0.5, f64::NAN]),
        Err(FusionStrategyError::WeightNaN)
    );
}

#[test]
fn test_try_weighted_rejects_infinity() {
    assert_eq!(
        FusionStrategy::try_weighted(vec![f64::INFINITY, 0.5]),
        Err(FusionStrategyError::WeightInfinite)
    );
    assert_eq!(
        FusionStrategy::try_weighted(vec![0.5, f64::NEG_INFINITY]),
        Err(FusionStrategyError::WeightInfinite)
    );
}

#[test]
fn test_try_weighted_accepts_valid() {
    assert!(FusionStrategy::try_weighted(vec![0.5, 0.5]).is_ok());
    assert!(FusionStrategy::try_weighted(vec![0.0, 0.0]).is_ok());
    assert!(FusionStrategy::try_weighted(vec![-1.0, 1.0]).is_ok());
    assert!(FusionStrategy::try_weighted(vec![]).is_ok());
}

#[test]
fn test_try_weighted_rrf_rejects_invalid_parameters() {
    assert_eq!(
        FusionStrategy::try_weighted_rrf(0, vec![1.0]),
        Err(FusionStrategyError::RrfKZero)
    );
    assert_eq!(
        FusionStrategy::try_weighted_rrf(1, vec![f64::NAN]),
        Err(FusionStrategyError::WeightNaN)
    );
    assert_eq!(
        FusionStrategy::try_weighted_rrf(1, vec![f64::INFINITY]),
        Err(FusionStrategyError::WeightInfinite)
    );
    assert_eq!(
        FusionStrategy::try_weighted_rrf(1, vec![0.0]),
        Err(FusionStrategyError::WeightNotPositive)
    );
    assert_eq!(
        FusionStrategy::try_weighted_rrf(1, vec![-1.0]),
        Err(FusionStrategyError::WeightNotPositive)
    );
}

#[test]
fn test_try_weighted_rrf_accepts_positive_finite_weights() {
    assert_eq!(
        FusionStrategy::try_weighted_rrf(10, vec![1.0, 3.0]),
        Ok(FusionStrategy::WeightedRrf {
            k: 10,
            weights: vec![1.0, 3.0]
        })
    );
}

#[test]
fn test_try_custom_rejects_empty_name() {
    assert_eq!(
        FusionStrategy::try_custom(String::new(), serde_json::Value::Null),
        Err(FusionStrategyError::CustomNameEmpty)
    );
}

#[test]
fn test_try_custom_accepts_valid() {
    let result = FusionStrategy::try_custom(
        "decay_weighted".to_string(),
        serde_json::json!({"decay": 0.95}),
    );
    assert!(result.is_ok());
}

#[test]
fn test_serde_roundtrip_rrf() {
    let strategy = FusionStrategy::Rrf { k: 60 };
    let json = serde_json::to_string(&strategy).unwrap();
    let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(strategy, deserialized);
}

#[test]
fn test_serde_roundtrip_weighted() {
    let strategy = FusionStrategy::Weighted {
        weights: vec![0.6, 0.4],
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(strategy, deserialized);
}

#[test]
fn test_serde_roundtrip_weighted_rrf() {
    let strategy = FusionStrategy::WeightedRrf {
        k: 10,
        weights: vec![1.0, 3.0],
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(strategy, deserialized);
}

#[test]
fn test_serde_roundtrip_custom() {
    let strategy = FusionStrategy::Custom {
        name: "decay_weighted".to_string(),
        params: serde_json::json!({"decay": 0.95}),
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(strategy, deserialized);
}

#[test]
fn test_serde_rejects_rrf_k_zero() {
    let json = r#"{"rrf":{"k":0}}"#;
    let result: Result<FusionStrategy, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn test_serde_rejects_weighted_rrf_k_zero_and_nonpositive_weights() {
    let k_zero: Result<FusionStrategy, _> =
        serde_json::from_str(r#"{"weighted_rrf":{"k":0,"weights":[1.0]}}"#);
    assert!(k_zero.is_err());

    let zero_weight: Result<FusionStrategy, _> =
        serde_json::from_str(r#"{"weighted_rrf":{"k":1,"weights":[0.0]}}"#);
    assert!(zero_weight.is_err());
}

#[test]
fn test_serde_rejects_nan_weights() {
    // NaN cannot be represented in JSON, so this tests via the builder.
    // JSON with null weight would fail at a different level.
    assert!(FusionStrategy::try_weighted(vec![f64::NAN]).is_err());
}

#[test]
fn test_serde_rejects_custom_empty_name() {
    let json = r#"{"custom":{"name":"","params":null}}"#;
    let result: Result<FusionStrategy, _> = serde_json::from_str(json);
    assert!(result.is_err());
}
