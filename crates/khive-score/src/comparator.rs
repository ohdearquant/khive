//! Deterministic ranking with tie-breaking.

use crate::DeterministicScore;
use std::cmp::Ordering;

/// Scored item implementing max-heap `Ord`: higher score wins, lower ID breaks ties.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ranked<T: Ord> {
    score: DeterministicScore,
    id: T,
}

impl<T: Ord> Ranked<T> {
    /// Construct a `Ranked` item from a score and a tie-breaking ID.
    #[inline]
    pub fn new(score: DeterministicScore, id: T) -> Self {
        Self { score, id }
    }

    /// Return the score component.
    #[inline]
    pub fn score(&self) -> DeterministicScore {
        self.score
    }

    /// Return a reference to the tie-breaking ID.
    #[inline]
    pub fn id(&self) -> &T {
        &self.id
    }

    /// Consume `self` and return the ID.
    #[inline]
    pub fn into_id(self) -> T {
        self.id
    }

    /// Consume `self` and return `(score, id)`.
    #[inline]
    pub fn into_parts(self) -> (DeterministicScore, T) {
        (self.score, self.id)
    }
}

impl<T: Ord> Ord for Ranked<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl<T: Ord> PartialOrd for Ranked<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compare scores descending, lower ID wins ties.
#[inline(always)]
pub fn cmp_desc_then_id<T: Ord>(
    a_score: DeterministicScore,
    a_id: &T,
    b_score: DeterministicScore,
    b_id: &T,
) -> Ordering {
    b_score.cmp(&a_score).then_with(|| a_id.cmp(b_id))
}

/// Compare scores ascending, lower ID wins ties.
#[inline(always)]
pub fn cmp_asc_then_id<T: Ord>(
    a_score: DeterministicScore,
    a_id: &T,
    b_score: DeterministicScore,
    b_id: &T,
) -> Ordering {
    a_score.cmp(&b_score).then_with(|| a_id.cmp(b_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BinaryHeap;

    #[test]
    fn ranked_heap_determinism() {
        let mut heap: BinaryHeap<Ranked<u64>> = BinaryHeap::new();
        heap.push(Ranked::new(DeterministicScore::from_f64(0.95), 3));
        heap.push(Ranked::new(DeterministicScore::from_f64(0.95), 1));
        heap.push(Ranked::new(DeterministicScore::from_f64(0.95), 2));
        heap.push(Ranked::new(DeterministicScore::from_f64(0.87), 4));

        let results: Vec<_> = std::iter::from_fn(|| heap.pop()).collect();
        assert_eq!(results[0].id(), &1);
        assert_eq!(results[1].id(), &2);
        assert_eq!(results[2].id(), &3);
        assert_eq!(results[3].id(), &4);
    }

    #[test]
    fn cmp_desc() {
        let mut items = [
            (DeterministicScore::from_f64(0.9), 2u64),
            (DeterministicScore::from_f64(0.9), 1u64),
            (DeterministicScore::from_f64(0.8), 3u64),
        ];
        items.sort_by(|(sa, ia), (sb, ib)| cmp_desc_then_id(*sa, ia, *sb, ib));
        assert_eq!(items[0].1, 1);
        assert_eq!(items[1].1, 2);
        assert_eq!(items[2].1, 3);
    }

    #[test]
    fn cmp_asc_then_id_tie_lower_id_wins() {
        let score = DeterministicScore::from_f64(0.5);
        let mut items = [(score, 3u64), (score, 1u64), (score, 2u64)];
        items.sort_by(|(sa, ia), (sb, ib)| cmp_asc_then_id(*sa, ia, *sb, ib));
        assert_eq!(items[0].1, 1);
        assert_eq!(items[1].1, 2);
        assert_eq!(items[2].1, 3);
    }

    #[test]
    fn ranked_into_parts_returns_score_and_id() {
        let score = DeterministicScore::from_f64(0.75);
        let ranked = Ranked::new(score, 42u64);
        let (s, id) = ranked.into_parts();
        assert_eq!(s, score);
        assert_eq!(id, 42u64);
    }

    // ── Ranked Ord is max-heap adapter, not natural sort order ───────────────

    /// `BinaryHeap<Ranked<_>>` must pop best (highest score) first.
    #[test]
    fn ranked_heap_pops_highest_score_first() {
        use std::collections::BinaryHeap;
        let mut heap = BinaryHeap::new();
        heap.push(Ranked::new(DeterministicScore::from_f64(0.3), 3u64));
        heap.push(Ranked::new(DeterministicScore::from_f64(0.9), 1u64));
        heap.push(Ranked::new(DeterministicScore::from_f64(0.5), 2u64));

        let first = heap.pop().unwrap();
        assert_eq!(first.score(), DeterministicScore::from_f64(0.9));
        assert_eq!(first.id(), &1u64);
    }

    /// `Vec<Ranked<_>>::sort()` produces ascending order (lowest score first)
    /// because `Ranked::Ord` is a max-heap adapter.  This test documents that
    /// behaviour — callers who need descending order MUST use `cmp_desc_then_id`.
    #[test]
    fn ranked_vec_sort_is_ascending_not_ranking_order() {
        let mut items = [
            Ranked::new(DeterministicScore::from_f64(0.9), 1u64),
            Ranked::new(DeterministicScore::from_f64(0.3), 3u64),
            Ranked::new(DeterministicScore::from_f64(0.5), 2u64),
        ];
        items.sort();
        // sort() gives ascending (lowest score first) because Ord is max-heap-adapted.
        assert_eq!(items[0].score(), DeterministicScore::from_f64(0.3));
        assert_eq!(items[2].score(), DeterministicScore::from_f64(0.9));
    }

    /// To get descending (ranking) order, use `cmp_desc_then_id`.
    #[test]
    fn cmp_desc_then_id_gives_descending_order() {
        let mut items: Vec<(DeterministicScore, u64)> = vec![
            (DeterministicScore::from_f64(0.3), 3),
            (DeterministicScore::from_f64(0.9), 1),
            (DeterministicScore::from_f64(0.5), 2),
        ];
        items.sort_unstable_by(|(sa, ia), (sb, ib)| cmp_desc_then_id(*sa, ia, *sb, ib));
        assert_eq!(items[0].1, 1); // 0.9 first
        assert_eq!(items[1].1, 2); // 0.5 second
        assert_eq!(items[2].1, 3); // 0.3 last
    }
}
