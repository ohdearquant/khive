//! ScoredEntry wrapper for heap operations

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use uuid::Uuid;

use khive_score::DeterministicScore;

use super::has_id::HasId;

/// Scored candidate with deterministic `Ord`: score descending, UUID ascending on tie.
#[derive(Debug, Clone, Copy)]
pub struct ScoredEntry<T> {
    /// The candidate being scored
    candidate: T,
    /// Cached raw score value (human-readable)
    score: f64,
    /// Cached UUID for tie-breaking
    id: Uuid,
    /// Original index in the candidate list
    index: usize,
    /// Deterministic fixed-point score for ordering
    det_score: DeterministicScore,
}

impl<T: HasId> ScoredEntry<T> {
    /// Create a new scored entry.
    #[inline]
    pub fn new(candidate: T, score: f64, index: usize) -> Self {
        let id = candidate.id();
        let det_score = DeterministicScore::from_f64(score);
        Self {
            candidate,
            score,
            id,
            index,
            det_score,
        }
    }

    /// Get the candidate reference.
    #[inline]
    pub fn candidate(&self) -> &T {
        &self.candidate
    }

    /// Consume and return the candidate.
    #[inline]
    pub fn into_candidate(self) -> T {
        self.candidate
    }

    /// Get the cached score.
    #[inline]
    pub fn score(&self) -> f64 {
        self.score
    }

    /// Get the cached UUID.
    #[inline]
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Get the original index.
    #[inline]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Get the deterministic fixed-point score.
    #[inline]
    pub fn det_score(&self) -> DeterministicScore {
        self.det_score
    }
}

impl<T> Eq for ScoredEntry<T> {}

impl<T> PartialEq for ScoredEntry<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.det_score == other.det_score && self.id == other.id
    }
}

impl<T> Ord for ScoredEntry<T> {
    /// Compare entries for use with BinaryHeap and sorted collections.
    ///
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.det_score
            .cmp(&other.det_score)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl<T> PartialOrd for ScoredEntry<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Hash for ScoredEntry<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.det_score.hash(state);
        self.id.hash(state);
    }
}

#[cfg(test)]
#[path = "scored_entry_tests.rs"]
mod tests;
