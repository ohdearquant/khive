//! Unified hybrid search interface.
//!
//! Combines HNSW vector search and BM25 keyword search with pluggable fusion (RRF by default).
//! VectorSearch, KeywordSearch, HybridSearcher, and Reranker are independent composable traits.

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
pub use searcher::{
    fuse_search_results, fuse_search_results_checked, HybridSearcher, KeywordSearch, Reranker,
    VectorSearch,
};
