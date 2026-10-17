use khive_retrieval::{
    ArmEvidence, ArmOutcome, EngineEvidence, FusionStrategy, RetrievalEnvelope, RetrievalEvidence,
    RetrievalScoreKind, RetrievalStrategy, StrategySource, RETRIEVAL_STRATEGY_VERSION,
};
use serde_json::{json, Value};

fn arm(requested: usize, returned: usize, outcome: ArmOutcome) -> ArmEvidence {
    ArmEvidence {
        requested_candidates: requested,
        returned_candidates: returned,
        outcome,
    }
}

fn evidence() -> RetrievalEvidence {
    RetrievalEvidence {
        strategy: RetrievalStrategy {
            engine_fusion: Some(FusionStrategy::WeightedRrf {
                k: 60,
                weights: vec![1.0, 0.8],
            }),
            hybrid_fusion: FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
        },
        strategy_version: RETRIEVAL_STRATEGY_VERSION,
        strategy_source: StrategySource::Request,
        requested_engines: Some(vec!["zeta".into(), "alpha".into()]),
        selected_engines: vec!["zeta".into(), "alpha".into()],
        used_engines: vec!["zeta".into(), "alpha".into()],
        engines: vec![
            EngineEvidence {
                name: "zeta".into(),
                effective_weight: 1.0,
                arm: arm(20, 3, ArmOutcome::Ran),
            },
            EngineEvidence {
                name: "alpha".into(),
                effective_weight: 0.8,
                arm: arm(20, 0, ArmOutcome::Ran),
            },
        ],
        text: arm(30, 2, ArmOutcome::Ran),
        degraded: false,
        engine_score_kind: Some(RetrievalScoreKind::WeightedRrf),
        hybrid_score_kind: RetrievalScoreKind::Weighted,
    }
}

fn expected_evidence() -> Value {
    json!({
        "strategy": {
            "engine_fusion": {"weighted_rrf": {"k": 60, "weights": [1.0, 0.8]}},
            "hybrid_fusion": {"weighted": {"weights": [0.7, 0.3]}}
        },
        "strategy_version": 1,
        "strategy_source": "request",
        "requested_engines": ["zeta", "alpha"],
        "selected_engines": ["zeta", "alpha"],
        "used_engines": ["zeta", "alpha"],
        "engines": [
            {"name": "zeta", "effective_weight": 1.0, "arm": {
                "requested_candidates": 20, "returned_candidates": 3,
                "outcome": {"status": "ran"}
            }},
            {"name": "alpha", "effective_weight": 0.8, "arm": {
                "requested_candidates": 20, "returned_candidates": 0,
                "outcome": {"status": "ran"}
            }}
        ],
        "text": {"requested_candidates": 30, "returned_candidates": 2,
                 "outcome": {"status": "ran"}},
        "degraded": false,
        "engine_score_kind": "weighted_rrf",
        "hybrid_score_kind": "weighted"
    })
}

#[test]
fn populated_wire_snapshot_preserves_hits_order_and_resolved_weights() {
    let results = vec![
        json!({"id": "second", "score": 0.5}),
        json!({"id": "first"}),
    ];
    let actual = serde_json::to_value(RetrievalEnvelope {
        results: results.clone(),
        retrieval: evidence(),
    })
    .unwrap();
    assert_eq!(
        actual,
        json!({"results": results, "retrieval": expected_evidence()})
    );
}

#[test]
fn empty_results_retain_config_source_and_zero_hit_participation() {
    let mut observed = evidence();
    observed.strategy_source = StrategySource::Config;
    observed.requested_engines = None;
    observed.engines[0].arm.returned_candidates = 0;
    observed.text.returned_candidates = 0;
    let mut expected = expected_evidence();
    expected["strategy_source"] = json!("config");
    expected["requested_engines"] = Value::Null;
    expected["engines"][0]["arm"]["returned_candidates"] = json!(0);
    expected["text"]["returned_candidates"] = json!(0);
    assert_eq!(
        serde_json::to_value(RetrievalEnvelope::<Value> {
            results: vec![],
            retrieval: observed,
        })
        .unwrap(),
        json!({"results": [], "retrieval": expected})
    );
}

#[test]
fn degraded_snapshot_distinguishes_failed_engine_from_successful_zero_hits() {
    let mut observed = evidence();
    observed.engines[0].arm = arm(
        20,
        0,
        ArmOutcome::Failed {
            reason: "provider timeout".into(),
        },
    );
    observed.used_engines = vec!["alpha".into()];
    observed.degraded = true;
    let mut expected = expected_evidence();
    expected["engines"][0]["arm"] = json!({
        "requested_candidates": 20, "returned_candidates": 0,
        "outcome": {"status": "failed", "reason": "provider timeout"}
    });
    expected["used_engines"] = json!(["alpha"]);
    expected["degraded"] = json!(true);
    assert_eq!(
        serde_json::to_value(RetrievalEnvelope::<Value> {
            results: vec![],
            retrieval: observed,
        })
        .unwrap(),
        json!({"results": [], "retrieval": expected})
    );
}

#[test]
fn keyword_only_snapshot_has_no_engine_stage_and_preserves_empty_selection() {
    let observed = RetrievalEvidence {
        strategy: RetrievalStrategy {
            engine_fusion: None,
            hybrid_fusion: FusionStrategy::KeywordOnly,
        },
        strategy_version: RETRIEVAL_STRATEGY_VERSION,
        strategy_source: StrategySource::Request,
        requested_engines: Some(vec![]),
        selected_engines: vec![],
        used_engines: vec![],
        engines: vec![],
        text: arm(10, 0, ArmOutcome::Ran),
        degraded: false,
        engine_score_kind: None,
        hybrid_score_kind: RetrievalScoreKind::Keyword,
    };
    assert_eq!(
        serde_json::to_value(RetrievalEnvelope::<String> {
            results: vec![],
            retrieval: observed,
        })
        .unwrap(),
        json!({"results": [], "retrieval": {
            "strategy": {"engine_fusion": null, "hybrid_fusion": "keyword_only"},
            "strategy_version": 1, "strategy_source": "request",
            "requested_engines": [], "selected_engines": [], "used_engines": [],
            "engines": [], "text": {"requested_candidates": 10, "returned_candidates": 0,
                                      "outcome": {"status": "ran"}},
            "degraded": false, "engine_score_kind": null, "hybrid_score_kind": "keyword"
        }})
    );
}

#[test]
fn skipped_unavailable_and_failed_reasons_are_distinct_wire_dispositions() {
    let cases = [
        (ArmOutcome::Ran, json!({"status": "ran"})),
        (
            ArmOutcome::Skipped {
                reason: "vector only".into(),
            },
            json!({"status": "skipped", "reason": "vector only"}),
        ),
        (
            ArmOutcome::Unavailable {
                reason: "index cold".into(),
            },
            json!({"status": "unavailable", "reason": "index cold"}),
        ),
        (
            ArmOutcome::Failed {
                reason: "read refused".into(),
            },
            json!({"status": "failed", "reason": "read refused"}),
        ),
    ];
    for (outcome, expected) in cases {
        assert_eq!(
            serde_json::to_value(arm(0, 0, outcome)).unwrap(),
            json!({
                "requested_candidates": 0, "returned_candidates": 0, "outcome": expected
            })
        );
    }
}

#[test]
fn custom_stage_evidence_retains_executor_parameters_and_declared_score_kind() {
    let mut observed = evidence();
    observed.strategy.engine_fusion = Some(FusionStrategy::Rrf { k: 8 });
    for engine in &mut observed.engines {
        engine.effective_weight = 1.0;
    }
    observed.strategy.hybrid_fusion = FusionStrategy::Custom {
        name: "host_fusion".into(),
        params: json!({"tie_break": "stable"}),
    };
    observed.engine_score_kind = Some(RetrievalScoreKind::Rrf);
    observed.hybrid_score_kind = RetrievalScoreKind::Custom {
        name: "host_rank_v2".into(),
    };
    observed.text = arm(
        0,
        0,
        ArmOutcome::Skipped {
            reason: "producer policy".into(),
        },
    );
    let actual = serde_json::to_value(observed).unwrap();
    assert_eq!(
        actual["strategy"]["engine_fusion"],
        json!({"rrf": {"k": 8}})
    );
    assert_eq!(
        actual["strategy"]["hybrid_fusion"],
        json!({"custom": {
            "name": "host_fusion", "params": {"tie_break": "stable"}
        }})
    );
    assert_eq!(
        actual["hybrid_score_kind"],
        json!({"custom": {"name": "host_rank_v2"}})
    );
    assert_eq!(actual["engine_score_kind"], json!("rrf"));
    assert!(actual["engines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["effective_weight"] == json!(1.0)));
    assert!(actual.get("query_vector").is_none());
}
