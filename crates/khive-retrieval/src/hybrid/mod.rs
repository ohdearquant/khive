//! Unified hybrid search interface.
//!
//! Combines a vector search and a keyword search with pluggable fusion (RRF by default).
//! VectorSearch, KeywordSearch, HybridSearcher, and Reranker are independent composable traits.

mod config;
#[cfg(feature = "native-rerank")]
mod cross_encoder;
pub mod dual_index;
#[cfg(feature = "executor")]
pub mod executor;
mod identity;
mod labelled;
mod labelled_scored;
mod searcher;
mod two_stage;

#[cfg(feature = "native-rerank")]
pub(crate) mod native_rerank {
    pub use super::cross_encoder::{
        BertLoraGeometry, CrossEncoderScorer, CrossEncoderScores, LoraHook, LoraProjectionShape,
        LoraTargetGeometry, NativeCrossEncoderModel, NativeCrossEncoderReranker,
        NativeRerankResult, NoopLoraHook, RerankDocumentResolver,
    };
}

// Re-export public types
pub use config::{HybridConfig, Query, DEFAULT_POOL_MULTIPLIER};
pub use dual_index::{DualIndexConfig, DualIndexRouter, DualIndexStrategy};
pub use identity::IdentityReranker;
pub use labelled::{
    combine_best_ranked_evidence, combine_leg_first_appearance, fuse_labelled, HitLabel,
};
pub use labelled_scored::fuse_labelled_scored;
#[cfg(feature = "native-rerank")]
pub use native_rerank::*;
pub use searcher::{
    fuse_search_results, fuse_search_results_checked, HybridSearcher, KeywordSearch, Reranker,
    VectorSearch,
};
pub use two_stage::fuse_two_stage;
