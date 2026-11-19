use super::*;

#[test]
fn test_config_defaults() {
    let config = HnswConfig::default();
    assert_eq!(config.m, 20);
    assert_eq!(config.ef_construction, 200);
    assert_eq!(config.ef_search, 80);
    assert_eq!(config.dimensions, 384);
}

#[test]
fn test_config_variants() {
    let high = HnswConfig::high_recall();
    assert_eq!(high.m, 32);
    assert_eq!(high.ef_construction, 400);

    let fast = HnswConfig::fast_build();
    assert_eq!(fast.m, 12);

    let low = HnswConfig::low_memory();
    assert_eq!(low.m, 8);
}

#[test]
fn test_with_dimensions() {
    let config = HnswConfig::with_dimensions(1536);
    assert_eq!(config.dimensions, 1536);
    assert_eq!(config.m, 20); // Other defaults preserved
}

#[test]
fn test_try_with_dimensions_rejects_zero() {
    let result = HnswConfig::try_with_dimensions(0);
    assert!(result.is_err());
}

#[test]
fn test_validate_rejects_m_zero() {
    let config = HnswConfig {
        m: 0,
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_validate_rejects_m_max0_less_than_m() {
    let config = HnswConfig {
        m: 20,
        m_max0: 10,
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_validate_rejects_ef_construction_zero() {
    let config = HnswConfig {
        ef_construction: 0,
        ..Default::default()
    };
    assert!(
        config.validate().is_err(),
        "ef_construction = 0 must be rejected"
    );
}

#[test]
fn test_serde_rejects_ef_construction_zero() {
    let json = r#"{"m":20,"m_max0":40,"ef_construction":0,"ml":0.2886751345948129,"ef_search":80,"dimensions":384,"metric":"cosine","rebuild_threshold":0.1}"#;
    let result: std::result::Result<HnswConfig, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "ef_construction = 0 must be rejected at deserialization"
    );
}

#[test]
fn test_validate_rejects_ef_search_zero() {
    let config = HnswConfig {
        ef_search: 0,
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_validate_rejects_non_finite_ml() {
    let config = HnswConfig {
        ml: f64::NAN,
        ..Default::default()
    };
    assert!(config.validate().is_err());
    let config2 = HnswConfig {
        ml: f64::INFINITY,
        ..Default::default()
    };
    assert!(config2.validate().is_err());
    let config3 = HnswConfig {
        ml: -1.0,
        ..Default::default()
    };
    assert!(config3.validate().is_err());
}

#[test]
fn test_validate_rejects_invalid_rebuild_threshold() {
    let config = HnswConfig {
        rebuild_threshold: f64::NAN,
        ..Default::default()
    };
    assert!(config.validate().is_err());
    let config2 = HnswConfig {
        rebuild_threshold: -0.1,
        ..Default::default()
    };
    assert!(config2.validate().is_err());
    let config3 = HnswConfig {
        rebuild_threshold: 1.1,
        ..Default::default()
    };
    assert!(config3.validate().is_err());
}

#[test]
fn test_validate_accepts_boundary_rebuild_threshold() {
    let config0 = HnswConfig {
        rebuild_threshold: 0.0,
        ..Default::default()
    };
    assert!(config0.validate().is_ok());
    let config1 = HnswConfig {
        rebuild_threshold: 1.0,
        ..Default::default()
    };
    assert!(config1.validate().is_ok());
}

#[test]
#[should_panic(expected = "HNSW dimensions must be > 0")]
fn test_with_dimensions_rejects_zero() {
    HnswConfig::with_dimensions(0);
}

#[test]
#[should_panic(expected = "HNSW configuration must be valid")]
fn test_index_with_config_rejects_zero_dimensions() {
    crate::HnswIndex::with_config(HnswConfig {
        dimensions: 0,
        ..Default::default()
    });
}

#[test]
fn test_distance_metric_default() {
    assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
}
