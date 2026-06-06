//! Rank fusion strategies for hybrid search (ADR-006, ADR-012).
//!
//! Combines ranked result lists from multiple retrievers into a single ranked
//! output using [`DeterministicScore`](khive_score::DeterministicScore).
//!
//! Strategies: RRF (default), Weighted, Union, VectorOnly, KeywordOnly, Custom.
//! All built-in strategies produce deterministic output with ID-based tie-breaking.
//!
//! ```rust
//! use khive_fusion::{fuse, FusionStrategy};
//! use khive_score::DeterministicScore;
//!
//! let sources = vec![
//!     vec![("a", DeterministicScore::from_f64(0.9)),
//!          ("b", DeterministicScore::from_f64(0.8))],
//!     vec![("b", DeterministicScore::from_f64(0.95))],
//! ];
//! let fused = fuse(sources, &FusionStrategy::Rrf { k: 60 }, 5).unwrap();
//! assert_eq!(fused[0].0, "b");
//! ```

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
