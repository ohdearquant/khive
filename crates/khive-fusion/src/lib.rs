//! Rank fusion strategies (RRF, Weighted, Union, VectorOnly, KeywordOnly, Custom) for hybrid search.

mod fuse;
mod rrf;
mod strategy;
mod union;
mod weighted;

#[cfg(test)]
mod tests;

// Re-export public types and functions
pub use fuse::{fuse, FuseError};
pub use rrf::reciprocal_rank_fusion;
pub use strategy::{FusionStrategy, FusionStrategyError, DEFAULT_RRF_K};
pub use union::union_fusion;
pub use weighted::{
    normalize_weights, try_normalize_weights, weighted_fusion, weights_are_normalized,
};
