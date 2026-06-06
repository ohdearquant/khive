//! Query Intermediate Representation for the retrieval pipeline.
//!
//! Composable IR tree representing how sub-queries are structured, filtered, and fused,
//! separate from the Query struct which captures what to search.

use khive_score::DeterministicScore;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core IR node
// ---------------------------------------------------------------------------

/// A node in the Query IR tree.
///
/// Each variant represents a single retrieval operation or combinator.
/// Nodes compose recursively -- a `Fuse` holds children, a `Filter` wraps
/// a single child, and leaf nodes (`Vector`, `Keyword`, `Empty`) terminate
/// the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryNode {
    /// Vector similarity search (e.g. HNSW nearest-neighbor).
    Vector {
        /// Pre-computed query embedding.
        embedding: Vec<f32>,
        /// Number of results to return.
        top_k: usize,
        /// Optional minimum similarity threshold.
        min_score: Option<DeterministicScore>,
    },

    /// Keyword / BM25 text search.
    Keyword {
        /// Query text.
        text: String,
        /// Number of results to return.
        top_k: usize,
        /// Optional minimum relevance threshold.
        min_score: Option<DeterministicScore>,
    },

    /// Fuse multiple sub-queries into a single ranked list.
    Fuse {
        /// Sub-queries to fuse.
        children: Vec<QueryNode>,
        /// Strategy for combining ranked lists.
        strategy: FuseStrategy,
        /// Number of results after fusion.
        top_k: usize,
    },

    /// Filter the results of a sub-query.
    Filter {
        /// The sub-query whose results are filtered.
        child: Box<QueryNode>,
        /// Predicate to apply.
        predicate: FilterPredicate,
    },

    /// Rerank the results of a sub-query.
    Rerank {
        /// The sub-query whose results are reranked.
        child: Box<QueryNode>,
        /// Reranking method.
        method: RerankMethod,
        /// Number of results after reranking.
        top_k: usize,
    },

    /// An empty query that is guaranteed to produce no results.
    ///
    /// Useful as the result of constant-folding provably-empty sub-trees.
    Empty,
}

// ---------------------------------------------------------------------------
// Supporting enums
// ---------------------------------------------------------------------------

/// Fusion strategy for combining sub-query result lists.
///
/// Mirrors [`FusionStrategy`](crate::fusion::FusionStrategy) at the IR level
/// so that the query plan is self-contained and serialisable without depending
/// on runtime fusion internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FuseStrategy {
    /// Reciprocal Rank Fusion with smoothing constant `k`.
    ///
    /// Standard default: k = 60 (Craswell et al., 2009).
    Rrf {
        /// Smoothing constant.
        k: usize,
    },

    /// Weighted linear combination of scores.
    ///
    /// One weight per child; weights are normalised at execution time.
    Weighted {
        /// Per-child weights (will be normalised to sum to 1.0).
        weights: Vec<f64>,
    },

    /// Union with max-score-per-document semantics.
    Union,
}

/// Predicate for post-retrieval filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterPredicate {
    /// Keep only results whose score meets a minimum threshold.
    MinScore(DeterministicScore),

    /// Keep at most `k` results (top-k truncation).
    TopK(usize),

    /// Keep results where a metadata field equals a given value.
    MetadataEquals {
        /// Metadata field name.
        field: String,
        /// Expected value (JSON).
        value: serde_json::Value,
    },

    /// All contained predicates must hold (conjunction).
    And(Vec<FilterPredicate>),

    /// At least one contained predicate must hold (disjunction).
    Or(Vec<FilterPredicate>),
}

/// Method for reranking search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankMethod {
    /// Cross-encoder neural reranking (placeholder for future integration).
    CrossEncoder {
        /// Model identifier.
        model: String,
    },

    /// Score-based reranking with custom per-signal weights.
    ScoreWeighted {
        /// Weights applied to each scoring signal.
        weights: Vec<f64>,
    },
}

// ---------------------------------------------------------------------------
// Construction helpers
// ---------------------------------------------------------------------------

impl QueryNode {
    /// Create a vector search leaf node.
    pub fn vector(embedding: Vec<f32>, top_k: usize) -> Self {
        QueryNode::Vector {
            embedding,
            top_k,
            min_score: None,
        }
    }

    /// Create a keyword search leaf node.
    pub fn keyword(text: impl Into<String>, top_k: usize) -> Self {
        QueryNode::Keyword {
            text: text.into(),
            top_k,
            min_score: None,
        }
    }

    /// Create a hybrid query (vector + keyword with RRF fusion, `top_k * 3` candidates each).
    pub fn hybrid(embedding: Vec<f32>, text: impl Into<String>, top_k: usize) -> Self {
        let candidate_k = top_k.saturating_mul(3);
        QueryNode::Fuse {
            children: vec![
                QueryNode::vector(embedding, candidate_k),
                QueryNode::keyword(text, candidate_k),
            ],
            strategy: FuseStrategy::Rrf { k: 60 },
            top_k,
        }
    }

    /// Wrap this node with a minimum-score filter.
    #[must_use]
    pub fn with_min_score(self, min_score: DeterministicScore) -> Self {
        QueryNode::Filter {
            child: Box::new(self),
            predicate: FilterPredicate::MinScore(min_score),
        }
    }

    /// Wrap this node with a top-k truncation filter.
    #[must_use]
    pub fn with_top_k(self, k: usize) -> Self {
        QueryNode::Filter {
            child: Box::new(self),
            predicate: FilterPredicate::TopK(k),
        }
    }

    // -----------------------------------------------------------------------
    // Analysis helpers
    // -----------------------------------------------------------------------

    /// Returns `true` if this query is provably empty (no results possible).
    ///
    /// A query is provably empty when:
    /// - It is the `Empty` variant.
    /// - A leaf has `top_k == 0`.
    /// - A keyword leaf has empty text.
    /// - A fuse node has no children.
    /// - A filter/rerank wraps a provably-empty child.
    pub fn is_empty(&self) -> bool {
        match self {
            QueryNode::Empty => true,
            QueryNode::Vector { top_k: 0, .. } => true,
            QueryNode::Keyword { top_k: 0, .. } => true,
            QueryNode::Keyword { text, .. } if text.is_empty() => true,
            QueryNode::Fuse { children, .. } if children.is_empty() => true,
            QueryNode::Filter { child, .. } => child.is_empty(),
            QueryNode::Rerank { child, .. } => child.is_empty(),
            _ => false,
        }
    }

    /// Count the total number of leaf search operations in the tree.
    ///
    /// `Vector` and `Keyword` nodes each count as 1.  `Empty` counts as 0.
    /// Combinators recurse into their children.
    pub fn leaf_count(&self) -> usize {
        match self {
            QueryNode::Vector { .. } | QueryNode::Keyword { .. } => 1,
            QueryNode::Fuse { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
            QueryNode::Filter { child, .. } | QueryNode::Rerank { child, .. } => child.leaf_count(),
            QueryNode::Empty => 0,
        }
    }

    /// Return the effective `top_k` requested by this node.
    ///
    /// For `Filter` nodes with a `TopK` predicate, the predicate's value is
    /// returned.  Otherwise the child's `top_k` propagates upward.
    pub fn top_k(&self) -> usize {
        match self {
            QueryNode::Vector { top_k, .. } => *top_k,
            QueryNode::Keyword { top_k, .. } => *top_k,
            QueryNode::Fuse { top_k, .. } => *top_k,
            QueryNode::Filter { child, predicate } => match predicate {
                FilterPredicate::TopK(k) => *k,
                _ => child.top_k(),
            },
            QueryNode::Rerank { top_k, .. } => *top_k,
            QueryNode::Empty => 0,
        }
    }

    /// Return the depth of the IR tree (longest root-to-leaf path).
    ///
    /// Leaf nodes have depth 1.  `Empty` has depth 0.
    pub fn depth(&self) -> usize {
        match self {
            QueryNode::Empty => 0,
            QueryNode::Vector { .. } | QueryNode::Keyword { .. } => 1,
            QueryNode::Fuse { children, .. } => {
                1 + children.iter().map(|c| c.depth()).max().unwrap_or(0)
            }
            QueryNode::Filter { child, .. } | QueryNode::Rerank { child, .. } => 1 + child.depth(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
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
}
