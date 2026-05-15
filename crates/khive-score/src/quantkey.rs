//! Lightweight quantized score key for hot loops (8 bytes).
//!
//! Packs a 32-bit quantized score + 32-bit ID prefix into 8 bytes
//! per ADR-006. NaN → 0 (neutral), matching DeterministicScore.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

/// 8-byte packed sort key: i32 quantized score + u32 ID prefix.
///
/// For sort-only operations where the full DeterministicScore is not needed.
/// Score descending, lower ID prefix wins ties.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct QuantKey {
    q: i32,
    id_prefix: u32,
}

impl Hash for QuantKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.q.hash(state);
        self.id_prefix.hash(state);
    }
}

impl QuantKey {
    const SCALE: f32 = 1_000_000.0;

    #[inline]
    pub fn new(score: f32, id_prefix: u32) -> Self {
        let s = if score.is_nan() { 0.0 } else { score };
        let q = (s * Self::SCALE)
            .round()
            .clamp(i32::MIN as f32, i32::MAX as f32) as i32;
        Self { q, id_prefix }
    }

    #[inline]
    pub fn from_f64(score: f64, id_prefix: u32) -> Self {
        Self::new(score as f32, id_prefix)
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
    pub fn id_prefix(&self) -> u32 {
        self.id_prefix
    }
}

impl Ord for QuantKey {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.q
            .cmp(&other.q)
            .then_with(|| other.id_prefix.cmp(&self.id_prefix))
    }
}

impl PartialOrd for QuantKey {
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
    fn size_is_8_bytes() {
        assert_eq!(std::mem::size_of::<QuantKey>(), 8);
    }

    #[test]
    fn precision() {
        let a = QuantKey::new(0.123456, 1);
        let b = QuantKey::new(0.123457, 2);
        assert_ne!(a.quantized_score(), b.quantized_score());
    }

    #[test]
    fn heap_order() {
        let mut heap: BinaryHeap<QuantKey> = BinaryHeap::new();
        heap.push(QuantKey::new(0.95, 3));
        heap.push(QuantKey::new(0.95, 1));
        heap.push(QuantKey::new(0.95, 2));
        heap.push(QuantKey::new(0.87, 4));

        assert_eq!(heap.pop().unwrap().id_prefix(), 1);
        assert_eq!(heap.pop().unwrap().id_prefix(), 2);
        assert_eq!(heap.pop().unwrap().id_prefix(), 3);
        assert_eq!(heap.pop().unwrap().id_prefix(), 4);
    }

    #[test]
    fn nan_maps_to_zero() {
        let nan_key = QuantKey::new(f32::NAN, 1);
        let zero_key = QuantKey::new(0.0, 1);
        assert_eq!(nan_key.quantized_score(), zero_key.quantized_score());
    }
}
