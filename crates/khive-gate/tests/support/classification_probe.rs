//! Actual classifier calls under the isolated allocation observer.
use std::hint::black_box;

use khive_gate::{classify_operation, OperationAccess};

use crate::allocation_counter;

pub const FIXED_CALLS: usize = 1_000_000;

pub fn allocation_count(verb: &str, expected: OperationAccess, calls: usize) -> usize {
    let (all_match, allocations) = allocation_counter::measure(|| {
        let mut all_match = true;
        for _ in 0..calls {
            all_match &= black_box(classify_operation(black_box(verb))) == Some(expected);
        }
        black_box(all_match)
    });
    assert!(all_match, "classifier result changed for {verb}");
    assert_eq!(allocations, 0, "prepared classifier allocated for {verb}");
    allocations
}
