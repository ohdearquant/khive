//! Deterministic fixed-point scoring and checked scalar arithmetic.
//!
//! Fixed-point scores retain their cross-platform ordering contract. Floating
//! helpers preserve their documented evaluation order without promising
//! cross-platform bit-identical results.

mod arithmetic_error;
mod band;
mod comparator;
mod decay;
pub mod distance;
mod ops;
mod score;
mod unit_score;
pub mod vector;
mod weights;

pub use arithmetic_error::ArithmeticError;
pub use band::{min_max_band, min_max_band_f32, min_max_band_fixed, DegenerateSpan};
pub use comparator::{
    cmp_asc_then_id, cmp_desc_then_id, cmp_desc_then_id_f32, cmp_desc_then_id_f64, Ranked,
};
pub use decay::{Decay, DecayParameter, HalfLife};
// REASON: re-export the deprecated legacy function so downstream callers receive
// the deprecation warning at their call sites, not here in the crate facade.
#[allow(deprecated)]
pub use distance::{
    score_from_distance, score_from_distance_lossy, try_cosine_score_with_f32_tolerance,
    try_score_from_distance,
};
pub use ops::{
    avg_scores, avg_scores_checked, max_score, min_score, rrf_score, rrf_score_one_based,
    rrf_score_zero_based, sum_scores, weighted_sum, ScoreError,
};
pub use score::DeterministicScore;
pub use unit_score::{
    affine_clamped, affine_clamped_f32, boost_factor, boost_factor_f32, UnitScore,
};
pub use weights::{scaled_weighted_mean, NamedWeights, ValidatedWeights};
