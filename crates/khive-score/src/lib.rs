//! Cross-platform deterministic scoring.
//!
//! `DeterministicScore` converts f64 to fixed-point i64 (2^32 scale) for
//! identical ranking across x86_64, ARM64, and WASM.
//!
//! Vector similarity (dot product, cosine) is not in this crate — it belongs
//! with the embedding implementation (lattice).

mod comparator;
mod ops;
mod quantkey;
mod score;

pub use comparator::{cmp_asc_then_id, cmp_desc_then_id, Ranked};
pub use ops::{
    avg_scores, avg_scores_checked, max_score, min_score, rrf_score, sum_scores, weighted_sum,
    ScoreError,
};
pub use quantkey::QuantKey;
pub use score::DeterministicScore;
