//! Unified hybrid search interface.
//!
//! Combines HNSW vector search, BM25 keyword search, and graph traversal
//! into a single query interface with configurable fusion strategies.
//!
//! # Architecture (ADR-002)
//!
//! ```text
//! Query ──┬── [Vector Search] ── HNSW ── Vec<(Id, Distance)>
//!         │                                    │
//!         │                              distance → similarity
//!         │                                    │
//!         │                              Vec<(Id, DeterministicScore)>
//!         │                                    │
//!         └── [Keyword Search] ── BM25 ── Vec<(Id, BM25Score)>
//!                                              │
//!                                        normalize → DeterministicScore
//!                                              │
//!                                        Vec<(Id, DeterministicScore)>
//!                                              │
//!                                ┌─────────────┴─────────────┐
//!                                │   reciprocal_rank_fusion  │
//!                                │      k=60 (standard)      │
//!                                └─────────────┬─────────────┘
//!                                              │
//!                                    Vec<(Id, DeterministicScore)>
//! ```
//!
//! # Trait Hierarchy
//!
//! ```text
//! VectorSearch ──┐
//!                ├── HybridSearcher
//! KeywordSearch ─┘
//!
//! Reranker (standalone, generic over Id)
//! ```
//!
//! Each trait can be implemented independently:
//! - [`VectorSearch`]: Embedding-based nearest-neighbor search (e.g., HNSW)
//! - [`KeywordSearch`]: Text-based retrieval (e.g., BM25)
//! - [`HybridSearcher`]: Combined search requiring both vector + keyword
//! - [`Reranker`]: Post-retrieval reranking (e.g., cross-encoder)
//!
//! # Fusion Strategies
//!
//! - **RRF (Reciprocal Rank Fusion)**: Default and recommended. Uses only ranks,
//!   making it robust to score distribution differences.
//! - **Weighted**: Linear combination of scores with configurable weights.
//! - **Union**: Takes the maximum score per ID across sources.
//!
//! # Example
//!
//! ```rust,ignore
//! use khive_retrieval::hybrid::{
//!     HybridConfig, HybridSearcher, VectorSearch, KeywordSearch, Query, fuse_search_results,
//! };
//! use khive_score::DeterministicScore;
//!
//! // Create your own searcher implementing VectorSearch + KeywordSearch + HybridSearcher
//! // Then use fuse_search_results to combine vector and keyword results
//!
//! let vector_results = vec![("doc1".to_string(), DeterministicScore::from_f64(0.9))];
//! let keyword_results = vec![("doc1".to_string(), DeterministicScore::from_f64(0.85))];
//!
//! let config = HybridConfig::new(10);
//! let fused = fuse_search_results(vec![vector_results, keyword_results], &config);
//! ```
//!
//! See [ADR-002](../docs/ADR-002-hybrid-search.md) for algorithm specification.

mod config;
#[cfg(feature = "native-rerank")]
mod cross_encoder;
pub mod dual_index;
mod searcher;

// Re-export public types
pub use config::{HybridConfig, Query, DEFAULT_POOL_MULTIPLIER};
#[cfg(feature = "native-rerank")]
pub use cross_encoder::{CrossEncoderScorer, NativeCrossEncoderReranker, RerankDocumentResolver};
pub use dual_index::{DualIndexConfig, DualIndexRouter, DualIndexStrategy};
pub use searcher::{fuse_search_results, HybridSearcher, KeywordSearch, Reranker, VectorSearch};
