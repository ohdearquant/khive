use super::canonical::{CANONICAL_NAN_F32, CANONICAL_NAN_F64};
use super::*;
use std::cmp::Ordering;
use uuid::Uuid;

// ------------------------------------------------------------------------
// Canonical Function Tests
// ------------------------------------------------------------------------

#[test]
fn test_canonical_f64_nan_variants() {
    let nans = [
        f64::NAN,
        -f64::NAN,
        f64::from_bits(0x7ff0_0000_0000_0001), // signaling NaN
        f64::from_bits(0x7ff8_0000_0000_0001), // quiet NaN with payload
        f64::from_bits(0xfff8_0000_0000_0001), // negative quiet NaN
    ];

    for nan in nans {
        let canonical = canonical_f64(nan);
        assert!(canonical.is_nan(), "Should still be NaN");
        assert_eq!(
            canonical.to_bits(),
            CANONICAL_NAN_F64,
            "NaN variant {:016x} should canonicalize to {:016x}",
            nan.to_bits(),
            CANONICAL_NAN_F64
        );
    }
}

#[test]
fn test_canonical_f64_zero() {
    assert!(canonical_f64(-0.0).is_sign_positive());
    assert_eq!(canonical_f64(-0.0), 0.0);
    assert_eq!(canonical_f64(-0.0).to_bits(), 0u64);
}

#[test]
fn test_canonical_f64_preserves_normal() {
    let values = [1.0, -1.0, 0.5, f64::MAX, f64::MIN_POSITIVE, f64::EPSILON];
    for v in values {
        assert_eq!(canonical_f64(v), v);
        assert_eq!(canonical_f64(v).to_bits(), v.to_bits());
    }
}

#[test]
fn test_canonical_f32_nan_variants() {
    let nans = [
        f32::NAN,
        -f32::NAN,
        f32::from_bits(0x7f80_0001), // signaling NaN
        f32::from_bits(0x7fc0_0001), // quiet NaN with payload
    ];

    for nan in nans {
        let canonical = canonical_f32(nan);
        assert!(canonical.is_nan());
        assert_eq!(canonical.to_bits(), CANONICAL_NAN_F32);
    }
}

#[test]
fn test_canonical_f32_zero() {
    assert!(canonical_f32(-0.0_f32).is_sign_positive());
    assert_eq!(canonical_f32(-0.0_f32), 0.0_f32);
}

#[test]
fn test_canonical_idempotent() {
    let values = [0.0, -0.0, 1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
    for v in values {
        let once = canonical_f64(v);
        let twice = canonical_f64(once);
        assert_eq!(once.to_bits(), twice.to_bits());
    }
}

// ------------------------------------------------------------------------
// Comparison Function Tests
// ------------------------------------------------------------------------

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

    assert_eq!(
        cmp_desc_score_then_id(f64::NAN, id_a, 0.5, id_b),
        Ordering::Greater,
        "NaN should sort after normal values in descending"
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

    assert_eq!(sorted1, sorted2, "Multiple sorts should produce same order");

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

// ------------------------------------------------------------------------
// ScoredEntry Tests
// ------------------------------------------------------------------------

#[derive(Debug, Clone)]
// REASON: TestCandidate.value is only used to construct test data; the struct
// exists to implement HasId for heap ordering tests, not to read `.value`.
#[allow(dead_code)]
struct TestCandidate {
    id: Uuid,
    value: i32,
}

impl HasId for TestCandidate {
    fn id(&self) -> Uuid {
        self.id
    }
}

#[test]
fn test_scored_entry_ord() {
    let a = TestCandidate {
        id: Uuid::from_u128(1),
        value: 10,
    };
    let b = TestCandidate {
        id: Uuid::from_u128(2),
        value: 20,
    };

    let entry_a = ScoredEntry::new(&a, 0.9, 0);
    let entry_b = ScoredEntry::new(&b, 0.5, 1);

    assert!(entry_a > entry_b);
}

#[test]
fn test_scored_entry_heap() {
    use std::collections::BinaryHeap;

    let candidates: Vec<TestCandidate> = (0..10i32)
        .map(|i| TestCandidate {
            id: Uuid::from_u128(i as u128),
            value: i * 10,
        })
        .collect();

    let mut heap: BinaryHeap<ScoredEntry<&TestCandidate>> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| ScoredEntry::new(c, 0.5, i))
        .collect();

    let mut last_id = Uuid::nil();
    while let Some(entry) = heap.pop() {
        if last_id != Uuid::nil() {
            assert!(entry.id() > last_id, "Should pop in UUID order");
        }
        last_id = entry.id();
    }
}

#[test]
fn test_scored_entry_equality() {
    let a = TestCandidate {
        id: Uuid::from_u128(1),
        value: 10,
    };
    let b = TestCandidate {
        id: Uuid::from_u128(1),
        value: 20,
    };

    let entry_a = ScoredEntry::new(&a, 0.5, 0);
    let entry_b = ScoredEntry::new(&b, 0.5, 1);

    assert_eq!(entry_a, entry_b);
}

#[test]
fn test_scored_entry_hash() {
    use std::collections::HashSet;

    let a = TestCandidate {
        id: Uuid::from_u128(1),
        value: 10,
    };

    let entry1 = ScoredEntry::new(&a, 0.5, 0);
    let entry2 = ScoredEntry::new(&a, 0.5, 1);

    let mut set = HashSet::new();
    set.insert(entry1);
    assert!(set.contains(&entry2));
}

// ------------------------------------------------------------------------
// DeterministicScore Integration Tests
// ------------------------------------------------------------------------

#[test]
fn test_deterministic_score_roundtrip() {
    let s = DeterministicScore::from_f64(0.75);
    assert!((s.to_f64() - 0.75).abs() < 1e-9);
}

#[test]
fn test_deterministic_score_nan_maps_to_zero() {
    let s = DeterministicScore::from_f64(f64::NAN);
    assert_eq!(s, DeterministicScore::ZERO);
}

#[test]
fn test_ranked_heap_with_uuid_ids() {
    use std::collections::BinaryHeap;

    let mut heap: BinaryHeap<Ranked<Uuid>> = BinaryHeap::new();
    heap.push(Ranked::new(
        DeterministicScore::from_f64(0.9),
        Uuid::from_u128(3),
    ));
    heap.push(Ranked::new(
        DeterministicScore::from_f64(0.9),
        Uuid::from_u128(1),
    ));
    heap.push(Ranked::new(
        DeterministicScore::from_f64(0.9),
        Uuid::from_u128(2),
    ));
    heap.push(Ranked::new(
        DeterministicScore::from_f64(0.5),
        Uuid::from_u128(4),
    ));

    // All 0.9 scores — lower UUID pops first
    let first = heap.pop().unwrap();
    assert_eq!(*first.id(), Uuid::from_u128(1));
    let second = heap.pop().unwrap();
    assert_eq!(*second.id(), Uuid::from_u128(2));
    let third = heap.pop().unwrap();
    assert_eq!(*third.id(), Uuid::from_u128(3));
    // Then lower score
    let fourth = heap.pop().unwrap();
    assert_eq!(*fourth.id(), Uuid::from_u128(4));
}
