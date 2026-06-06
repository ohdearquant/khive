//! Fusion algorithms for combining retrieval results from multiple sources.
//!
//! Strategies: RRF (default, rank-based), Weighted (score-based), Union (max per ID),
//! VectorOnly, KeywordOnly. See `docs/algorithm.md` for the RRF formula, k=60
//! rationale, score normalization contract, and weighted fusion considerations.

mod fuse;
mod rrf;
mod strategy;
mod union;
mod weighted;

// Re-export public types and functions
pub use fuse::fuse;
pub use rrf::reciprocal_rank_fusion;
pub use strategy::{FusionStrategy, DEFAULT_RRF_K};
pub use union::union_fusion;
pub use weighted::{
    normalize_weights, try_normalize_weights, weighted_fusion, weights_are_normalized,
};
