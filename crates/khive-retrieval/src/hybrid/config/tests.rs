use super::*;

#[test]
fn test_query_text_only() {
    let q = Query::text("hello world");
    assert_eq!(q.text, "hello world");
    assert!(q.embedding.is_none());
    assert!(!q.has_embedding());
}

#[test]
fn test_query_hybrid() {
    let embedding = vec![0.1, 0.2, 0.3];
    let q = Query::hybrid("hello", embedding.clone());
    assert_eq!(q.text, "hello");
    assert_eq!(q.embedding, Some(embedding));
    assert!(q.has_embedding());
}

#[test]
fn test_query_with_filters() {
    let q = Query::text("test").with_filters(serde_json::json!({"type": "memory"}));
    assert!(q.filters.is_some());
}

#[test]
fn test_hybrid_config_default() {
    let config = HybridConfig::default();
    assert_eq!(config.top_k, 10);
    assert_eq!(config.candidate_pool_size, 50);
    assert!(matches!(
        config.fusion_strategy,
        FusionStrategy::Rrf { k: 60 }
    ));
    assert!(config.min_score.is_none());
}

#[test]
fn test_hybrid_config_new() {
    let config = HybridConfig::new(20);
    assert_eq!(config.top_k, 20);
    assert_eq!(config.candidate_pool_size, 100); // 20 * 5
}

#[test]
fn test_hybrid_config_builder() {
    let config = HybridConfig::new(10)
        .with_fusion_strategy(FusionStrategy::union())
        .with_pool_size(200)
        .with_weights(0.6, 0.4);

    assert_eq!(config.top_k, 10);
    assert_eq!(config.candidate_pool_size, 200);
    assert!(matches!(config.fusion_strategy, FusionStrategy::Union));
    assert_eq!(config.vector_weight, 0.6);
    assert_eq!(config.keyword_weight, 0.4);
}

#[test]
fn test_normalized_weights() {
    let config = HybridConfig::default();
    let (v, k) = config.normalized_weights();
    assert!((v - 0.7).abs() < 0.01);
    assert!((k - 0.3).abs() < 0.01);

    // Zero weights -> equal
    let config = HybridConfig::default().with_weights(0.0, 0.0);
    let (v, k) = config.normalized_weights();
    assert!((v - 0.5).abs() < 0.01);
    assert!((k - 0.5).abs() < 0.01);
}

#[test]
fn test_weight_clamping() {
    let config = HybridConfig::default().with_weights(1.5, -0.5);
    assert_eq!(config.vector_weight, 1.0);
    assert_eq!(config.keyword_weight, 0.0);
}

/// Parser-level rejection only; see `crates/khive-retrieval/docs/api/hybrid-config.md`
/// for why this is not the real `TryFrom` regression guard.
#[test]
fn test_serde_json_rejects_nan_literal_vector_weight() {
    let json = r#"{
            "fusion_strategy": {"rrf": {"k": 60}},
            "top_k": 10,
            "candidate_pool_size": 50,
            "min_score": null,
            "vector_weight": NaN,
            "keyword_weight": 0.3
        }"#;
    let result: Result<HybridConfig, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "JSON literal NaN must be rejected by the parser"
    );
}

/// Same caveat as above: `Infinity` is not a valid JSON token either.
#[test]
fn test_serde_json_rejects_infinity_literal_vector_weight() {
    let json = r#"{
            "fusion_strategy": {"rrf": {"k": 60}},
            "top_k": 10,
            "candidate_pool_size": 50,
            "min_score": null,
            "vector_weight": Infinity,
            "keyword_weight": 0.3
        }"#;
    let result: Result<HybridConfig, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "JSON literal Infinity must be rejected by the parser"
    );
}

/// The real `TryFrom<RawHybridConfig>` regression guard (constructed directly since
/// `serde_json` cannot encode a literal NaN). See
/// `crates/khive-retrieval/docs/api/hybrid-config.md` for the full rationale.
#[test]
fn test_try_from_rejects_nan_vector_weight() {
    let raw = RawHybridConfig {
        fusion_strategy: FusionStrategy::rrf(),
        top_k: 10,
        candidate_pool_size: 50,
        min_score: None,
        vector_weight: f64::NAN,
        keyword_weight: 0.3,
        timeout: None,
    };
    let result = HybridConfig::try_from(raw);
    assert!(
        result.is_err(),
        "NaN vector_weight must be rejected via TryFrom"
    );
}

#[test]
fn test_try_from_rejects_infinite_vector_weight() {
    let raw = RawHybridConfig {
        fusion_strategy: FusionStrategy::rrf(),
        top_k: 10,
        candidate_pool_size: 50,
        min_score: None,
        vector_weight: f64::INFINITY,
        keyword_weight: 0.3,
        timeout: None,
    };
    let result = HybridConfig::try_from(raw);
    assert!(
        result.is_err(),
        "+Infinity vector_weight must be rejected via TryFrom"
    );
}

#[test]
fn test_try_from_rejects_nan_keyword_weight() {
    let raw = RawHybridConfig {
        fusion_strategy: FusionStrategy::rrf(),
        top_k: 10,
        candidate_pool_size: 50,
        min_score: None,
        vector_weight: 0.7,
        keyword_weight: f64::NAN,
        timeout: None,
    };
    let result = HybridConfig::try_from(raw);
    assert!(
        result.is_err(),
        "NaN keyword_weight must be rejected via TryFrom"
    );
}

#[test]
fn test_try_from_rejects_negative_infinity_keyword_weight() {
    let raw = RawHybridConfig {
        fusion_strategy: FusionStrategy::rrf(),
        top_k: 10,
        candidate_pool_size: 50,
        min_score: None,
        vector_weight: 0.7,
        keyword_weight: f64::NEG_INFINITY,
        timeout: None,
    };
    let result = HybridConfig::try_from(raw);
    assert!(
        result.is_err(),
        "-Infinity keyword_weight must be rejected via TryFrom"
    );
}

/// Positive control: a valid finite config still deserializes correctly through the
/// `TryFrom` boundary — the fix must not reject legitimate input.
#[test]
fn test_serde_accepts_valid_finite_config() {
    let json = r#"{
            "fusion_strategy": {"rrf": {"k": 60}},
            "top_k": 10,
            "candidate_pool_size": 50,
            "min_score": null,
            "vector_weight": 0.7,
            "keyword_weight": 0.3
        }"#;
    let config: HybridConfig = serde_json::from_str(json).expect("valid config");
    assert_eq!(config.top_k, 10);
    assert_eq!(config.candidate_pool_size, 50);
    assert_eq!(config.vector_weight, 0.7);
    assert_eq!(config.keyword_weight, 0.3);
}

#[test]
fn test_serde_roundtrip_preserves_default_config() {
    let config = HybridConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let restored: HybridConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.vector_weight, config.vector_weight);
    assert_eq!(restored.keyword_weight, config.keyword_weight);
    assert_eq!(restored.top_k, config.top_k);
}
