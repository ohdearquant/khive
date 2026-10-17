use super::*;

#[test]
fn precision_default_is_one() {
    let sel = Selection::new(42i32, 0.8, 0);
    assert_eq!(sel.precision, 1.0);
}

#[test]
fn with_precision_sets_field() {
    let sel = Selection::new(42i32, 0.8, 0).with_precision(0.5);
    assert_eq!(sel.precision, 0.5);
}

#[test]
fn map_propagates_precision() {
    let sel = Selection::new(42i32, 0.8, 0).with_precision(0.75);
    let mapped = sel.map(|v| v.to_string());
    assert_eq!(mapped.precision, 0.75);
    assert_eq!(mapped.item, "42");
    assert_eq!(mapped.score, 0.8);
}

#[test]
fn map_preserves_all_stats() {
    let sel = Selection::new(1i32, 0.5, 2)
        .with_precision(0.6)
        .with_considered(10)
        .with_passed(7)
        .with_reason("test");
    let mapped = sel.map(|v| v * 2);
    assert_eq!(mapped.item, 2);
    assert_eq!(mapped.score, 0.5);
    assert_eq!(mapped.precision, 0.6);
    assert_eq!(mapped.index, 2);
    assert_eq!(mapped.considered, 10);
    assert_eq!(mapped.passed, 7);
    assert_eq!(mapped.reason.as_deref(), Some("test"));
}
