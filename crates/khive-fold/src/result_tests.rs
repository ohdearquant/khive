use super::*;

#[test]
fn test_fold_outcome_creation() {
    let result = FoldOutcome::new(42, 10);
    assert_eq!(result.state, 42);
    assert_eq!(result.entries_processed, 10);
}

#[test]
fn test_fold_outcome_map() {
    let result = FoldOutcome::new(42, 10);
    let mapped = result.map(|x| x.to_string());
    assert_eq!(mapped.state, "42");
    assert_eq!(mapped.entries_processed, 10);
}

#[test]
fn deterministic_no_timing_fields() {
    let a = FoldOutcome::new(7usize, 3);
    let b = FoldOutcome::new(7usize, 3);
    assert_eq!(a, b);
}

#[test]
fn default_is_zero_state_zero_count() {
    let d = FoldOutcome::<usize>::default();
    assert_eq!(d.state, 0);
    assert_eq!(d.entries_processed, 0);
}
