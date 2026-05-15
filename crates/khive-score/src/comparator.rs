//! Deterministic ranking with tie-breaking.

use crate::DeterministicScore;
use std::cmp::Ordering;

/// Ranked item: score descending, ID ascending for ties.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ranked<T: Ord> {
    score: DeterministicScore,
    id: T,
}

impl<T: Ord> Ranked<T> {
    #[inline]
    pub fn new(score: DeterministicScore, id: T) -> Self {
        Self { score, id }
    }

    #[inline]
    pub fn score(&self) -> DeterministicScore {
        self.score
    }

    #[inline]
    pub fn id(&self) -> &T {
        &self.id
    }

    #[inline]
    pub fn into_id(self) -> T {
        self.id
    }

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
}
