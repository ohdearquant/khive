// ── JSON NaN / Infinity / overflow tests ────────────────────────────────────
// JSON does not have a NaN or Infinity token. serde_json rejects these at the parser level
// (before TryFrom is reached). 1e400 overflows f64 to infinity — also rejected.

/// Edge serde must reject JSON NaN literal in weight.
#[test]
fn edge_serde_rejects_json_nan_weight() {
    let id = uuid::Uuid::nil();
    let src = uuid::Uuid::nil();
    let tgt = uuid::Uuid::nil();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":NaN,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "JSON NaN weight must be rejected by the parser"
    );
}

/// Edge serde must reject JSON Infinity literal in weight.
#[test]
fn edge_serde_rejects_json_infinity_weight() {
    let id = uuid::Uuid::nil();
    let src = uuid::Uuid::nil();
    let tgt = uuid::Uuid::nil();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":Infinity,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "JSON Infinity weight must be rejected by the parser"
    );
}

/// Edge serde must reject 1e400 (overflows to f64::INFINITY) via TryFrom.
#[test]
fn edge_serde_rejects_overflow_to_infinity_weight() {
    let id = uuid::Uuid::nil();
    let src = uuid::Uuid::nil();
    let tgt = uuid::Uuid::nil();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":1e400,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "1e400 weight overflowing to infinity must be rejected"
    );
}
