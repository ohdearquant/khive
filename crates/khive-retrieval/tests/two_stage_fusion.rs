//! Actual two-stage helper regressions; no search service or runtime registry is needed.

use std::future::{ready, Ready};

use khive_retrieval::{fuse_two_stage, FusionStrategy as Strategy, Result, RetrievalError};
use khive_score::{rrf_score, DeterministicScore as Score};
use serde_json::{json, Value};

type Rows = Vec<(&'static str, Score)>;

fn rows(ids: &[&'static str]) -> Rows {
    ids.iter()
        .enumerate()
        .map(|(i, id)| (*id, Score::from_f64(1.0 / (i + 1) as f64)))
        .collect()
}

fn no_custom(_: String, _: Value, _: [Rows; 2], _: usize) -> Option<Ready<Result<Rows>>> {
    None
}

async fn builtin(
    arms: Vec<Rows>,
    engine: Option<&Strategy>,
    text: Rows,
    hybrid: &Strategy,
) -> Rows {
    fuse_two_stage(arms, engine, text, hybrid, no_custom)
        .await
        .unwrap()
}

fn score(result: &Rows, id: &str) -> Score {
    result.iter().find(|(held, _)| *held == id).unwrap().1
}

#[tokio::test]
async fn text_share_stays_point_three_with_two_and_five_agreeing_engines() {
    let engine = Strategy::Rrf { k: 10 };
    let hybrid = Strategy::Weighted {
        weights: vec![0.7, 0.3],
    };
    for count in [2, 5] {
        let result = builtin(
            vec![rows(&["x"]); count],
            Some(&engine),
            rows(&["y"]),
            &hybrid,
        )
        .await;
        assert_eq!(result.len(), 2);
        assert_eq!(score(&result, "x"), Score::from_f64(0.7));
        assert_eq!(score(&result, "y"), Score::from_f64(0.3));
        assert!(score(&result, "y") > Score::ZERO);
    }
}

#[tokio::test]
async fn named_weight_order_reverses_winner_and_each_stage_has_its_own_k() {
    let arms = vec![rows(&["a", "b"]), rows(&["b", "a"])];
    let weighted = Strategy::WeightedRrf {
        k: 10,
        weights: vec![1.0, 3.0],
    };
    let vector = builtin(arms.clone(), Some(&weighted), vec![], &Strategy::VectorOnly).await;
    assert_eq!(vector[0].0, "b");
    assert_eq!(
        score(&vector, "a").to_raw(),
        rrf_score(1, 10).to_raw() + rrf_score(2, 10).to_raw() * 3
    );
    assert_eq!(
        score(&vector, "b").to_raw(),
        rrf_score(2, 10).to_raw() + rrf_score(1, 10).to_raw() * 3
    );
    let swapped = Strategy::WeightedRrf {
        k: 10,
        weights: vec![3.0, 1.0],
    };
    assert_eq!(
        builtin(arms.clone(), Some(&swapped), vec![], &Strategy::VectorOnly).await[0].0,
        "a"
    );
    for hybrid_k in [2, 100] {
        let fused = builtin(
            arms.clone(),
            Some(&weighted),
            rows(&["a"]),
            &Strategy::Rrf { k: hybrid_k },
        )
        .await;
        assert_eq!(score(&fused, "b"), rrf_score(1, hybrid_k));
        assert_eq!(
            score(&fused, "a").to_raw(),
            rrf_score(2, hybrid_k).to_raw() + rrf_score(1, hybrid_k).to_raw()
        );
    }
    let unit = Strategy::WeightedRrf {
        k: 10,
        weights: vec![1.0, 1.0],
    };
    let ordinary = builtin(
        arms.clone(),
        Some(&Strategy::Rrf { k: 10 }),
        vec![],
        &Strategy::VectorOnly,
    )
    .await;
    assert_eq!(
        ordinary,
        builtin(arms, Some(&unit), vec![], &Strategy::VectorOnly).await
    );
    assert_eq!(
        ordinary.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[tokio::test]
async fn no_intermediate_or_final_cap_loses_a_later_hybrid_winner() {
    let result = builtin(
        vec![rows(&["a", "b", "c"])],
        Some(&Strategy::Rrf { k: 10 }),
        rows(&["b", "d"]),
        &Strategy::Rrf { k: 10 },
    )
    .await;
    assert_eq!(result[0].0, "b"); // Would lose a vector contribution with a hidden top-1 stage.
    assert_eq!(
        score(&result, "b").to_raw(),
        rrf_score(2, 10).to_raw() + rrf_score(1, 10).to_raw()
    );
    assert_eq!(result.len(), 4);
    assert!(result.iter().any(|(id, _)| *id == "c"));
    assert!(result.iter().any(|(id, _)| *id == "d"));
}

#[tokio::test]
async fn empty_engine_and_modality_slots_do_not_rebind_weights() {
    let engine = Strategy::WeightedRrf {
        k: 10,
        weights: vec![1.0, 999.0, 3.0],
    };
    let vector = builtin(
        vec![rows(&["a"]), vec![], rows(&["b"])],
        Some(&engine),
        vec![],
        &Strategy::VectorOnly,
    )
    .await;
    assert_eq!(score(&vector, "a"), rrf_score(1, 10));
    assert_eq!(score(&vector, "b").to_raw(), rrf_score(1, 10).to_raw() * 3);
    let hybrid = Strategy::Weighted {
        weights: vec![0.7, 0.3],
    };
    assert_eq!(
        builtin(
            vec![vec![], vec![], vec![]],
            Some(&engine),
            rows(&["text"]),
            &hybrid
        )
        .await,
        vec![("text", Score::from_f64(0.3))]
    );
    assert_eq!(
        builtin(
            vec![rows(&["vector"]), vec![], vec![]],
            Some(&engine),
            vec![],
            &hybrid
        )
        .await,
        vec![("vector", Score::from_f64(0.7))]
    );
    let rank_hybrid = Strategy::WeightedRrf {
        k: 3,
        weights: vec![2.0, 5.0],
    };
    assert_eq!(
        score(
            &builtin(
                vec![vec![], vec![], vec![]],
                Some(&engine),
                rows(&["text"]),
                &rank_hybrid
            )
            .await,
            "text"
        )
        .to_raw(),
        rrf_score(1, 3).to_raw() * 5
    );
}

#[tokio::test]
async fn duplicate_ranks_count_once_and_keyword_only_keeps_first_occurrence() {
    let engine = Strategy::Rrf { k: 10 };
    let vector = builtin(
        vec![rows(&["a", "a", "b"])],
        Some(&engine),
        vec![],
        &Strategy::VectorOnly,
    )
    .await;
    assert_eq!(score(&vector, "a"), rrf_score(1, 10));
    assert_eq!(score(&vector, "b"), rrf_score(3, 10));
    let text = rows(&["b", "a", "b"]);
    let only = builtin(
        vec![rows(&["ignored"])],
        None,
        text.clone(),
        &Strategy::KeywordOnly,
    )
    .await;
    assert_eq!(only, text[..2]);
}

#[tokio::test]
async fn linear_fusion_preserves_min_max_and_max_per_id_without_weight_overflow() {
    let engine = Strategy::Rrf { k: 10 };
    let weighted = Strategy::Weighted {
        weights: vec![f64::MAX, f64::MAX / 2.0],
    };
    let result = builtin(vec![rows(&["x"])], Some(&engine), rows(&["y"]), &weighted).await;
    assert_eq!(score(&result, "x"), Score::from_f64(2.0 / 3.0));
    assert_eq!(score(&result, "y"), Score::from_f64(1.0 / 3.0));
    let extreme = Strategy::Weighted {
        weights: vec![f64::MIN_POSITIVE, f64::MAX],
    };
    let result = builtin(
        vec![rows(&["tiny"])],
        Some(&engine),
        rows(&["large"]),
        &extreme,
    )
    .await;
    assert_eq!(
        result,
        vec![("large", Score::from_f64(1.0)), ("tiny", Score::ZERO)]
    );
    let text = vec![
        ("low", Score::from_f64(-10.0)),
        ("high", Score::from_f64(30.0)),
        ("high", Score::from_f64(10.0)),
        ("mid", Score::from_f64(10.0)),
    ];
    let result = builtin(
        vec![],
        Some(&engine),
        text,
        &Strategy::Weighted {
            weights: vec![1.0, 1.0],
        },
    )
    .await;
    assert_eq!(
        result,
        vec![
            ("high", Score::from_f64(0.5)),
            ("mid", Score::from_f64(0.25)),
            ("low", Score::ZERO)
        ]
    );
}

#[tokio::test]
async fn custom_lookup_runs_even_empty_and_preserves_executor_errors() {
    let custom = Strategy::Custom {
        name: "missing".into(),
        params: json!({"opaque": [2, 3]}),
    };
    let engine = Strategy::Rrf { k: 10 };
    for arms in [vec![], vec![rows(&["x"])]] {
        let err = fuse_two_stage(arms, Some(&engine), vec![], &custom, no_custom)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RetrievalError::Fusion(ref message) if message.contains("unknown custom strategy 'missing'"))
        );
    }
    let mut calls = 0;
    let result = fuse_two_stage(
        vec![rows(&["x", "shared"])],
        Some(&engine),
        rows(&["shared", "y"]),
        &custom,
        |name, params, sources, limit| {
            calls += 1;
            assert_eq!(name, "missing");
            assert_eq!(params, json!({"opaque": [2, 3]}));
            assert_eq!(
                sources[0].iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                ["x", "shared"]
            );
            assert_eq!(sources[1], rows(&["shared", "y"]));
            assert_eq!(limit, 3);
            Some(ready(Err(RetrievalError::QueryCancelled)))
        },
    )
    .await;
    assert!(matches!(result, Err(RetrievalError::QueryCancelled)));
    assert_eq!(calls, 1);
    let result = fuse_two_stage(
        vec![],
        Some(&engine),
        vec![],
        &custom,
        |_, _, sources, limit| {
            assert!(sources[0].is_empty() && sources[1].is_empty());
            assert_eq!(limit, 0);
            Some(ready(Ok(rows(&["executor-result"]))))
        },
    )
    .await
    .unwrap();
    assert_eq!(result, rows(&["executor-result"]));
}

#[tokio::test]
async fn invalid_configuration_fails_before_empty_results_or_custom_dispatch() {
    let custom = Strategy::Custom {
        name: "known".into(),
        params: Value::Null,
    };
    let invalid_engines = [
        Strategy::Weighted {
            weights: vec![1.0; 3],
        },
        Strategy::Union,
        Strategy::VectorOnly,
        Strategy::KeywordOnly,
        custom.clone(),
        Strategy::Rrf { k: 0 },
        Strategy::WeightedRrf {
            k: 0,
            weights: vec![1.0; 3],
        },
        Strategy::WeightedRrf {
            k: 1,
            weights: vec![1.0; 2],
        },
    ];
    for engine in invalid_engines {
        assert!(fuse_two_stage(
            vec![vec![]; 3],
            Some(&engine),
            vec![],
            &custom,
            |_, _, _: [Rows; 2], _| -> Option<Ready<Result<Rows>>> {
                panic!("invalid engine reached dispatch")
            }
        )
        .await
        .is_err());
    }
    let valid_engine = Strategy::Rrf { k: 1 };
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let engine = Strategy::WeightedRrf {
            k: 1,
            weights: vec![bad],
        };
        assert!(
            fuse_two_stage(vec![vec![]], Some(&engine), vec![], &custom, no_custom)
                .await
                .is_err()
        );
        for hybrid in [
            Strategy::Weighted {
                weights: vec![1.0, bad],
            },
            Strategy::WeightedRrf {
                k: 1,
                weights: vec![bad, 1.0],
            },
        ] {
            assert!(
                fuse_two_stage(vec![], Some(&valid_engine), vec![], &hybrid, no_custom)
                    .await
                    .is_err()
            );
        }
    }
    for hybrid in [
        Strategy::Rrf { k: 0 },
        Strategy::WeightedRrf {
            k: 0,
            weights: vec![1.0; 2],
        },
        Strategy::Weighted { weights: vec![1.0] },
        Strategy::WeightedRrf {
            k: 1,
            weights: vec![1.0; 3],
        },
        Strategy::Custom {
            name: String::new(),
            params: Value::Null,
        },
    ] {
        assert!(
            fuse_two_stage(vec![], Some(&valid_engine), vec![], &hybrid, no_custom)
                .await
                .is_err()
        );
    }
    for hybrid in [
        Strategy::Union,
        Strategy::VectorOnly,
        Strategy::Rrf { k: 1 },
        custom,
    ] {
        assert!(fuse_two_stage(vec![], None, vec![], &hybrid, no_custom)
            .await
            .is_err());
    }
    assert!(fuse_two_stage(
        vec![],
        Some(&valid_engine),
        vec![],
        &Strategy::KeywordOnly,
        no_custom
    )
    .await
    .is_err());
    assert!(builtin(vec![], None, vec![], &Strategy::KeywordOnly)
        .await
        .is_empty());
}

#[tokio::test]
async fn score_overflow_and_nonfinite_linear_scores_are_errors() {
    let oversized_engine = Strategy::WeightedRrf {
        k: 1,
        weights: vec![f64::MAX],
    };
    assert!(fuse_two_stage(
        vec![rows(&["a"])],
        Some(&oversized_engine),
        vec![],
        &Strategy::VectorOnly,
        no_custom
    )
    .await
    .is_err());
    let engine = Strategy::Rrf { k: 1 };
    let oversized_hybrid = Strategy::WeightedRrf {
        k: 1,
        weights: vec![f64::MAX, 1.0],
    };
    assert!(fuse_two_stage(
        vec![rows(&["a"])],
        Some(&engine),
        vec![],
        &oversized_hybrid,
        no_custom
    )
    .await
    .is_err());
    for bad in [Score::MIN, Score::MAX, Score::NEG_INF] {
        let err = fuse_two_stage(
            vec![],
            Some(&engine),
            vec![("bad", bad)],
            &Strategy::Weighted {
                weights: vec![1.0, 1.0],
            },
            no_custom,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RetrievalError::Fusion(ref message) if message.contains("linear input scores must be finite"))
        );
    }
}

#[tokio::test]
async fn rank_denominator_overflow_is_refused_but_empty_arms_are_valid() {
    for engine in [
        Strategy::Rrf { k: usize::MAX },
        Strategy::WeightedRrf {
            k: usize::MAX,
            weights: vec![1.0],
        },
    ] {
        let err = fuse_two_stage(
            vec![rows(&["a"])],
            Some(&engine),
            vec![],
            &Strategy::VectorOnly,
            no_custom,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RetrievalError::Fusion(ref message) if message.contains("engine fusion: RRF k plus source rank"))
        );
        assert!(
            builtin(vec![vec![]], Some(&engine), vec![], &Strategy::VectorOnly)
                .await
                .is_empty()
        );
    }
    let engine = Strategy::Rrf { k: 1 };
    for hybrid in [
        Strategy::Rrf { k: usize::MAX },
        Strategy::WeightedRrf {
            k: usize::MAX,
            weights: vec![1.0, 1.0],
        },
    ] {
        let err = fuse_two_stage(vec![], Some(&engine), rows(&["a"]), &hybrid, no_custom)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RetrievalError::Fusion(ref message) if message.contains("hybrid fusion: RRF k plus source rank"))
        );
        assert!(builtin(vec![], Some(&engine), vec![], &hybrid)
            .await
            .is_empty());
    }
}

#[tokio::test]
async fn union_keeps_max_scores_without_linear_normalization() {
    let result = builtin(
        vec![rows(&["shared", "vector"])],
        Some(&Strategy::Rrf { k: 1 }),
        vec![
            ("shared", Score::from_f64(0.25)),
            ("text", Score::from_f64(9.0)),
        ],
        &Strategy::Union,
    )
    .await;
    assert_eq!(
        result,
        vec![
            ("text", Score::from_f64(9.0)),
            ("shared", rrf_score(1, 1)),
            ("vector", rrf_score(2, 1)),
        ]
    );
}
