//! `khive-vamana` — batch-built Vamana ANN index for pre-normalized `f32` vectors.
//!
//! All vectors must be unit-normalized before insertion. The crate validates
//! vector dimensionality but not norms.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use khive_vamana::{VamanaConfig, build, search};
//!
//! let config = VamanaConfig::with_dimensions(128);
//! let index = build(&vectors, config)?;
//! let results = search(&index, &query, 10)?;
//! ```

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

pub fn build(vectors: &[f32], config: VamanaConfig) -> Result<VamanaIndex> {
    VamanaIndex::build(vectors, config)
}

pub fn search(index: &VamanaIndex, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
    index.search(query, k)
}
