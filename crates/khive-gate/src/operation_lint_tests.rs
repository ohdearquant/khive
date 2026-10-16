use super::*;

#[test]
fn graph_lint_is_explicitly_write_for_its_strongest_requested_effect() {
    assert_eq!(
        classify_operation("knowledge.lint"),
        Some(OperationAccess::Write)
    );
    for unknown in [
        "knowledge.lint_config",
        "knowledge.lint.stats",
        "knowledge.lint ",
    ] {
        assert_eq!(classify_operation(unknown), None);
    }
    assert!(CLASSIFIED_OPERATIONS
        .windows(2)
        .all(|pair| pair[0].0 < pair[1].0));
}
