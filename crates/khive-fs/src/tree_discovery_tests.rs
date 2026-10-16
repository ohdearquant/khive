use super::remaining_entry_budget;

#[test]
fn finite_entry_caps_keep_zero_and_last_representable_budget() {
    assert_eq!(remaining_entry_budget(0, 0), Some(0));
    assert_eq!(remaining_entry_budget(7, 3), Some(4));
    assert_eq!(remaining_entry_budget(7, 7), Some(0));
    assert_eq!(
        remaining_entry_budget(usize::MAX - 1, usize::MAX - 2),
        Some(1)
    );
    assert_eq!(
        remaining_entry_budget(usize::MAX - 1, usize::MAX - 1),
        Some(0)
    );
}

#[test]
fn maximum_entry_cap_preserves_saturating_progress_without_local_refusal() {
    for visited in [0, 1, usize::MAX - 1, usize::MAX] {
        assert_eq!(remaining_entry_budget(usize::MAX, visited), None);
    }
}
