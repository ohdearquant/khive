//! Deterministic comparison functions for scored candidates.

use std::cmp::Ordering;
use uuid::Uuid;

use khive_score::{cmp_asc_then_id, cmp_desc_then_id, DeterministicScore};

/// Descending score order with UUID tie-breaking; NaN maps to ZERO.
#[inline]
pub fn cmp_desc_score_then_id(score_a: f64, id_a: Uuid, score_b: f64, id_b: Uuid) -> Ordering {
    cmp_desc_then_id(
        DeterministicScore::from_f64(score_a),
        &id_a,
        DeterministicScore::from_f64(score_b),
        &id_b,
    )
}

/// Ascending score order with UUID tie-breaking; NaN maps to ZERO.
#[inline]
pub fn cmp_asc_score_then_id(score_a: f64, id_a: Uuid, score_b: f64, id_b: Uuid) -> Ordering {
    cmp_asc_then_id(
        DeterministicScore::from_f64(score_a),
        &id_a,
        DeterministicScore::from_f64(score_b),
        &id_b,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_descending_score_ordering() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);

        assert_eq!(
            cmp_desc_score_then_id(0.9, id_a, 0.5, id_b),
            Ordering::Less,
            "Higher score should come first"
        );
        assert_eq!(
            cmp_desc_score_then_id(0.5, id_a, 0.9, id_b),
            Ordering::Greater,
            "Lower score should come second"
        );
    }

    #[test]
    fn test_uuid_tie_breaking() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);

        assert_eq!(
            cmp_desc_score_then_id(0.5, id_a, 0.5, id_b),
            Ordering::Less,
            "Lower UUID should come first on tie"
        );
        assert_eq!(
            cmp_desc_score_then_id(0.5, id_b, 0.5, id_a),
            Ordering::Greater,
            "Higher UUID should come second on tie"
        );
    }

    #[test]
    fn test_nan_handling() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);

        // NaN maps to DeterministicScore::ZERO (neutral), so positive scores come first.
        assert_eq!(
            cmp_desc_score_then_id(f64::NAN, id_a, 0.5, id_b),
            Ordering::Greater,
            "NaN should sort after positive values in descending"
        );
        assert_eq!(
            cmp_desc_score_then_id(0.5, id_a, f64::NAN, id_b),
            Ordering::Less,
            "Normal value should sort before NaN in descending"
        );
        assert_eq!(
            cmp_desc_score_then_id(f64::NAN, id_a, f64::NAN, id_b),
            Ordering::Less,
            "Two NaNs should use UUID tie-breaking"
        );
    }

    #[test]
    fn test_sorting_stability() {
        let entries: Vec<(f64, Uuid)> = (0..100)
            .map(|i| (0.5, Uuid::from_u128(i as u128)))
            .collect();

        let mut sorted1 = entries.clone();
        let mut sorted2 = entries.clone();

        sorted1.sort_by(|a, b| cmp_desc_score_then_id(a.0, a.1, b.0, b.1));
        sorted2.sort_by(|a, b| cmp_desc_score_then_id(a.0, a.1, b.0, b.1));

        assert_eq!(sorted1, sorted2);

        for i in 0..99 {
            assert!(sorted1[i].1 < sorted1[i + 1].1);
        }
    }

    #[test]
    fn test_ascending_variant() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);

        assert_eq!(
            cmp_asc_score_then_id(0.3, id_a, 0.5, id_b),
            Ordering::Less,
            "Lower score should come first in ascending"
        );
        assert_eq!(
            cmp_asc_score_then_id(0.5, id_a, 0.5, id_b),
            Ordering::Less,
            "Equal scores use UUID tie-breaking"
        );
    }
}
