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
