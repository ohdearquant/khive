use super::*;

#[test]
fn test_entry_not_found_display() {
    let id = Uuid::new_v4();
    let err = FoldError::EntryNotFound(id);
    assert!(err.to_string().contains(&id.to_string()));
}

#[test]
fn test_invalid_entry_type_display() {
    let err = FoldError::InvalidEntryType {
        expected: "data".into(),
        actual: "item".into(),
    };
    let msg = err.to_string();
    assert!(msg.contains("data"));
    assert!(msg.contains("item"));
}

#[test]
fn test_context_display() {
    let err = FoldError::Context("missing budget".into());
    assert!(err.to_string().contains("missing budget"));
}

#[test]
fn test_from_serde_error() {
    let json_err: serde_json::Error = serde_json::from_str::<i32>("invalid").unwrap_err();
    let err: FoldError = json_err.into();
    assert!(matches!(err, FoldError::Serialization(_)));
}

#[test]
fn test_budget_exhausted_display() {
    let err = FoldError::BudgetExhausted {
        needed: 100,
        budget: 50,
    };
    let msg = err.to_string();
    assert!(msg.contains("100"));
    assert!(msg.contains("50"));
}

#[test]
fn test_anchor_not_found_display() {
    let err = FoldError::AnchorNotFound("my-anchor".into());
    assert!(err.to_string().contains("my-anchor"));
}
