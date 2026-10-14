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
            // Sub-queries should request 3x candidates.
            assert_eq!(children[0].top_k(), 30);
            assert_eq!(children[1].top_k(), 30);
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
