//! Ordinary deterministic gate; no latency threshold is asserted.
use std::hint::black_box;

use khive_gate::CLASSIFIED_OPERATIONS;

#[path = "support/allocation_counter.rs"]
mod allocation_counter;
#[path = "support/classification_probe.rs"]
mod classification_probe;

#[test]
fn all_classifier_verbs_do_not_allocate() {
    assert!(!CLASSIFIED_OPERATIONS.is_empty());
    for &(verb, expected) in CLASSIFIED_OPERATIONS {
        classification_probe::allocation_count(verb, expected, classification_probe::FIXED_CALLS);
    }
}

#[test]
fn allocation_observer_detects_heap_growth() {
    let (allocation, observed) =
        allocation_counter::measure(|| black_box(Box::new(black_box([42_u8; 64]))));
    assert!(observed > 0, "observer must detect a real heap allocation");
    assert_eq!(allocation[0], 42);
    drop(allocation);
}
