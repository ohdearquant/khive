// REASON: format_args! inlining would break compatibility with older Rust toolchains in CI
#![allow(clippy::uninlined_format_args)]
// REASON: field_reassign_with_default is needed by the shadow-validation builder pattern in persist
#![allow(clippy::field_reassign_with_default)]
// REASON: benchmark helpers use hand-tuned constants close to Rust built-ins (e.g. 1.0/3.0)
#![allow(clippy::approx_constant)]

//! Hybrid search and ranking with deterministic scoring for khive.
//!
//! Combines HNSW vector search, BM25 keyword search, and RRF fusion into a unified
//! retrieval layer. All scores use `DeterministicScore` for cross-platform consistency.
//! See `docs/architecture.md` for module layout, design principles, ID bridging
//! strategies, and trait composition guide.

#![warn(missing_docs)]
#![warn(clippy::all)]

#[cfg(feature = "storage-adapters")]
pub mod adapters;
pub mod error;
pub mod eval;
pub mod hybrid;
pub mod metrics;
#[cfg(feature = "persist")]
pub mod persist;
pub mod policy;
pub mod query_ir;
#[cfg(feature = "persist")]
pub mod replay;
pub mod search_config;
pub mod timeout;
#[cfg(feature = "persist")]
pub mod weights;

// Re-export adapter types
#[cfg(feature = "storage-adapters")]
pub use adapters::{StorageKeywordSearch, StorageVectorSearch};

// Re-export core types
pub use error::{ErrorKind, Result, RetrievalError};

pub use khive_bm25::{Bm25Config, Bm25Index, Bm25Stats, DocumentId, SearchContext};
pub use khive_fusion::{
    fuse, normalize_weights, reciprocal_rank_fusion, weighted_fusion, weights_are_normalized,
    FusionStrategy, DEFAULT_RRF_K,
};
pub use khive_hnsw::{
    DistanceMetric, HnswCheckpointConfig, HnswConfig, HnswIndex, HnswSearchContext, HnswSnapshot,
    NodeId, RebuildStats, TombstoneStats,
};
// Formal proof: khive.Retrieval.HNSW.checkpoint_correctness
pub use hybrid::{
    fuse_search_results, fuse_search_results_checked, DualIndexConfig, DualIndexRouter,
    DualIndexStrategy, HybridConfig, HybridSearcher, KeywordSearch, Query, Reranker, VectorSearch,
};
#[cfg(feature = "checkpoint")]
pub use khive_hnsw::{HnswCheckpoint, HnswCheckpointStore};
// TODO(port-rerank): native cross-encoder reranking deferred; khive-inference not ported yet
// #[cfg(feature = "native-rerank")]
// pub use hybrid::{CrossEncoderScorer, NativeCrossEncoderReranker, RerankDocumentResolver};
pub use metrics::{MetricEvent, MetricValue, MetricsSink, NoopSink, RecordingSink};
#[cfg(feature = "persist")]
pub use persist::{
    PersistError, PersistenceStats, RetrievalPersistence, ShadowMetrics, ShadowValidationConfig,
    ShadowValidationResult,
};
pub use policy::{filter_by_policy, filter_by_predicate, ClearanceLevel, SearchPolicy};
pub use query_ir::{FilterPredicate, FuseStrategy, QueryNode, RerankMethod};
pub use search_config::SearchConfig;
pub use timeout::{
    search_with_cancellation, search_with_deadline, search_with_optional_timeout,
    search_with_timeout,
};

/// Re-exports from `lattice-embed`. Use these instead of depending on `lattice-embed` directly.
pub mod embed {
    // Core types and traits (always available, no feature gate needed)
    /// Result alias for embedding operations.
    pub use lattice_embed::Result as EmbedResult;
    pub use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    // Native model implementations (pure Rust lattice-embed via "embed" feature)
    #[cfg(feature = "embed")]
    pub use lattice_embed::{CachedEmbeddingService, NativeEmbeddingService};
}
