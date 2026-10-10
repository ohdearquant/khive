use super::ScanWindows;

#[test]
fn windows_preserve_the_initial_value_and_only_continue_when_requested() {
    let mut windows = ScanWindows::new(0, 10_000, 0);
    assert_eq!(windows.next(), Some(0));
    assert_eq!(windows.next(), None);
    windows.grow(0, true);
    assert_eq!(windows.next(), Some(0));
}

#[test]
fn windows_preserve_adaptive_demand_withheld_growth_and_the_cap() {
    let mut windows = ScanWindows::new(7, 20, 1);
    assert_eq!(windows.next(), Some(7));
    windows.grow(2, false);
    assert_eq!(windows.next(), Some(4));
    windows.grow(1, true);
    assert_eq!(windows.next(), Some(8));
    windows.grow(9, true);
    assert_eq!(windows.next(), Some(18));
    windows.grow(u32::MAX, true);
    assert_eq!(windows.next(), Some(20));
    windows.grow(0, true);
    assert_eq!(windows.next(), Some(20));
}
