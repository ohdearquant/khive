#[test]
fn default_config_roundtrip() {
    let cfg = RecallConfig::default();
    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: RecallConfig = serde_json::from_str(&json).expect("deserialize");
    let diff = (cfg.relevance_weight - back.relevance_weight).abs();
    assert!(diff < 1e-12);
    assert_eq!(cfg.decay_model, back.decay_model);
}

#[test]
fn decay_model_exponential_roundtrip() {
    let m = DecayModel::Exponential;
    let json = serde_json::to_string(&m).expect("serialize");
    let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(m, back);
}

#[test]
fn decay_model_hyperbolic_roundtrip() {
    let m = DecayModel::Hyperbolic;
    let json = serde_json::to_string(&m).expect("serialize");
    let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(m, back);
}

#[test]
fn decay_model_powerlaw_roundtrip() {
    let m = DecayModel::PowerLaw {
        half_life_days: 14.0,
    };
    let json = serde_json::to_string(&m).expect("serialize");
    let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(m, back);
}

#[test]
fn decay_model_none_roundtrip() {
    let m = DecayModel::None;
    let json = serde_json::to_string(&m).expect("serialize");
    let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(m, back);
}

#[test]
fn partial_config_deserializes_with_defaults() {
    // Only override one field — the rest should default.
    let json = r#"{"relevance_weight": 0.5}"#;
    let cfg: RecallConfig = serde_json::from_str(json).expect("deserialize partial");
    // specified field
    let diff = (cfg.relevance_weight - 0.5).abs();
    assert!(diff < 1e-12);
    // unspecified fields keep defaults
    let diff2 = (cfg.salience_weight - 0.20).abs();
    assert!(diff2 < 1e-12);
    assert_eq!(cfg.decay_model, DecayModel::Exponential);
}

// ── RecallConfig new fields ───────────────────────────────────────────────

#[test]
fn new_fields_have_correct_defaults() {
    let cfg = RecallConfig::default();
    assert_eq!(cfg.candidate_limit, Some(150));
    // Calibrated default: RRF k=10 (2026-06 pen-default, reconfirmed 2026-07-21).
    assert!(
        matches!(cfg.fuse_strategy, FusionStrategy::Rrf { k: 10 }),
        "default fuse_strategy should be Rrf {{ k: 10 }}, got {:?}",
        cfg.fuse_strategy
    );
    assert!(!cfg.include_breakdown);
}
