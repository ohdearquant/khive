use super::{GateError, GateValidationError};

#[test]
fn wire_reason_never_echoes_backend_display_text() {
    let canary = "postgres://svc:not-a-real-secret@internal-host";
    let error = GateError::Internal(canary.to_string());

    assert!(!error.wire_reason().contains(canary));
    assert_eq!(error.wire_reason(), "gate backend unavailable");
}

#[test]
fn policy_and_internal_classify_into_distinguishable_stable_text() {
    let internal = GateError::Internal("connection refused".to_string());
    let policy = GateError::Policy("rule set has no allow clause".to_string());
    let validation = GateError::Validation(GateValidationError::EmptyVerb);

    assert_eq!(internal.wire_reason(), "gate backend unavailable");
    assert_eq!(policy.wire_reason(), "gate policy evaluation failed");
    assert_eq!(validation.wire_reason(), "gate request validation failed");
    assert_ne!(internal.wire_reason(), policy.wire_reason());
    assert_ne!(policy.wire_reason(), validation.wire_reason());
}
