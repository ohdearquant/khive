//! Cross-platform deterministic scoring via fixed-point i64 (2^32 scale).

mod comparator;
pub mod distance;
mod ops;
mod score;

pub use comparator::{cmp_asc_then_id, cmp_desc_then_id, Ranked};
// REASON: re-export the deprecated legacy function so downstream callers receive
// the deprecation warning at their call sites, not here in the crate facade.
#[allow(deprecated)]
pub use distance::{score_from_distance, score_from_distance_lossy, try_score_from_distance};
pub use ops::{
    avg_scores, avg_scores_checked, max_score, min_score, rrf_score, rrf_score_one_based,
    rrf_score_zero_based, sum_scores, weighted_sum, ScoreError,
};
pub use score::DeterministicScore;
