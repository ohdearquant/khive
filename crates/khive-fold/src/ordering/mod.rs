//! Deterministic ordering primitives: `HasId`, `ScoredEntry`, canonical f32/f64, UUID tie-breaking.

mod canonical;
mod compare;
mod has_id;
mod scored_entry;

pub use canonical::{canonical_f32, canonical_f64};
pub use compare::{cmp_asc_score_then_id, cmp_desc_score_then_id};
pub use has_id::HasId;
pub use scored_entry::ScoredEntry;

// Re-exports from khive-score
pub use khive_score::{cmp_asc_then_id, cmp_desc_then_id, DeterministicScore, Ranked};

#[cfg(test)]
mod tests;
