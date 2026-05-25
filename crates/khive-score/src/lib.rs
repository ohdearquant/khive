//! Cross-platform deterministic scoring.
//!
//! `DeterministicScore` converts f64 to fixed-point i64 (2^32 scale) for
//! identical ranking across x86_64, ARM64, and WASM.
//!
//! `score_from_distance` is the canonical distance-to-similarity conversion
//! used by all vector retrieval back-ends (HNSW, flat-scan, …).

mod comparator;
pub mod distance;
mod ops;
mod score;

pub use comparator::{cmp_asc_then_id, cmp_desc_then_id, Ranked};
pub use distance::score_from_distance;
pub use ops::{
    avg_scores, avg_scores_checked, max_score, min_score, rrf_score, sum_scores, weighted_sum,
    ScoreError,
};
pub use score::DeterministicScore;
