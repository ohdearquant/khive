//! Vamana ANN index for batch-built approximate nearest neighbor search.
//!
//! All vectors must be unit-normalized before insertion; dimensionality is
//! validated at every public boundary, but unit-norm is not enforced (the
//! adjacent bridge normalizes before calling here). Non-finite float values
//! (`NaN`, `Infinity`) are rejected at [`VamanaIndex::build`],
//! [`VamanaIndex::search`], and [`VamanaIndex::from_snapshot`].
//!
//! See ADR-048 for default parameters (`max_degree=64`, `alpha=1.2`).

pub mod config;
pub mod distance;
pub mod error;
pub mod graph;
pub mod index;

pub use config::VamanaConfig;
pub use error::{Result, VamanaError};
pub use graph::{GreedySearchResult, VamanaGraph, VisitedSet};
pub use index::{
    CorpusFingerprint, VamanaIndex, VamanaIndexSnapshot, VamanaSnapshot, VAMANA_SNAPSHOT_FORMAT,
    VAMANA_SNAPSHOT_VERSION,
};

/// Build a Vamana index from a flat row-major vector slice and a config.
///
/// Delegates to [`VamanaIndex::build`]; see that method for full error contract.
pub fn build(vectors: &[f32], config: VamanaConfig) -> Result<VamanaIndex> {
    VamanaIndex::build(vectors, config)
}

/// Search an already-built index for the `k` nearest neighbors of `query`.
///
/// Delegates to [`VamanaIndex::search`]; see that method for full error contract.
pub fn search(index: &VamanaIndex, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
    index.search(query, k)
}
