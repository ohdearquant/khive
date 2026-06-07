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
mod tests {
    use super::*;
    use std::collections::BinaryHeap;

    #[derive(Debug)]
    struct Item {
        id: Uuid,
    }

    impl HasId for Item {
        fn id(&self) -> Uuid {
            self.id
        }
    }

    #[test]
    fn test_scored_entry_higher_score_pops_first() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);

        let mut heap = BinaryHeap::new();
        heap.push(ScoredEntry::new(Item { id: id_a }, 0.3, 0));
        heap.push(ScoredEntry::new(Item { id: id_b }, 0.9, 1));

        let first = heap.pop().unwrap();
        assert_eq!(first.id(), id_b, "Higher score pops first");
        assert!((first.score() - 0.9).abs() < 1e-10);
    }

    #[test]
    fn test_scored_entry_uuid_tie_breaking() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);

        let mut heap = BinaryHeap::new();
        heap.push(ScoredEntry::new(Item { id: id_b }, 0.5, 1));
        heap.push(ScoredEntry::new(Item { id: id_a }, 0.5, 0));

        // Lower UUID (id_a=1) should pop first
        let first = heap.pop().unwrap();
        assert_eq!(first.id(), id_a, "Lower UUID pops first on tie");
    }

    #[test]
    fn test_scored_entry_nan_pops_last() {
        let id_nan = Uuid::from_u128(1);
        let id_normal = Uuid::from_u128(2);

        let mut heap = BinaryHeap::new();
        heap.push(ScoredEntry::new(Item { id: id_nan }, f64::NAN, 0));
        heap.push(ScoredEntry::new(Item { id: id_normal }, 0.5, 1));

        let first = heap.pop().unwrap();
        assert_eq!(first.id(), id_normal, "Normal score pops before NaN");

        let second = heap.pop().unwrap();
        assert_eq!(second.id(), id_nan, "NaN pops last");
    }

    #[test]
    fn test_scored_entry_index_preserved() {
        let id = Uuid::from_u128(42);
        let entry = ScoredEntry::new(Item { id }, 0.7, 99);
        assert_eq!(entry.index(), 99);
    }

    #[test]
    fn test_scored_entry_equality_by_det_score_and_id() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(1); // same UUID
        let a = ScoredEntry::new(Item { id: id_a }, 0.5, 0);
        let b = ScoredEntry::new(Item { id: id_b }, 0.5, 99); // different index
        assert_eq!(a, b, "Equality ignores index");
    }

    #[test]
    fn test_scored_entry_det_score_accessor() {
        let id = Uuid::from_u128(1);
        let entry = ScoredEntry::new(Item { id }, 0.75, 0);
        let det = entry.det_score();
        assert!((det.to_f64() - 0.75).abs() < 1e-9);
    }
}
