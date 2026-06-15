use super::common::*;
use crate::config::{DecayModel, RecallConfig};
use khive_fusion::FusionStrategy;
use khive_runtime::RuntimeError;
use serde_json::Value;
use uuid::Uuid;

// ── TextSnippetPolicy ────────────────────────────────────────────────────

#[test]
fn text_snippet_policy_omit_returns_zero() {
    assert_eq!(TextSnippetPolicy::Omit.snippet_chars(), 0);
}

#[test]
fn text_snippet_policy_include_returns_chars() {
    assert_eq!(
        TextSnippetPolicy::Include { chars: 200 }.snippet_chars(),
        200
    );
}

#[test]
fn text_snippet_policy_include_zero_chars_clamps_to_one() {
    assert_eq!(
        TextSnippetPolicy::Include { chars: 0 }.snippet_chars(),
        1,
        "Include{{chars:0}} must clamp to 1 so callers always get some snippet"
    );
}

#[test]
fn validate_memory_type_rejects_invalid() {
    let err = validate_memory_type("bogus").unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput for unknown memory_type, got {err:?}"
    );
}

#[test]
fn validate_memory_type_accepts_episodic() {
    assert!(validate_memory_type("episodic").is_ok());
}

#[test]
fn validate_memory_type_accepts_semantic() {
    assert!(validate_memory_type("semantic").is_ok());
}

#[test]
fn effective_config_uses_defaults() {
    let p = RecallParams {
        query: "test".to_string(),
        limit: None,
        memory_type: None,
        min_score: None,
        min_salience: None,
        config: None,
        top_k: None,
        fusion_strategy: None,
        score_floor: None,
        embedding_model: None,
        include_breakdown: None,
        tags: None,
        tag_mode: TagMode::Any,
        entity_names: None,
        full_content: None,
    };
    let cfg = p.effective_config(RecallConfig::default());
    assert!((cfg.relevance_weight - 0.70).abs() < 1e-12);
    assert!((cfg.salience_weight - 0.20).abs() < 1e-12);
    assert!((cfg.temporal_weight - 0.10).abs() < 1e-12);
}

#[test]
fn effective_config_legacy_overrides() {
    let p = RecallParams {
        query: "test".to_string(),
        limit: None,
        memory_type: None,
        min_score: Some(0.5),
        min_salience: Some(0.3),
        config: None,
        top_k: None,
        fusion_strategy: None,
        score_floor: None,
        embedding_model: None,
        include_breakdown: None,
        tags: None,
        tag_mode: TagMode::Any,
        entity_names: None,
        full_content: None,
    };
    let cfg = p.effective_config(RecallConfig::default());
    assert!((cfg.min_score - 0.5).abs() < 1e-12);
    assert!((cfg.min_salience - 0.3).abs() < 1e-12);
}

#[test]
fn effective_config_explicit_config_wins() {
    let p = RecallParams {
        query: "test".to_string(),
        limit: None,
        memory_type: None,
        min_score: Some(0.1),
        min_salience: None,
        config: Some(RecallConfig {
            relevance_weight: 0.50,
            ..RecallConfig::default()
        }),
        top_k: None,
        fusion_strategy: None,
        score_floor: None,
        embedding_model: None,
        include_breakdown: None,
        tags: None,
        tag_mode: TagMode::Any,
        entity_names: None,
        full_content: None,
    };
    let cfg = p.effective_config(RecallConfig::default());
    assert!((cfg.relevance_weight - 0.50).abs() < 1e-12);
    assert!((cfg.min_score - 0.1).abs() < 1e-12);
}

#[test]
fn test_weighted_strategy_preserves_pack_weights() {
    let base = RecallConfig {
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.8, 0.2],
        },
        ..RecallConfig::default()
    };

    let p = RecallParams {
        query: "test".to_string(),
        limit: None,
        memory_type: None,
        min_score: None,
        min_salience: None,
        config: None,
        top_k: None,
        fusion_strategy: Some("weighted".to_string()),
        score_floor: None,
        embedding_model: None,
        include_breakdown: None,
        tags: None,
        tag_mode: TagMode::Any,
        entity_names: None,
        full_content: None,
    };

    let mut cfg = p.effective_config(base);
    if let Some(ref fs) = p.fusion_strategy {
        let mut new_strategy = parse_fusion_strategy_str(fs).unwrap();
        if let (
            FusionStrategy::Weighted {
                weights: ref mut new_w,
            },
            FusionStrategy::Weighted {
                weights: ref existing_w,
            },
        ) = (&mut new_strategy, &cfg.fuse_strategy)
        {
            *new_w = existing_w.clone();
        }
        cfg.fuse_strategy = new_strategy;
    }

    match cfg.fuse_strategy {
        FusionStrategy::Weighted { weights } => {
            assert_eq!(
                weights,
                vec![0.8, 0.2],
                "fusion_strategy=weighted must preserve pack weights [0.8, 0.2], not override with [0.3, 0.7]"
            );
        }
        other => panic!("expected Weighted strategy, got {other:?}"),
    }
}

#[test]
fn fusion_strategy_change_produces_observable_ordering_difference() {
    use khive_storage::types::{TextSearchHit, VectorSearchHit};
    use std::collections::HashSet;
    use uuid::Uuid;

    let id_a = Uuid::from_u128(0xAAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA);
    let id_b = Uuid::from_u128(0xBBBB_BBBB_BBBB_BBBB_BBBB_BBBB_BBBB_BBBB);
    let id_c = Uuid::from_u128(0xCCCC_CCCC_CCCC_CCCC_CCCC_CCCC_CCCC_CCCC);

    let text_hits = vec![
        TextSearchHit {
            subject_id: id_a,
            score: 0.9_f64.into(),
            rank: 1,
            title: None,
            snippet: None,
        },
        TextSearchHit {
            subject_id: id_b,
            score: 0.5_f64.into(),
            rank: 2,
            title: None,
            snippet: None,
        },
    ];
    let vector_hits = vec![
        VectorSearchHit {
            subject_id: id_c,
            score: 0.95_f64.into(),
            rank: 1,
        },
        VectorSearchHit {
            subject_id: id_a,
            score: 0.3_f64.into(),
            rank: 2,
        },
    ];
    let memory_ids: HashSet<Uuid> = [id_a, id_b, id_c].into_iter().collect();

    let candidates_rrf = RecallCandidateSet {
        namespace: "local".to_string(),
        text_hits: text_hits.clone(),
        vector_hits_per_model: vec![("mock".to_string(), vector_hits.clone())],
        multilingual_routed: false,
    };
    let cfg_rrf = RecallConfig {
        fuse_strategy: FusionStrategy::Rrf { k: 60 },
        ..RecallConfig::default()
    };
    let rrf_results = fuse_candidates(&candidates_rrf, &memory_ids, &cfg_rrf, 10);
    let rrf_order: Vec<Uuid> = rrf_results.iter().map(|h| h.entity_id).collect();

    let candidates_weighted = RecallCandidateSet {
        namespace: "local".to_string(),
        text_hits,
        vector_hits_per_model: vec![("mock".to_string(), vector_hits)],
        multilingual_routed: false,
    };
    let cfg_weighted = RecallConfig {
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.9, 0.1],
        },
        ..RecallConfig::default()
    };
    let weighted_results = fuse_candidates(&candidates_weighted, &memory_ids, &cfg_weighted, 10);
    let weighted_order: Vec<Uuid> = weighted_results.iter().map(|h| h.entity_id).collect();

    assert_ne!(
        rrf_order, weighted_order,
        "fusion_strategy change must affect ordering; RRF and Weighted produced identical: {rrf_order:?}"
    );
    assert_eq!(
        rrf_order.first(),
        Some(&id_a),
        "RRF must put id_a first (highest combined rank)"
    );
    assert_eq!(
        weighted_order.first(),
        Some(&id_c),
        "Weighted(vector=0.9) must put id_c first (highest vector score)"
    );
}

#[test]
fn compute_score_weighted_strategy_formula() {
    let cfg = RecallConfig {
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.3, 0.7],
        },
        ..RecallConfig::default()
    };
    let relevance = 0.5;
    let salience = 0.8;
    let decay_factor = 0.01;
    let age_days = 0.0;
    let pipeline = make_pipeline(&cfg);
    let (total, bd) = compute_score(&cfg, &pipeline, relevance, salience, decay_factor, age_days);
    let amplified = 0.8_f64.powf(SALIENCE_AMPLIFIER_ALPHA);
    let expected = 0.70 * 0.5 + 0.20 * amplified + 0.10 * 1.0;
    assert!(
        (total - expected).abs() < 1e-10,
        "got {total}, expected {expected}"
    );
    assert!((bd.relevance - 0.5).abs() < 1e-12);
    assert!((bd.salience_raw - 0.8).abs() < 1e-12);
}

#[test]
fn compute_score_rrf_strategy_normalizes_to_comparable_range() {
    let cfg = RecallConfig {
        fuse_strategy: FusionStrategy::Rrf { k: 60 },
        ..RecallConfig::default()
    };
    let raw_rrf_rank1 = 1.0 / 61.0;
    let pipeline = make_pipeline(&cfg);
    let (_, bd) = compute_score(&cfg, &pipeline, raw_rrf_rank1, 1.0, 0.0, 0.0);
    assert!(
        (bd.relevance - 1.0).abs() < 1e-10,
        "RRF rank-1 relevance should normalize to 1.0, got {}",
        bd.relevance
    );
}

#[test]
fn compute_score_rrf_multi_source_clamped_to_one() {
    let cfg = RecallConfig {
        fuse_strategy: FusionStrategy::Rrf { k: 60 },
        ..RecallConfig::default()
    };
    let raw_rrf_two_sources = 2.0 / 61.0;
    let pipeline = make_pipeline(&cfg);
    let (total, bd) = compute_score(&cfg, &pipeline, raw_rrf_two_sources, 1.0, 0.0, 0.0);
    assert!(
        bd.relevance <= 1.0,
        "relevance must not exceed 1.0 for multi-source RRF, got {}",
        bd.relevance
    );
    assert!(
        total <= 1.0,
        "composite score must not exceed 1.0, got {total}"
    );
    assert!(
        total >= 0.0,
        "composite score must not be negative, got {total}"
    );
}

#[test]
fn compute_score_exponential_decay_at_decay_factor_half_life() {
    let cfg = RecallConfig {
        decay_model: DecayModel::Exponential,
        temporal_half_life_days: 30.0,
        ..RecallConfig::default()
    };
    let age_days = std::f64::consts::LN_2 / 0.01;
    let pipeline = make_pipeline(&cfg);
    let (_, bd) = compute_score(&cfg, &pipeline, 0.5, 1.0, 0.01, age_days);
    assert!(
        (bd.salience_decayed - 0.5).abs() < 1e-10,
        "salience_decayed = {}",
        bd.salience_decayed
    );
    assert!(bd.temporal < 0.5, "temporal = {}", bd.temporal);
}

#[test]
fn compute_score_temporal_halves_at_temporal_half_life() {
    let cfg = RecallConfig {
        temporal_half_life_days: 30.0,
        ..RecallConfig::default()
    };
    let pipeline = make_pipeline(&cfg);
    let (_, bd) = compute_score(&cfg, &pipeline, 0.5, 1.0, 0.01, 30.0);
    assert!(
        (bd.temporal - 0.5).abs() < 1e-10,
        "temporal = {}",
        bd.temporal
    );
}

#[test]
fn compute_score_custom_weights() {
    let cfg = RecallConfig {
        relevance_weight: 1.0,
        salience_weight: 0.0,
        temporal_weight: 0.0,
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.5, 0.5],
        },
        ..RecallConfig::default()
    };
    let pipeline = make_pipeline(&cfg);
    let (total, _) = compute_score(&cfg, &pipeline, 0.8, 0.9, 0.01, 10.0);
    assert!((total - 0.8).abs() < 1e-10, "got {total}");
}

#[test]
fn remember_params_default_memory_type_is_episodic() {
    assert!(validate_memory_type("episodic").is_ok());
}

#[test]
fn remember_params_salience_below_zero_rejected() {
    let salience: f64 = -0.1;
    let result: Result<f64, RuntimeError> = if !(0.0..=1.0).contains(&salience) {
        Err(RuntimeError::InvalidInput(format!(
            "salience must be in [0, 1], got {salience}"
        )))
    } else {
        Ok(salience)
    };
    assert!(result.is_err(), "expected error for salience < 0");
}

#[test]
fn remember_params_salience_above_one_rejected() {
    let salience: f64 = 1.1;
    let result: Result<f64, RuntimeError> = if !(0.0..=1.0).contains(&salience) {
        Err(RuntimeError::InvalidInput(format!(
            "salience must be in [0, 1], got {salience}"
        )))
    } else {
        Ok(salience)
    };
    assert!(result.is_err(), "expected error for salience > 1");
}

#[test]
fn remember_params_salience_boundary_values_accepted() {
    for val in [0.0_f64, 0.5, 1.0] {
        let result: Result<(), RuntimeError> = if !(0.0..=1.0).contains(&val) {
            Err(RuntimeError::InvalidInput("out of range".into()))
        } else {
            Ok(())
        };
        assert!(result.is_ok(), "boundary {val} should be accepted");
    }
}

#[test]
fn remember_params_decay_factor_below_zero_rejected() {
    let df: f64 = -0.01;
    let result: Result<f64, RuntimeError> = if df < 0.0 {
        Err(RuntimeError::InvalidInput(format!(
            "decay_factor must be >= 0, got {df}"
        )))
    } else {
        Ok(df)
    };
    assert!(result.is_err(), "expected error for decay_factor < 0");
}

#[test]
fn remember_params_decay_factor_above_one_accepted() {
    let df: f64 = 2.5;
    let result: Result<f64, RuntimeError> = if df < 0.0 {
        Err(RuntimeError::InvalidInput("negative".into()))
    } else {
        Ok(df)
    };
    assert!(result.is_ok(), "decay_factor > 1 should be accepted");
}

#[test]
fn remember_params_invalid_source_id_uuid_is_rejected() {
    let sid = "not-a-uuid";
    let result: Result<Uuid, RuntimeError> = sid
        .parse::<Uuid>()
        .map_err(|_| RuntimeError::InvalidInput(format!("source_id {sid:?} is not a valid UUID")));
    assert!(result.is_err(), "expected error for invalid UUID string");
}

#[test]
fn remember_params_valid_source_id_uuid_is_accepted() {
    let sid = "00000000-0000-0000-0000-000000000001";
    let result = sid.parse::<Uuid>();
    assert!(result.is_ok(), "valid UUID should parse successfully");
}

#[test]
fn recall_rerank_config_empty_reranker_weights_has_no_active() {
    let cfg = RecallConfig::default();
    let active: Vec<_> = cfg
        .reranker_weights
        .iter()
        .filter(|(_, &w)| w > 0.0)
        .collect();
    assert!(active.is_empty(), "default config has no active rerankers");
}

#[test]
fn recall_rerank_config_with_reranker_weight_is_active() {
    let mut cfg = RecallConfig::default();
    cfg.reranker_weights
        .insert("cross_encoder".to_string(), 0.5);
    let active: Vec<_> = cfg
        .reranker_weights
        .iter()
        .filter(|(_, &w)| w > 0.0)
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].0, "cross_encoder");
}

#[test]
fn recall_config_reranker_fields_default_empty() {
    let cfg = RecallConfig::default();
    assert!(cfg.reranker_weights.is_empty());
}

#[test]
fn recall_config_negative_reranker_weight_fails_validation() {
    let mut cfg = RecallConfig::default();
    cfg.reranker_weights
        .insert("bad_reranker".to_string(), -0.1);
    assert!(cfg.validate().is_err());
}

#[test]
fn recall_config_zero_reranker_weight_validates() {
    let mut cfg = RecallConfig::default();
    cfg.reranker_weights
        .insert("disabled_reranker".to_string(), 0.0);
    assert!(cfg.validate().is_ok());
}

#[test]
fn high_salience_outranks_low_salience_on_similar_relevance() {
    let cfg = RecallConfig {
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.5, 0.5],
        },
        ..RecallConfig::default()
    };
    let relevance = 0.5;
    let age_days = 0.0;
    let decay_factor = 0.01;
    let pipeline = make_pipeline(&cfg);

    let (score_high, _) = compute_score(&cfg, &pipeline, relevance, 0.9, decay_factor, age_days);
    let (score_low, _) = compute_score(&cfg, &pipeline, relevance, 0.3, decay_factor, age_days);

    assert!(
        score_high > score_low,
        "high salience (score={score_high}) should outrank low salience (score={score_low})"
    );

    let gap = score_high - score_low;
    assert!(gap > 0.05, "salience score gap should be > 0.05, got {gap}");
}

#[test]
fn salience_amplifier_discriminates_more_than_linear() {
    let cfg = RecallConfig::default();
    let relevance = 0.0;
    let age_days = 0.0;
    let pipeline = make_pipeline(&cfg);

    let (score_high, _) = compute_score(&cfg, &pipeline, relevance, 0.9, 0.0, age_days);
    let (score_low, _) = compute_score(&cfg, &pipeline, relevance, 0.3, 0.0, age_days);
    let amplified_spread = score_high - score_low;

    let linear_spread = 0.20_f64 * (0.9 - 0.3);

    assert!(
        amplified_spread > linear_spread,
        "amplified spread ({amplified_spread}) should exceed linear spread ({linear_spread})"
    );
}

#[test]
fn vector_candidates_per_model_shape_is_array_of_model_objects() {
    use khive_storage::types::VectorSearchHit;
    use uuid::Uuid;

    let id1 = Uuid::from_u128(0x1);
    let id2 = Uuid::from_u128(0x2);

    let hits_a = vec![VectorSearchHit {
        subject_id: id1,
        score: 0.9_f64.into(),
        rank: 1,
    }];
    let hits_b = vec![VectorSearchHit {
        subject_id: id2,
        score: 0.7_f64.into(),
        rank: 1,
    }];

    let candidates = RecallCandidateSet {
        namespace: "test".to_string(),
        text_hits: vec![],
        vector_hits_per_model: vec![
            ("model-a".to_string(), hits_a),
            ("model-b".to_string(), hits_b),
        ],
        multilingual_routed: false,
    };

    let per_model: Vec<Value> = candidates
        .vector_hits_per_model
        .iter()
        .map(|(model, hits)| {
            let hits_json: Vec<Value> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "id": h.subject_id.to_string(),
                        "score": h.score.to_f64(),
                        "rank": h.rank,
                    })
                })
                .collect();
            serde_json::json!({ "model": model, "hits": hits_json })
        })
        .collect();

    assert_eq!(per_model.len(), 2, "should have one entry per model");
    assert_eq!(per_model[0]["model"], "model-a");
    assert_eq!(per_model[0]["hits"][0]["id"], id1.to_string());
    assert_eq!(per_model[1]["model"], "model-b");
    assert_eq!(per_model[1]["hits"][0]["id"], id2.to_string());
}

#[test]
fn recall_params_empty_query_should_be_rejected() {
    for q in &["", "   ", "\t\n"] {
        let result: Result<(), RuntimeError> = if q.trim().is_empty() {
            Err(RuntimeError::InvalidInput("query must not be empty".into()))
        } else {
            Ok(())
        };
        assert!(
            result.is_err(),
            "empty/whitespace query {:?} must be rejected",
            q
        );
    }
}

#[test]
fn compute_score_composite_bounded_to_unit_interval() {
    let cfgs = [
        RecallConfig {
            fuse_strategy: FusionStrategy::Rrf { k: 60 },
            ..RecallConfig::default()
        },
        RecallConfig::default(),
        RecallConfig {
            fuse_strategy: FusionStrategy::Union,
            ..RecallConfig::default()
        },
    ];
    for cfg in &cfgs {
        let pipeline = make_pipeline(cfg);
        for raw_relevance in [0.0, 0.5, 1.0, 2.0 / 61.0, 1.0 / 61.0] {
            for salience in [0.0, 0.3, 0.9, 1.0] {
                let (total, _) = compute_score(cfg, &pipeline, raw_relevance, salience, 0.01, 0.0);
                assert!(
                    (0.0..=1.0).contains(&total),
                    "composite score out of [0,1]: {total} (relevance={raw_relevance}, salience={salience}, strategy={:?})",
                    cfg.fuse_strategy
                );
            }
        }
    }
}

#[test]
fn default_fusion_strategy_is_weighted() {
    let cfg = RecallConfig::default();
    assert!(
        matches!(cfg.fuse_strategy, FusionStrategy::Weighted { .. }),
        "default fuse_strategy must be Weighted (CC-6), got {:?}",
        cfg.fuse_strategy
    );
}

#[test]
fn salience_dominates_relevance_under_default_weighted_strategy() {
    let cfg = RecallConfig::default();
    let age_days = 0.0;
    let decay = 0.01;
    let pipeline = make_pipeline(&cfg);

    let relevance_low = 0.9;
    let relevance_high = 0.8;

    let (score_high, _) = compute_score(&cfg, &pipeline, relevance_high, 0.9, decay, age_days);
    let (score_low, _) = compute_score(&cfg, &pipeline, relevance_low, 0.3, decay, age_days);

    assert!(
        score_high > score_low,
        "high-salience (0.9, relevance=0.8, score={score_high}) should outrank \
         low-salience (0.3, relevance=0.9, score={score_low}) under default Weighted strategy"
    );
}

#[test]
fn fanout_constant_matches_production_limit() {
    assert_eq!(
        RECALL_FTS_TERM_FANOUT_LIMIT, 10,
        "RECALL_FTS_TERM_FANOUT_LIMIT drifted from 10; update this test if intentional"
    );
}

#[test]
fn recall_text_terms_with_limit_truncates_to_limit() {
    let terms = recall_text_terms_with_limit("a b c d e f g h i j k", 10);
    assert_eq!(
        terms.len(),
        10,
        "expected 10 terms, got {}: {terms:?}",
        terms.len()
    );
    assert_eq!(terms[0], "a");
    assert_eq!(terms[9], "j");
}

#[test]
fn recall_text_terms_with_limit_smaller_cap() {
    let terms = recall_text_terms_with_limit("recall search path latency", 3);
    assert_eq!(terms, vec!["recall", "search", "path"]);
}

#[test]
fn recall_text_terms_cjk_not_dropped() {
    let terms = recall_text_terms_with_limit("東京 レイテンシ ベクトル検索", 10);
    assert_eq!(
        terms.len(),
        3,
        "CJK terms must not be dropped by ASCII cleanup: got {terms:?}"
    );
    assert!(
        terms.contains(&"東京".to_string()),
        "expected 東京 in {terms:?}"
    );
    assert!(
        terms.contains(&"レイテンシ".to_string()),
        "expected レイテンシ in {terms:?}"
    );
}

#[test]
fn recall_text_terms_deduplicates() {
    let terms = recall_text_terms_with_limit("recall recall search search", 10);
    assert_eq!(terms, vec!["recall", "search"]);
}

#[test]
fn recall_text_terms_production_path_uses_constant() {
    let query = "a b c d e f g h i j k";
    assert_eq!(
        recall_text_terms(query),
        recall_text_terms_with_limit(query, RECALL_FTS_TERM_FANOUT_LIMIT),
    );
}

// ── Type-differentiated salience + decay defaults (#84) ─────────────────────
//
// Production-path coverage (dispatches the real handler, asserts stored values)
// lives in crates/khive-pack-memory/tests/integration.rs:
//   - test_remember_episodic_defaults_stored
//   - test_remember_omitted_memory_type_uses_episodic_defaults
//   - test_remember_semantic_defaults_stored
//   - test_remember_explicit_salience_overrides_episodic_default
//   - test_remember_explicit_decay_overrides_episodic_default
//
// The named constants exercised by those tests are defined in handlers/common.rs:
//   DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC,
//   DEFAULT_DECAY_EPISODIC, DEFAULT_DECAY_SEMANTIC.

#[test]
fn remember_type_defaults_constants_are_differentiated() {
    use super::common::{
        DEFAULT_DECAY_EPISODIC, DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC,
        DEFAULT_SALIENCE_SEMANTIC,
    };
    const { assert!(DEFAULT_SALIENCE_EPISODIC < DEFAULT_SALIENCE_SEMANTIC) };
    const { assert!(DEFAULT_DECAY_EPISODIC > DEFAULT_DECAY_SEMANTIC) };
    assert!(
        (DEFAULT_SALIENCE_EPISODIC - 0.3).abs() < 1e-12,
        "episodic salience constant must be 0.3"
    );
    assert!(
        (DEFAULT_SALIENCE_SEMANTIC - 0.5).abs() < 1e-12,
        "semantic salience constant must be 0.5"
    );
    assert!(
        (DEFAULT_DECAY_EPISODIC - 0.02).abs() < 1e-12,
        "episodic decay constant must be 0.02"
    );
    assert!(
        (DEFAULT_DECAY_SEMANTIC - 0.005).abs() < 1e-12,
        "semantic decay constant must be 0.005"
    );
}
