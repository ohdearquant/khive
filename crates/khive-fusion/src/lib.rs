//! Fusion algorithms for combining retrieval results.
//!
//! This module implements rank fusion strategies for hybrid search, combining
//! results from multiple retrieval sources (e.g., vector search, keyword search).
//!
//! # Supported Strategies
//!
//! - **RRF (Reciprocal Rank Fusion)**: Default and recommended. Uses only ranks,
//!   making it robust to score distribution differences.
//! - **Weighted**: Linear combination of scores with configurable weights.
//! - **Union**: Takes the maximum score per ID across sources.
//!
//! # Algorithm
//!
//! RRF formula:
//! ```text
//! score(d) = Σ 1/(k + rank_i(d))
//! ```
//! where:
//! - k = 60 (standard, dampens high-rank dominance)
//! - rank_i(d) = position of d in retriever i's results (1-indexed)
//! - If d not in retriever i, contribution = 0
//!
//! # Example
//!
//! ```rust
//! use khive_fusion::{fuse, FusionStrategy, reciprocal_rank_fusion};
//! use khive_score::DeterministicScore;
//!
//! // Two retrieval sources with different rankings
//! let vector_results = vec![
//!     ("doc_a", DeterministicScore::from_f64(0.95)),
//!     ("doc_b", DeterministicScore::from_f64(0.90)),
//!     ("doc_c", DeterministicScore::from_f64(0.85)),
//! ];
//!
//! let keyword_results = vec![
//!     ("doc_b", DeterministicScore::from_f64(0.88)),
//!     ("doc_c", DeterministicScore::from_f64(0.75)),
//!     ("doc_d", DeterministicScore::from_f64(0.70)),
//! ];
//!
//! // Fuse using RRF with k=60
//! let fused = fuse(
//!     vec![vector_results, keyword_results],
//!     &FusionStrategy::Rrf { k: 60 },
//!     5,
//! );
//!
//! // doc_b appears in both sources, so it gets highest RRF score
//! assert_eq!(fused[0].0, "doc_b");
//! ```

mod fuse;
mod rrf;
mod strategy;
mod union;
mod weighted;

#[cfg(test)]
mod tests;

// Re-export public types and functions
pub use fuse::fuse;
pub use rrf::reciprocal_rank_fusion;
pub use strategy::{FusionStrategy, DEFAULT_RRF_K};
pub use union::union_fusion;
pub use weighted::{normalize_weights, weighted_fusion, weights_are_normalized};
