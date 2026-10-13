use super::*;
use serde::Deserialize;

// ── DecayModel ────────────────────────────────────────────────────────────

#[test]
fn exponential_halves_at_decay_factor_half_life() {
    // exponential decay: salience * exp(-decay_factor * age_days)
    // Half-life = ln(2) / decay_factor ≈ 69.3 days for decay_factor=0.01
    let model = DecayModel::Exponential;
    let salience = 1.0;
    let decay_factor = 0.01;
    let half_life_days = std::f64::consts::LN_2 / decay_factor;
    let result = model.apply(salience, half_life_days, decay_factor, 30.0);
    let diff = (result - 0.5).abs();
    assert!(
        diff < 1e-10,
        "exponential should give 0.5 at ln(2)/decay_factor days, got {result}"
    );
}

#[test]
fn exponential_full_salience_at_zero_age() {
    let model = DecayModel::Exponential;
    let result = model.apply(0.8, 0.0, 0.01, 30.0);
    let diff = (result - 0.8).abs();
    assert!(
        diff < 1e-12,
        "at age=0 salience should be unchanged, got {result}"
    );
}

#[test]
fn exponential_uses_note_decay_factor_not_half_life() {
    // Verify the formula uses decay_factor param, not the half_life param.
    // At age=1 day, decay_factor=1.0 → exp(-1.0) ≈ 0.3679.
    // If we were using half_life=10 days, exp(-ln2/10) ≈ 0.933.
    let model = DecayModel::Exponential;
    let result = model.apply(1.0, 1.0, 1.0, 10.0);
    let expected = (-1.0f64).exp();
    assert!(
        (result - expected).abs() < 1e-12,
        "expected {expected}, got {result}"
    );
}

#[test]
fn hyperbolic_halves_at_one_over_decay_factor() {
    // salience / (1 + k * age) = 0.5 when age = 1/k
    let model = DecayModel::Hyperbolic;
    let salience = 1.0;
    let k = 0.05;
    let age = 1.0 / k; // 20 days
    let result = model.apply(salience, age, k, 30.0);
    let diff = (result - 0.5).abs();
    assert!(
        diff < 1e-10,
        "hyperbolic at age=1/k should give 0.5, got {result}"
    );
}

#[test]
fn hyperbolic_full_salience_at_zero_age() {
    let model = DecayModel::Hyperbolic;
    let result = model.apply(0.7, 0.0, 0.05, 30.0);
    let diff = (result - 0.7).abs();
    assert!(
        diff < 1e-12,
        "at age=0 salience should be unchanged, got {result}"
    );
}

#[test]
fn powerlaw_halves_at_half_life() {
    let hl = 30.0;
    let model = DecayModel::PowerLaw { half_life_days: hl };
    let salience = 1.0;
    // salience * hl / (hl + age) = 0.5 when age = hl
    let result = model.apply(salience, hl, 0.01, hl);
    let diff = (result - 0.5).abs();
    assert!(
        diff < 1e-10,
        "power-law should give 0.5 at half-life, got {result}"
    );
}

#[test]
fn decay_none_returns_salience_unchanged() {
    let model = DecayModel::None;
    let result = model.apply(0.6, 100.0, 0.99, 30.0);
    let diff = (result - 0.6).abs();
    assert!(
        diff < 1e-12,
        "None model must not alter salience, got {result}"
    );
}

// ── RecallConfig ──────────────────────────────────────────────────────────

#[test]
fn default_config_validates() {
    assert!(RecallConfig::default().validate().is_ok());
}

#[test]
fn negative_relevance_weight_fails_validation() {
    let cfg = RecallConfig {
        relevance_weight: -0.1,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn negative_salience_weight_fails_validation() {
    let cfg = RecallConfig {
        salience_weight: -1.0,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn negative_temporal_weight_fails_validation() {
    let cfg = RecallConfig {
        temporal_weight: -0.5,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn all_zero_weights_fails_validation() {
    let cfg = RecallConfig {
        relevance_weight: 0.0,
        salience_weight: 0.0,
        temporal_weight: 0.0,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_half_life_fails_validation() {
    let cfg = RecallConfig {
        temporal_half_life_days: 0.0,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn negative_half_life_fails_validation() {
    let cfg = RecallConfig {
        temporal_half_life_days: -5.0,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn non_uniform_weights_validate() {
    let cfg = RecallConfig {
        relevance_weight: 0.5,
        salience_weight: 0.3,
        temporal_weight: 0.2,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_ok());
}

// ── Serde roundtrips ──────────────────────────────────────────────────────

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

#[test]
fn tuning_grid_contains_the_runtime_default() {
    #[derive(Deserialize)]
    struct CandidatePool {
        candidate_multiplier: u32,
        candidate_limit: u32,
    }

    #[derive(Deserialize)]
    struct FixedDimensions {
        min_score: f64,
    }

    #[derive(Deserialize)]
    struct TuningGrid {
        weight_triples: Vec<[f64; 3]>,
        candidate_pools: Vec<CandidatePool>,
        fusion_configs: Vec<FusionStrategy>,
        decay_models: Vec<DecayModel>,
        temporal_half_life_days: Vec<f64>,
        min_salience_values: Vec<f64>,
        fixed: FixedDimensions,
    }

    let grid: TuningGrid =
        serde_json::from_str(include_str!("../testdata/recall_tuning_grid.json"))
            .expect("recall tuning grid must be valid JSON");
    let cfg = RecallConfig::default();

    assert!(grid.weight_triples.iter().any(|weights| {
        (weights[0] - cfg.relevance_weight).abs() < 1e-12
            && (weights[1] - cfg.salience_weight).abs() < 1e-12
            && (weights[2] - cfg.temporal_weight).abs() < 1e-12
    }));
    assert!(grid.candidate_pools.iter().any(|pool| {
        pool.candidate_multiplier == cfg.candidate_multiplier
            && Some(pool.candidate_limit) == cfg.candidate_limit
    }));
    assert!(grid.fusion_configs.contains(&cfg.fuse_strategy));
    assert!(grid.decay_models.contains(&cfg.decay_model));
    assert!(grid
        .temporal_half_life_days
        .contains(&cfg.temporal_half_life_days));
    assert!(grid.min_salience_values.contains(&cfg.min_salience));
    assert!((grid.fixed.min_score - cfg.min_score).abs() < 1e-12);
}

#[test]
fn candidate_limit_zero_fails_validation() {
    let cfg = RecallConfig {
        candidate_limit: Some(0),
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn candidate_limit_some_positive_validates() {
    let cfg = RecallConfig {
        candidate_limit: Some(100),
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_ok());
}

#[test]
fn min_score_nan_fails_validation() {
    let cfg = RecallConfig {
        min_score: f64::NAN,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn min_salience_nan_fails_validation() {
    let cfg = RecallConfig {
        min_salience: f64::NAN,
        ..RecallConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn new_fields_roundtrip() {
    let cfg = RecallConfig {
        candidate_limit: Some(50),
        fuse_strategy: FusionStrategy::Union,
        include_breakdown: true,
        ..RecallConfig::default()
    };
    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: RecallConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.candidate_limit, Some(50));
    assert_eq!(back.fuse_strategy, FusionStrategy::Union);
    assert!(back.include_breakdown);
}

#[test]
fn partial_config_new_fields_use_defaults() {
    // Parse JSON that omits all new fields — they should fall back to defaults.
    // With #[serde(default)] on the struct, missing fields use RecallConfig::default(),
    // so candidate_limit falls back to Some(150), not Option::default() == None.
    let json = r#"{"temporal_weight": 0.15}"#;
    let cfg: RecallConfig = serde_json::from_str(json).expect("deserialize partial");
    assert_eq!(cfg.candidate_limit, Some(150));
    assert!(
        matches!(cfg.fuse_strategy, FusionStrategy::Rrf { k: 10 }),
        "partial config must deserialize fuse_strategy to the Rrf {{ k: 10 }} default"
    );
    assert!(!cfg.include_breakdown);
}

// ── ScoreBreakdown ────────────────────────────────────────────────────────

#[test]
fn score_breakdown_total_sums_contributions() {
    let bd = ScoreBreakdown {
        relevance: 0.5,
        salience_raw: 0.8,
        salience_decayed: 0.6,
        temporal: 0.3,
        weighted: WeightedContributions {
            relevance_contribution: 0.35,
            salience_contribution: 0.12,
            temporal_contribution: 0.03,
        },
        profile_component: 1.0,
        entity_posterior_mean: None,
    };
    let expected = 0.35 + 0.12 + 0.03;
    let diff = (bd.total() - expected).abs();
    assert!(
        diff < 1e-12,
        "total() should sum weighted contributions, got {}",
        bd.total()
    );
}

// ── RecallFtsGatherConfig ─────────────────────────────────────────────────

#[test]
fn fts_gather_default_is_disabled() {
    let cfg = RecallFtsGatherConfig::default();
    assert!(!cfg.enabled);
    assert_eq!(cfg.term_k, 10);
    assert_eq!(cfg.selection_rule, RecallFtsSelectionRule::Original);
    assert_eq!(cfg.gather_mode, RecallFtsGatherMode::Ranked);
    assert!(cfg.gather_limit.is_none());
    assert_eq!(cfg.gather_cap_multiplier, 4);
    assert!(cfg.cjk_bypass_ranked);
}

#[test]
fn fts_gather_validates_zero_term_k() {
    let cfg = RecallFtsGatherConfig {
        term_k: 0,
        ..RecallFtsGatherConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn fts_gather_validates_zero_multiplier() {
    let cfg = RecallFtsGatherConfig {
        gather_cap_multiplier: 0,
        ..RecallFtsGatherConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn fts_gather_validates_zero_gather_limit() {
    let cfg = RecallFtsGatherConfig {
        gather_limit: Some(0),
        ..RecallFtsGatherConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn fts_gather_effective_gather_limit_uses_multiplier() {
    let cfg = RecallFtsGatherConfig {
        gather_cap_multiplier: 4,
        ..RecallFtsGatherConfig::default()
    };
    assert_eq!(cfg.effective_gather_limit(150).unwrap(), 600);
}

#[test]
fn fts_gather_effective_gather_limit_explicit_wins() {
    let cfg = RecallFtsGatherConfig {
        gather_limit: Some(500),
        gather_cap_multiplier: 4,
        ..RecallFtsGatherConfig::default()
    };
    assert_eq!(cfg.effective_gather_limit(150).unwrap(), 500);
}

#[test]
fn fts_gather_effective_gather_limit_too_small_fails() {
    let cfg = RecallFtsGatherConfig {
        gather_limit: Some(100),
        ..RecallFtsGatherConfig::default()
    };
    assert!(cfg.effective_gather_limit(150).is_err());
}

#[test]
fn fts_gather_from_env_returns_none_when_no_env_set() {
    // Ensure none of the relevant vars are set in CI/test environment.
    // If they are set by a prior test, this test may be flaky; the vars
    // are prefixed to avoid collisions.
    if std::env::var("KHIVE_RECALL_FTS_GATHER").is_ok()
        || std::env::var("KHIVE_RECALL_FTS_TERM_K").is_ok()
    {
        return; // skip if env is already configured
    }
    let result = RecallFtsGatherConfig::from_env().unwrap();
    assert!(result.is_none());
}

#[test]
fn fts_gather_invalid_gather_value_fails() {
    // We can't easily set env vars in unit tests without affecting other
    // tests. This exercises the validation branch directly.
    // Manually simulate what from_env would produce for an invalid value.
    let cfg = RecallFtsGatherConfig {
        term_k: 0,
        ..RecallFtsGatherConfig::default()
    };
    assert!(cfg.validate().is_err(), "term_k=0 must fail validation");
}

#[test]
fn fts_gather_config_wire_modes_preserve_search_options() {
    use khive_storage::types::TextGatherMode;
    use RecallFtsGatherMode::*;

    for (wire, mode) in [
        ("ranked", Ranked),
        ("unranked", Unranked),
        ("rank_within_cap", RankWithinCap),
    ] {
        let mut cfg: RecallFtsGatherConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "gather_mode": wire,
        }))
        .unwrap();
        cfg.validate().unwrap();
        let canonical: TextGatherMode = cfg.gather_mode;
        assert_eq!(canonical, mode);
        assert_eq!(
            cfg.to_search_options(150).unwrap(),
            TextSearchOptions {
                gather_mode: canonical,
                gather_limit: Some(600),
            }
        );
        assert_eq!(
            serde_json::to_value(&cfg).unwrap(),
            serde_json::json!({
                "enabled": true,
                "term_k": 10,
                "selection_rule": "original",
                "gather_mode": wire,
                "gather_limit": null,
                "gather_cap_multiplier": 4,
                "cjk_bypass_ranked": true,
            })
        );

        cfg.gather_limit = Some(300);
        assert_eq!(cfg.to_search_options(150).unwrap().gather_limit, Some(300));
        assert!(matches!(
            cfg.to_search_options(301),
            Err(RuntimeError::InvalidInput(message))
                if message == "fts_gather.gather_limit (300) must be >= candidate_limit (301)"
        ));
    }

    let defaults: RecallFtsGatherConfig = serde_json::from_str("{}").unwrap();
    assert!(!defaults.enabled);
    assert_eq!(
        defaults.to_search_options(150).unwrap(),
        TextSearchOptions {
            gather_mode: Ranked,
            gather_limit: Some(600),
        }
    );
    for invalid in [
        serde_json::json!("Ranked"),
        serde_json::json!("rank_subset"),
        serde_json::json!("unknown"),
        serde_json::Value::Null,
        serde_json::json!(7),
        serde_json::json!({}),
    ] {
        assert!(
            serde_json::from_value::<RecallFtsGatherConfig>(serde_json::json!({
                "gather_mode": invalid,
            }))
            .is_err()
        );
    }
}

#[test]
fn recall_config_default_has_fts_gather() {
    let cfg = RecallConfig::default();
    assert!(!cfg.fts_gather.enabled);
}

#[test]
fn recall_config_roundtrip_with_fts_gather() {
    let cfg = RecallConfig {
        fts_gather: RecallFtsGatherConfig {
            enabled: true,
            term_k: 5,
            selection_rule: RecallFtsSelectionRule::HighestIdf,
            gather_mode: RecallFtsGatherMode::RankWithinCap,
            gather_limit: Some(600),
            gather_cap_multiplier: 4,
            cjk_bypass_ranked: true,
        },
        ..RecallConfig::default()
    };
    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: RecallConfig = serde_json::from_str(&json).expect("deserialize");
    assert!(back.fts_gather.enabled);
    assert_eq!(back.fts_gather.term_k, 5);
    assert_eq!(
        back.fts_gather.gather_mode,
        RecallFtsGatherMode::RankWithinCap
    );
    assert_eq!(back.fts_gather.gather_limit, Some(600));
}

#[test]
fn english_only_embedding_models_name_all_minilm_and_no_multilingual_model() {
    assert!(is_english_only_embedding_model("all-minilm-l6-v2"));
    // Registered names compare ASCII case-insensitively.
    assert!(is_english_only_embedding_model("All-MiniLM-L6-v2"));
    for model in [
        "paraphrase-multilingual-minilm-l12-v2",
        "multilingual-e5-small",
        "multilingual-e5-base",
        "my-custom-encoder",
        "all-minilm-l6-v2-extra",
        "",
    ] {
        assert!(!is_english_only_embedding_model(model), "{model:?}");
    }
}
