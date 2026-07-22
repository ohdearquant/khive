//! Vamana ANN index: batch-built approximate nearest-neighbor search over unit-normalized vectors.

pub mod config;
pub mod distance;
pub mod error;
#[cfg(feature = "mmap")]
pub mod external_ids;
pub mod graph;
pub mod index;

pub use config::VamanaConfig;
pub use error::{Result, VamanaError};
#[cfg(feature = "mmap")]
pub use external_ids::{
    read_external_ids_sidecar, segment_commit_digest, write_external_ids_sidecar,
    ExternalIdsWriteError,
};
pub use graph::{GreedySearchResult, VamanaGraph, VisitedSet};
#[cfg(feature = "mmap")]
pub use index::read_commit_fingerprint;
pub use index::{
    corpus_content_hash, CorpusFingerprint, PersistedFingerprint, VamanaIndex, VamanaIndexSnapshot,
    VamanaSnapshot, VAMANA_SNAPSHOT_FORMAT, VAMANA_SNAPSHOT_VERSION,
};
#[cfg(feature = "mmap")]
pub use index::{read_commit_info, PersistedCommitInfo};

/// Build a Vamana index from a flat row-major vector slice.
pub fn build(vectors: &[f32], config: VamanaConfig) -> Result<VamanaIndex> {
    VamanaIndex::build(vectors, config)
}

/// Search an index for the `k` nearest neighbors of `query`.
pub fn search(index: &VamanaIndex, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
    index.search(query, k)
}
