//! Verb handlers for the template pack.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

/// `template.my_verb` — example verb demonstrating parameter validation.
///
/// Accepts `{ "name": "<string>" }` and returns `{ "ok": true, "name": "<string>" }`.
/// Returns an error when `name` is absent or not a non-empty string.
/// See `crates/khive-pack-template/docs/api/pack-scaffold.md`.
pub(crate) async fn handle_my_verb(
    _runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "template.my_verb requires a non-empty string field \"name\"".to_string(),
            )
        })?;

    Ok(json!({ "ok": true, "name": name }))
}
