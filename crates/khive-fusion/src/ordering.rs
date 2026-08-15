//! Canonical tie-break comparator shared by every fusion strategy.

use khive_score::DeterministicScore;
use std::cmp::Ordering;

/// Score-descending, then ID-ascending. RRF, Weighted, and Union all sort
/// their output with this comparator; callers dispatching a runtime-registered
/// custom strategy (`FusionStrategy::Custom`) apply it too, so a registered
/// executor cannot bypass the crate's ranking invariant by returning results
/// in an arbitrary order.
pub fn cmp_desc_then_id<Id: Ord>(
    a: &(Id, DeterministicScore),
    b: &(Id, DeterministicScore),
) -> Ordering {
    match b.1.cmp(&a.1) {
        Ordering::Equal => a.0.cmp(&b.0),
        other => other,
    }
}
