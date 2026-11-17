use super::*;

// -- Construction -------------------------------------------------------

#[test]
fn test_vector_construction() {
    let emb = vec![0.1, 0.2, 0.3];
    let node = QueryNode::vector(emb.clone(), 10);
    match &node {
        QueryNode::Vector {
            embedding,
            top_k,
            min_score,
        } => {
            assert_eq!(embedding, &emb);
            assert_eq!(*top_k, 10);
            assert!(min_score.is_none());
        }
        other => panic!("expected Vector, got {:?}", other),
    }
}

#[test]
fn test_keyword_construction() {
    let node = QueryNode::keyword("hello world", 5);
    match &node {
        QueryNode::Keyword {
            text,
            top_k,
            min_score,
        } => {
            assert_eq!(text, "hello world");
            assert_eq!(*top_k, 5);
            assert!(min_score.is_none());
        }
        other => panic!("expected Keyword, got {:?}", other),
    }
}

#[test]
fn test_hybrid_construction() {
    let emb = vec![0.1_f32; 128];
    let node = QueryNode::hybrid(emb, "distributed consensus", 10);
    match &node {
        QueryNode::Fuse {
            children,
            strategy,
            top_k,
        } => {
            assert_eq!(children.len(), 2);
            assert_eq!(*top_k, 10);
            // Sub-queries should request 4x candidates.
            assert_eq!(children[0].top_k(), 40);
            assert_eq!(children[1].top_k(), 40);
            assert!(matches!(strategy, FuseStrategy::Rrf { k: 60 }));
        }
        other => panic!("expected Fuse, got {:?}", other),
    }
}

// -- is_empty -----------------------------------------------------------

#[test]
fn test_empty_variant() {
    assert!(QueryNode::Empty.is_empty());
    assert_eq!(QueryNode::Empty.leaf_count(), 0);
    assert_eq!(QueryNode::Empty.top_k(), 0);
    assert_eq!(QueryNode::Empty.depth(), 0);
}

#[test]
fn test_vector_top_k_zero_is_empty() {
    let node = QueryNode::vector(vec![1.0], 0);
    assert!(node.is_empty());
}

#[test]
fn test_keyword_top_k_zero_is_empty() {
    let node = QueryNode::keyword("hello", 0);
    assert!(node.is_empty());
}

#[test]
fn test_keyword_empty_text_is_empty() {
    let node = QueryNode::keyword("", 10);
    assert!(node.is_empty());
}

#[test]
fn test_fuse_no_children_is_empty() {
    let node = QueryNode::Fuse {
        children: vec![],
        strategy: FuseStrategy::Rrf { k: 60 },
        top_k: 10,
    };
    assert!(node.is_empty());
}

#[test]
fn test_filter_of_empty_is_empty() {
    let node = QueryNode::Empty.with_min_score(DeterministicScore::from_f64(0.5));
    assert!(node.is_empty());
}

#[test]
fn test_rerank_of_empty_is_empty() {
    let node = QueryNode::Rerank {
        child: Box::new(QueryNode::Empty),
        method: RerankMethod::ScoreWeighted { weights: vec![1.0] },
        top_k: 10,
    };
    assert!(node.is_empty());
}

#[test]
fn test_non_empty_query() {
    let node = QueryNode::keyword("hello", 5);
    assert!(!node.is_empty());
}

// -- leaf_count ---------------------------------------------------------

#[test]
fn test_leaf_count_single() {
    assert_eq!(QueryNode::vector(vec![1.0], 5).leaf_count(), 1);
    assert_eq!(QueryNode::keyword("q", 5).leaf_count(), 1);
}

#[test]
fn test_leaf_count_hybrid() {
    let q = QueryNode::hybrid(vec![1.0], "q", 10);
    assert_eq!(q.leaf_count(), 2);
}

#[test]
fn test_leaf_count_nested() {
    // Fuse(Fuse(vec, kw), kw) = 3 leaves
    let inner = QueryNode::hybrid(vec![1.0], "inner", 10);
    let outer = QueryNode::Fuse {
        children: vec![inner, QueryNode::keyword("outer", 10)],
        strategy: FuseStrategy::Union,
        top_k: 10,
    };
    assert_eq!(outer.leaf_count(), 3);
}

// -- top_k --------------------------------------------------------------

#[test]
fn test_top_k_leaf() {
    assert_eq!(QueryNode::vector(vec![], 7).top_k(), 7);
    assert_eq!(QueryNode::keyword("q", 3).top_k(), 3);
}

#[test]
fn test_top_k_fuse() {
    let q = QueryNode::hybrid(vec![1.0], "q", 15);
    assert_eq!(q.top_k(), 15);
}

#[test]
fn test_top_k_filter_topk_predicate() {
    let node = QueryNode::keyword("q", 100).with_top_k(5);
    assert_eq!(node.top_k(), 5);
}

#[test]
fn test_top_k_filter_non_topk_predicate() {
    let node = QueryNode::keyword("q", 20).with_min_score(DeterministicScore::from_f64(0.5));
    // min_score filter doesn't change top_k -- falls through to child.
    assert_eq!(node.top_k(), 20);
}

// -- depth --------------------------------------------------------------

#[test]
fn test_depth_leaf() {
    assert_eq!(QueryNode::vector(vec![1.0], 5).depth(), 1);
    assert_eq!(QueryNode::keyword("q", 5).depth(), 1);
}

#[test]
fn test_depth_hybrid() {
    let q = QueryNode::hybrid(vec![1.0], "q", 10);
    // Fuse -> leaf = depth 2
    assert_eq!(q.depth(), 2);
}

#[test]
fn test_depth_chained_filters() {
    let q = QueryNode::keyword("q", 10)
        .with_min_score(DeterministicScore::from_f64(0.5))
        .with_top_k(5);
    // TopK(Filter(MinScore(Keyword))) = 3 wrappers + 1 leaf = depth 3
    assert_eq!(q.depth(), 3);
}

// -- with_min_score / with_top_k chaining -------------------------------

#[test]
fn test_builder_chaining() {
    let node = QueryNode::keyword("rust async patterns", 20)
        .with_min_score(DeterministicScore::from_f64(0.3))
        .with_top_k(10);

    assert_eq!(node.top_k(), 10);
    assert_eq!(node.leaf_count(), 1);
    assert!(!node.is_empty());
}

// -- Serde round-trip ---------------------------------------------------

#[test]
fn test_serde_roundtrip_vector() {
    let node = QueryNode::vector(vec![0.1, 0.2, 0.3], 10);
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.top_k(), 10);
    assert_eq!(back.leaf_count(), 1);
}

#[test]
fn test_serde_roundtrip_keyword() {
    let node = QueryNode::keyword("hello world", 5);
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.top_k(), 5);
}

#[test]
fn test_serde_roundtrip_hybrid() {
    let node = QueryNode::hybrid(vec![1.0, 2.0], "search query", 10);
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.top_k(), 10);
    assert_eq!(back.leaf_count(), 2);
}

#[test]
fn test_serde_roundtrip_complex() {
    let node = QueryNode::hybrid(vec![0.5; 4], "complex query", 10)
        .with_min_score(DeterministicScore::from_f64(0.2))
        .with_top_k(5);

    let json = serde_json::to_string_pretty(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.top_k(), 5);
    assert_eq!(back.leaf_count(), 2);
    assert!(!back.is_empty());
}

#[test]
fn test_serde_roundtrip_empty() {
    let node = QueryNode::Empty;
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert!(back.is_empty());
}

#[test]
fn test_serde_roundtrip_filter_metadata() {
    let node = QueryNode::Filter {
        child: Box::new(QueryNode::keyword("docs", 10)),
        predicate: FilterPredicate::MetadataEquals {
            field: "type".to_string(),
            value: serde_json::json!("memory"),
        },
    };
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.leaf_count(), 1);
}

#[test]
fn test_serde_roundtrip_rerank() {
    let node = QueryNode::Rerank {
        child: Box::new(QueryNode::keyword("rerank me", 20)),
        method: RerankMethod::CrossEncoder {
            model: "ms-marco-MiniLM".to_string(),
        },
        top_k: 10,
    };
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.top_k(), 10);
}

#[test]
fn test_serde_roundtrip_compound_predicate() {
    let pred = FilterPredicate::And(vec![
        FilterPredicate::MinScore(DeterministicScore::from_f64(0.3)),
        FilterPredicate::Or(vec![
            FilterPredicate::MetadataEquals {
                field: "lang".to_string(),
                value: serde_json::json!("en"),
            },
            FilterPredicate::MetadataEquals {
                field: "lang".to_string(),
                value: serde_json::json!("zh"),
            },
        ]),
    ]);
    let node = QueryNode::Filter {
        child: Box::new(QueryNode::keyword("test", 10)),
        predicate: pred,
    };
    let json = serde_json::to_string(&node).expect("serialize");
    let back: QueryNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.leaf_count(), 1);
}

#[test]
fn fusion_strategy_conversion_preserves_all_variants_and_custom_payload() {
    let strategies = [
        FusionStrategy::Rrf { k: 17 },
        FusionStrategy::Weighted {
            weights: vec![2.0, 0.5],
        },
        FusionStrategy::WeightedRrf {
            k: 31,
            weights: vec![0.25, 3.0],
        },
        FusionStrategy::Union,
        FusionStrategy::VectorOnly,
        FusionStrategy::KeywordOnly,
        FusionStrategy::Custom {
            name: "nested-custom".into(),
            params: serde_json::json!({"ordered": [3, null, true, "raw"], "nested": {"weight": 0.25}}),
        },
    ];
    for (index, expected) in strategies.into_iter().enumerate() {
        let ir = FuseStrategy::from(expected.clone());
        match (index, &ir) {
            (0, FuseStrategy::Rrf { k }) => assert_eq!(*k, 17),
            (1, FuseStrategy::Weighted { weights }) => assert_eq!(weights, &[2.0, 0.5]),
            (2, FuseStrategy::WeightedRrf { k, weights }) => {
                assert_eq!(*k, 31);
                assert_eq!(
                    weights,
                    &[0.25, 3.0],
                    "ordered weighted RRF parameters changed"
                );
            }
            (3, FuseStrategy::Union)
            | (4, FuseStrategy::VectorOnly)
            | (5, FuseStrategy::KeywordOnly) => {}
            (6, FuseStrategy::Custom { name, params }) => {
                assert_eq!(name, "nested-custom");
                assert_eq!(
                    params,
                    &serde_json::json!({"ordered": [3, null, true, "raw"], "nested": {"weight": 0.25}}),
                    "opaque custom parameters changed"
                );
            }
            _ => panic!("fusion strategy changed variant at index {index}: {ir:?}"),
        }
        assert_eq!(FusionStrategy::try_from(ir), Ok(expected));
    }
}

#[test]
fn fusion_strategy_serde_preserves_legacy_and_new_wire_forms() {
    let cases = [
        (
            FuseStrategy::Rrf { k: 17 },
            serde_json::json!({"rrf": {"k": 17}}),
        ),
        (
            FuseStrategy::Weighted {
                weights: vec![2.0, 0.5],
            },
            serde_json::json!({"weighted": {"weights": [2.0, 0.5]}}),
        ),
        (FuseStrategy::Union, serde_json::json!("union")),
        (
            FuseStrategy::WeightedRrf {
                k: 31,
                weights: vec![0.25, 3.0],
            },
            serde_json::json!({"weighted_rrf": {"k": 31, "weights": [0.25, 3.0]}}),
        ),
        (FuseStrategy::VectorOnly, serde_json::json!("vector_only")),
        (FuseStrategy::KeywordOnly, serde_json::json!("keyword_only")),
        (
            FuseStrategy::Custom {
                name: "nested-custom".into(),
                params: serde_json::json!({"ordered": [3, null, true, "raw"], "nested": {"weight": 0.25}}),
            },
            serde_json::json!({"custom": {"name": "nested-custom", "params": {"ordered": [3, null, true, "raw"], "nested": {"weight": 0.25}}}}),
        ),
    ];
    for (strategy, expected_wire) in cases {
        assert_eq!(
            serde_json::to_value(&strategy).expect("serialize strategy"),
            expected_wire
        );
        let decoded: FuseStrategy =
            serde_json::from_value(expected_wire).expect("deserialize strategy");
        assert_eq!(
            FusionStrategy::try_from(decoded),
            FusionStrategy::try_from(strategy)
        );
    }
}

#[test]
fn ir_fusion_conversion_rejects_invalid_parameters_without_clamping() {
    let invalid = [
        (FuseStrategy::Rrf { k: 0 }, FusionStrategyError::RrfKZero),
        (
            FuseStrategy::Weighted {
                weights: vec![f64::NAN],
            },
            FusionStrategyError::WeightNaN,
        ),
        (
            FuseStrategy::Weighted {
                weights: vec![f64::INFINITY],
            },
            FusionStrategyError::WeightInfinite,
        ),
        (
            FuseStrategy::Weighted {
                weights: vec![f64::NEG_INFINITY],
            },
            FusionStrategyError::WeightInfinite,
        ),
        (
            FuseStrategy::WeightedRrf {
                k: 0,
                weights: vec![1.0],
            },
            FusionStrategyError::RrfKZero,
        ),
        (
            FuseStrategy::WeightedRrf {
                k: 7,
                weights: vec![f64::NAN],
            },
            FusionStrategyError::WeightNaN,
        ),
        (
            FuseStrategy::WeightedRrf {
                k: 7,
                weights: vec![f64::INFINITY],
            },
            FusionStrategyError::WeightInfinite,
        ),
        (
            FuseStrategy::WeightedRrf {
                k: 7,
                weights: vec![f64::NEG_INFINITY],
            },
            FusionStrategyError::WeightInfinite,
        ),
        (
            FuseStrategy::WeightedRrf {
                k: 7,
                weights: vec![0.0],
            },
            FusionStrategyError::WeightNotPositive,
        ),
        (
            FuseStrategy::WeightedRrf {
                k: 7,
                weights: vec![-0.5],
            },
            FusionStrategyError::WeightNotPositive,
        ),
        (
            FuseStrategy::Custom {
                name: String::new(),
                params: serde_json::json!({"keep": true}),
            },
            FusionStrategyError::CustomNameEmpty,
        ),
    ];
    for (ir, expected_error) in invalid {
        let actual = FusionStrategy::try_from(ir);
        assert_eq!(
            actual,
            Err(expected_error),
            "IR conversion must retain typed parameter refusal"
        );
    }
    assert_eq!(
        FusionStrategy::try_from(FuseStrategy::Weighted {
            weights: vec![-1.0, 0.0, 2.0]
        }),
        Ok(FusionStrategy::Weighted {
            weights: vec![-1.0, 0.0, 2.0]
        })
    );
    assert_eq!(
        FusionStrategy::try_from(FuseStrategy::Weighted { weights: vec![] }),
        Ok(FusionStrategy::Weighted { weights: vec![] })
    );
    assert_eq!(
        FusionStrategy::try_from(FuseStrategy::WeightedRrf {
            k: 7,
            weights: vec![]
        }),
        Ok(FusionStrategy::WeightedRrf {
            k: 7,
            weights: vec![]
        })
    );
    assert_eq!(
        FusionStrategy::try_from(FuseStrategy::Custom {
            name: " ".into(),
            params: serde_json::json!([null, false])
        }),
        Ok(FusionStrategy::Custom {
            name: " ".into(),
            params: serde_json::json!([null, false])
        })
    );
}

#[test]
fn hybrid_candidate_budget_is_literal_four_x_and_saturates() {
    for (requested, expected_candidates) in [
        (10, 40),
        (0, 0),
        (usize::MAX / 4 + 1, usize::MAX),
        (usize::MAX, usize::MAX),
    ] {
        let node = QueryNode::hybrid(vec![0.25], "literal query", requested);
        match node {
            QueryNode::Fuse {
                children,
                strategy,
                top_k,
            } => {
                assert_eq!(top_k, requested, "fusion result limit changed");
                assert!(matches!(strategy, FuseStrategy::Rrf { k: 60 }));
                assert_eq!(children.len(), 2);
                assert_eq!(
                    children[0].top_k(),
                    expected_candidates,
                    "vector candidate budget must be literal four-times or saturated"
                );
                assert_eq!(
                    children[1].top_k(),
                    expected_candidates,
                    "keyword candidate budget must be literal four-times or saturated"
                );
                assert!(
                    matches!(&children[0], QueryNode::Vector { embedding, min_score: None, .. } if embedding == &[0.25])
                );
                assert!(
                    matches!(&children[1], QueryNode::Keyword { text, min_score: None, .. } if text == "literal query")
                );
            }
            other => panic!("expected hybrid Fuse, got {other:?}"),
        }
    }
    assert_eq!(crate::HYBRID_CANDIDATE_MULTIPLIER, 4);
}

#[test]
fn raw_collect_analysis_preserves_structure_and_saturates_without_ranking() {
    let empty = QueryNode::Collect { children: vec![] };
    assert!(empty.is_empty());
    assert_eq!(empty.depth(), 1);
    assert_eq!(empty.leaf_count(), 0);
    assert_eq!(empty.top_k(), 0);
    let plan = QueryNode::Collect {
        children: vec![
            QueryNode::keyword("a", usize::MAX),
            QueryNode::Empty,
            QueryNode::vector(vec![0.25], 1),
        ],
    };
    assert!(!plan.is_empty());
    assert_eq!(plan.depth(), 2);
    assert_eq!(plan.leaf_count(), 2);
    assert_eq!(plan.top_k(), usize::MAX);
    let wire = serde_json::to_value(&plan).unwrap();
    assert_eq!(wire["collect"]["children"][1], serde_json::json!("empty"));
    let back: QueryNode = serde_json::from_value(wire).unwrap();
    assert_eq!(back.top_k(), usize::MAX);
    assert_eq!(back.leaf_count(), 2);
}
