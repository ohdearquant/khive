use super::*;

#[test]
fn traversal_options_default_max_depth_is_three() {
    assert_eq!(TraversalOptions::default().max_depth, 3);
    assert_eq!(
        TraversalOptions::default().effective_limit(),
        DEFAULT_TRAVERSAL_LIMIT
    );
}

#[test]
fn traverse_max_depth_over_public_cap_rejected() {
    let raw = serde_json::json!({
        "max_depth": MAX_TRAVERSAL_DEPTH + 1,
        "direction": "out",
        "relations": null,
        "min_weight": null,
        "limit": null,
    });
    let result: Result<TraversalOptions, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "max_depth above the public cap must be rejected, got {result:?}"
    );
}

#[test]
fn traverse_depth_and_limit_boundaries_are_inclusive() {
    let raw = serde_json::json!({
        "max_depth": MAX_TRAVERSAL_DEPTH,
        "direction": "out",
        "relations": null,
        "min_weight": null,
        "limit": MAX_TRAVERSAL_LIMIT,
    });
    let result: Result<TraversalOptions, _> = serde_json::from_value(raw);
    assert!(result.is_ok(), "inclusive public maxima must be accepted");
}

#[test]
fn traverse_limit_over_public_cap_rejected() {
    let raw = serde_json::json!({
        "max_depth": 1,
        "direction": "out",
        "relations": null,
        "min_weight": null,
        "limit": MAX_TRAVERSAL_LIMIT + 1,
    });
    let result: Result<TraversalOptions, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "limit above the public cap must be rejected"
    );
}

#[test]
fn traverse_root_cap_rejected_during_deserialization() {
    let raw = serde_json::json!({
        "roots": vec![Uuid::nil(); MAX_TRAVERSAL_ROOTS + 1],
        "options": {
            "max_depth": 1,
            "direction": "out",
            "relations": null,
            "min_weight": null,
            "limit": 1,
        },
        "include_roots": false,
    });
    let result: Result<TraversalRequest, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "root list above the public cap must be rejected"
    );
}

#[test]
fn traversal_execution_budget_is_shared_but_not_serialized() {
    let budget = TraversalExecutionBudget::new(2, Duration::from_secs(1));
    let clone = budget.clone();
    assert!(budget.try_consume_row());
    assert_eq!(clone.remaining_work(), 1);

    let request = TraversalRequest {
        roots: vec![Uuid::nil()],
        options: TraversalOptions::default(),
        include_roots: true,
        include_properties: false,
        execution_budget: budget,
    };
    let value = serde_json::to_value(request).unwrap();
    assert!(value.get("execution_budget").is_none());
}
