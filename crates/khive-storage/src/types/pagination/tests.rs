use super::*;

/// STORAGE-AUD-003 / #485: offset > i64::MAX must be rejected by serde
/// deserialization instead of silently narrowing to a negative i64 at the
/// SQLite boundary.
#[test]
fn page_offset_over_i64max_rejected() {
    let raw = serde_json::json!({
        "offset": (i64::MAX as u64) + 1,
        "limit": 50,
    });
    let result: Result<PageRequest, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "offset > i64::MAX must be rejected, got {result:?}"
    );
}

#[test]
fn page_offset_at_i64max_accepted() {
    let raw = serde_json::json!({
        "offset": i64::MAX as u64,
        "limit": 50,
    });
    let result: Result<PageRequest, _> = serde_json::from_value(raw);
    assert!(result.is_ok(), "offset == i64::MAX must be accepted");
}

#[test]
fn limit_report_has_one_shared_wire_shape() {
    assert_eq!(
        LimitReport::new(201, 200).value(),
        serde_json::json!({
            "requested_limit": 201,
            "effective_limit": 200,
            "limit_clamped": true,
        })
    );
    assert_eq!(
        LimitReport::new(2, 2).value(),
        serde_json::json!({
            "requested_limit": 2,
            "effective_limit": 2,
            "limit_clamped": false,
        })
    );
}
