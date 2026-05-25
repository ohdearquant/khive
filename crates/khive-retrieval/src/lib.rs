#![allow(clippy::uninlined_format_args)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::approx_constant)]
// Note: field_reassign_with_default is needed for some internal tests

//! Hybrid search and ranking with deterministic scoring for khive.
//!
//! This crate provides:
//! - HNSW vector search with `DeterministicScore` output
//! - BM25 keyword search for exact matches
//! - Reciprocal Rank Fusion (RRF) for hybrid search
//! - Graph traversal for relationship-aware retrieval
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      khive-retrieval                             │
//! │  ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌───────────┐    │
//! │  │   hnsw/   │  │   bm25/   │  │  graph/   │  │  fusion/  │    │
//! │  │ (vector)  │  │ (keyword) │  │(traversal)│  │   (RRF)   │    │
//! │  └───────────┘  └───────────┘  └───────────┘  └───────────┘    │
//! │                       │                                          │
//! │                       ▼                                          │
//! │               ┌───────────────┐                                  │
//! │               │    hybrid/    │                                  │
//! │               │   (unified)   │                                  │
//! │               └───────────────┘                                  │
//! │                                                                  │
//! │  Inputs: Query + optional embedding + optional start nodes       │
//! │  Outputs: Vec<(Id, DeterministicScore)>                         │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Design Principles
//!
//! ## Deterministic Scoring (ADR-002)
//!
//! All scores use `DeterministicScore` from `khive-score` for:
//! - Cross-platform identical rankings (x86_64, ARM64, WASM)
//! - `Ord` implementation (sortable, usable in BTreeSet)
//! - `Hash` implementation (cacheable)
//!
//! ## Index Management (ADR-003)
//!
//! - HNSW: Hierarchical Navigable Small World graphs for ANN search
//! - BM25: Okapi BM25 for keyword relevance
//! - Both support incremental updates with periodic rebuild
//!
//! ## Graph Traversal (ADR-004)
//!
//! - BFS for level-by-level exploration
//! - DFS for deep path exploration
//! - Bidirectional BFS for shortest path
//!
//! ## ID Types and Bridging
//!
//! Each retrieval module uses a different ID type:
//!
//! | Module | ID Type | Backing |
//! |--------|---------|---------|
//! | HNSW   | [`EmbeddingId`] | 128-bit (ULID, from khive-types) |
//! | BM25   | [`DocumentId`]  | Newtype over `String` |
//! | Graph  | `EntityRef`     | Enum (from khive-db) |
//! | Fusion | Generic `Id`    | `Eq + Hash + Clone + Ord` |
//!
//! The [`fusion::fuse`] function is generic over the ID type, so hybrid
//! search that combines results from different modules requires a common
//! representation. Bridging strategies:
//!
//! 1. **String-based**: Convert all IDs to `String` before fusion.
//! 2. **DocumentId-based**: Convert `EmbeddingId` to `DocumentId` via
//!    `DocumentId::new(embedding_id.to_string())`.
//! 3. **Application-level mapping**: Maintain a bidirectional lookup table
//!    between ID types in the application layer.
//!
//! See [`DocumentId`] for details on the newtype and conversion traits.
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use khive_retrieval::{VectorSearch, KeywordSearch, HybridSearcher, Query, HybridConfig};
//!
//! // Implement granular traits independently:
//! // - VectorSearch for embedding-based search (HNSW)
//! // - KeywordSearch for text-based search (BM25)
//! // - HybridSearcher for combined search (requires both)
//! // - Reranker for post-retrieval reranking (standalone)
//!
//! // Example: keyword-only search
//! let results = searcher.keyword_search("distributed systems", 10).await?;
//!
//! // Example: hybrid search (vector + keyword with fusion)
//! let query = Query::hybrid("distributed systems", embedding_vec);
//! let config = HybridConfig::new(10);
//! let results = searcher.hybrid_search(&query, &config).await?;
//!
//! for (id, score) in results {
//!     println!("{}: {}", id, score);
//! }
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

#[cfg(feature = "storage-adapters")]
pub mod adapters;
pub mod error;
pub mod eval;
// graph module depends on EntityRef/LinkStore/StorageContext from old monolith khive-db API;
// gated until ported to current khive-storage GraphStore trait.
#[cfg(feature = "graph-legacy")]
pub mod graph;
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

// Re-export types from sibling crates (now separate crates)
#[cfg(feature = "graph-legacy")]
pub use graph::{
    bfs_traverse, dfs_traverse, find_shortest_path, Direction, PathNode, TraversalOptions,
    MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_RESULTS,
};
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
    fuse_search_results, DualIndexConfig, DualIndexRouter, DualIndexStrategy, HybridConfig,
    HybridSearcher, KeywordSearch, Query, Reranker, VectorSearch,
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

/// Re-exports from `lattice-embed` for app-layer access.
///
/// Apps should use these re-exports instead of depending on `lattice-embed` directly.
/// This maintains the layer boundary: apps -> platform (retrieval) -> foundation (embed).
///
/// Core types (`EmbeddingModel`, `EmbeddingService`, `EmbedError`) are always available.
/// Native model implementations (`NativeEmbeddingService`, etc.) require the `embed` feature.
pub mod embed {
    // Core types and traits (always available, no feature gate needed)
    /// Result alias for embedding operations.
    pub use lattice_embed::Result as EmbedResult;
    pub use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    // Native model implementations (pure Rust lattice-embed via "embed" feature)
    #[cfg(feature = "embed")]
    pub use lattice_embed::{CachedEmbeddingService, NativeEmbeddingService};
}
