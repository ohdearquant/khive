//! Deterministic ranking with tie-breaking.

use crate::DeterministicScore;
use std::cmp::Ordering;

/// Max-heap ranking item: higher score wins and lower ID breaks ties.
///
/// Ordinary vector sorting is ascending; see
/// `crates/khive-score/docs/api/aggregation-and-ranking.md`.
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

/// Compare raw scores descending, with every NaN last and lower IDs breaking ties.
/// Non-NaN scores use `total_cmp`, so positive zero precedes negative zero.
/// NaN signs/payloads are equivalent when IDs match; scores are not quantized.
#[inline]
pub fn cmp_desc_then_id_f64<T: Ord>(a_score: f64, a_id: &T, b_score: f64, b_id: &T) -> Ordering {
    match (a_score.is_nan(), b_score.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => b_score.total_cmp(&a_score),
    }
    .then_with(|| a_id.cmp(b_id))
}

/// The raw f32 ordering follows `cmp_desc_then_id_f64` through lossless widening.
#[inline]
pub fn cmp_desc_then_id_f32<T: Ord>(a_score: f32, a_id: &T, b_score: f32, b_id: &T) -> Ordering {
    cmp_desc_then_id_f64(f64::from(a_score), a_id, f64::from(b_score), b_id)
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

    /// `Vec::sort` is ascending; ranking order requires `cmp_desc_then_id`.
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

    fn assert_raw_laws<S: Copy>(
        cases: &[(S, u8)],
        cmp: impl Fn(S, &u8, S, &u8) -> Ordering,
        equivalent: impl Fn(S, S) -> bool,
    ) {
        for &(a, ia) in cases {
            assert_eq!(cmp(a, &ia, a, &ia), Ordering::Equal);
            for &(b, ib) in cases {
                let ab = cmp(a, &ia, b, &ib);
                assert_eq!(ab, cmp(b, &ib, a, &ia).reverse());
                assert_eq!(ab == Ordering::Equal, ia == ib && equivalent(a, b));
                for &(c, ic) in cases {
                    let bc = cmp(b, &ib, c, &ic);
                    let ac = cmp(a, &ia, c, &ic);
                    if ab != Ordering::Greater && bc != Ordering::Greater {
                        assert_ne!(ac, Ordering::Greater);
                        if ab == Ordering::Less || bc == Ordering::Less {
                            assert_eq!(ac, Ordering::Less);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn raw_order_laws_include_generated_bits_and_nan_equivalence() {
        let bits64 = [
            0,
            1,
            0x8000_0000_0000_0000,
            0x8000_0000_0000_0001,
            0x3ff0_0000_0000_0000,
            0x3ff0_0000_0000_0001,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_0002,
            0xfff0_0000_0000_0001,
            0xfff8_0000_0000_0002,
        ];
        let bits32 = [
            0,
            1,
            0x8000_0000,
            0x8000_0001,
            0x3f80_0000,
            0x3f80_0001,
            0x7f80_0000,
            0xff80_0000,
            0x7f80_0001,
            0x7fc0_0002,
            0xff80_0001,
            0xffc0_0002,
        ];
        let mut cases64: Vec<_> = bits64
            .into_iter()
            .flat_map(|bits| [0u8, 1].map(|id| (f64::from_bits(bits), id)))
            .collect();
        let mut cases32: Vec<_> = bits32
            .into_iter()
            .flat_map(|bits| [0u8, 1].map(|id| (f32::from_bits(bits), id)))
            .collect();
        let mut seed = 0x1234_5678_9abc_def0u64;
        for id in 2..10 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            cases64.push((f64::from_bits(seed), id));
            cases32.push((f32::from_bits(seed as u32), id));
        }
        assert_raw_laws(&cases64, crate::cmp_desc_then_id_f64, |a, b| {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        });
        assert_raw_laws(&cases32, crate::cmp_desc_then_id_f32, |a, b| {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        });
        for &(a, ia) in &cases32 {
            for &(b, ib) in &cases32 {
                assert_eq!(
                    crate::cmp_desc_then_id_f32(a, &ia, b, &ib),
                    crate::cmp_desc_then_id_f64(f64::from(a), &ia, f64::from(b), &ib)
                );
            }
        }
    }

    #[test]
    fn raw_boundary_order_and_tuple_ids_are_explicit() {
        let rails64 = [
            f64::INFINITY,
            f64::MAX,
            0.5 + 2f64.powi(-40),
            0.5,
            f64::from_bits(1),
            0.0,
            -0.0,
            -f64::from_bits(1),
            -f64::MAX,
            f64::NEG_INFINITY,
        ];
        let rails32 = [
            f32::INFINITY,
            f32::MAX,
            f32::from_bits(0x3f00_0001),
            0.5,
            f32::from_bits(1),
            0.0,
            -0.0,
            -f32::from_bits(1),
            -f32::MAX,
            f32::NEG_INFINITY,
        ];
        for i in 0..rails64.len() {
            for j in i + 1..rails64.len() {
                assert_eq!(
                    crate::cmp_desc_then_id_f64(rails64[i], &9, rails64[j], &0),
                    Ordering::Less
                );
                assert_eq!(
                    crate::cmp_desc_then_id_f32(rails32[i], &9, rails32[j], &0),
                    Ordering::Less
                );
            }
        }
        for bits in [
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_0002,
            0xfff0_0000_0000_0001,
            0xfff8_0000_0000_0002,
        ] {
            assert_eq!(
                crate::cmp_desc_then_id_f64(f64::from_bits(bits), &0, f64::NEG_INFINITY, &9),
                Ordering::Greater
            );
        }
        for bits in [0x7f80_0001, 0x7fc0_0002, 0xff80_0001, 0xffc0_0002] {
            assert_eq!(
                crate::cmp_desc_then_id_f32(f32::from_bits(bits), &0, f32::NEG_INFINITY, &9),
                Ordering::Greater
            );
        }
        assert_eq!(
            crate::cmp_desc_then_id_f64(f64::NAN, &(4, 1), -f64::NAN, &(4, 2)),
            Ordering::Less
        );
        assert_eq!(
            crate::cmp_desc_then_id_f32(0.5, &(4, 2), 0.5, &(4, 1)),
            Ordering::Greater
        );
        assert_eq!(
            crate::cmp_desc_then_id_f64(0.5, &2, 0.5, &1),
            Ordering::Greater
        );
        assert_eq!(
            crate::cmp_desc_then_id_f32(f32::NAN, &2, -f32::NAN, &1),
            Ordering::Greater
        );
    }

    fn assert_shuffled<S: Copy>(original: &[(S, u8)], cmp: impl Fn(S, &u8, S, &u8) -> Ordering) {
        let expected = [4, 3, 2, 0, 1];
        let mut seed = 0x1234_5678u32;
        for _ in 0..64 {
            let mut items = original.to_vec();
            for i in (1..items.len()).rev() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                items.swap(i, seed as usize % (i + 1));
            }
            items.sort_unstable_by(|(a, ia), (b, ib)| cmp(*a, ia, *b, ib));
            assert_eq!(
                items.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn unique_id_shuffles_preserve_raw_ranking() {
        assert_shuffled(
            &[(f64::NAN, 1), (-f64::NAN, 0), (-0.0, 2), (0.0, 3), (0.5, 4)],
            crate::cmp_desc_then_id_f64,
        );
        assert_shuffled(
            &[(f32::NAN, 1), (-f32::NAN, 0), (-0.0, 2), (0.0, 3), (0.5, 4)],
            crate::cmp_desc_then_id_f32,
        );
    }
}
