//! Lightweight quantized score key for hot loops (8 bytes vs 16).

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

/// Quantized score key: i32 (1e-6 resolution) + ID tie-breaker.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct QuantKey<T: Ord + Copy> {
    q: i32,
    id: T,
}

impl<T: Ord + Copy + Hash> Hash for QuantKey<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.q.hash(state);
        self.id.hash(state);
    }
}

impl<T: Ord + Copy> QuantKey<T> {
    const SCALE: f32 = 1_000_000.0;

    #[inline]
    pub fn new(score: f32, id: T) -> Self {
        let s = if score.is_nan() { 0.0 } else { score };
        let q = (s * Self::SCALE)
            .round()
            .clamp(i32::MIN as f32, i32::MAX as f32) as i32;
        Self { q, id }
    }

    #[inline]
    pub fn from_f64(score: f64, id: T) -> Self {
        Self::new(score as f32, id)
    }

    #[inline]
    pub fn quantized_score(&self) -> i32 {
        self.q
    }

    #[inline]
    pub fn score(&self) -> f32 {
        self.q as f32 / Self::SCALE
    }

    #[inline]
    pub fn id(&self) -> T {
        self.id
    }
}

impl<T: Ord + Copy> Ord for QuantKey<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.q.cmp(&other.q).then_with(|| other.id.cmp(&self.id))
    }
}

impl<T: Ord + Copy> PartialOrd for QuantKey<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BinaryHeap;

    #[test]
    fn size() {
        assert_eq!(std::mem::size_of::<QuantKey<u32>>(), 8);
    }

    #[test]
    fn precision() {
        let a = QuantKey::new(0.123456, 1u32);
        let b = QuantKey::new(0.123457, 2u32);
        assert_ne!(a.quantized_score(), b.quantized_score());
    }

    #[test]
    fn heap_order() {
        let mut heap: BinaryHeap<QuantKey<u32>> = BinaryHeap::new();
        heap.push(QuantKey::new(0.95, 3));
        heap.push(QuantKey::new(0.95, 1));
        heap.push(QuantKey::new(0.95, 2));
        heap.push(QuantKey::new(0.87, 4));

        assert_eq!(heap.pop().unwrap().id(), 1);
        assert_eq!(heap.pop().unwrap().id(), 2);
        assert_eq!(heap.pop().unwrap().id(), 3);
        assert_eq!(heap.pop().unwrap().id(), 4);
    }

    #[test]
    fn nan_maps_to_zero() {
        let nan_key = QuantKey::new(f32::NAN, 1u32);
        let zero_key = QuantKey::new(0.0, 1u32);
        assert_eq!(nan_key.quantized_score(), zero_key.quantized_score());
    }
}
