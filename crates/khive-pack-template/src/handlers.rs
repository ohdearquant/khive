//! Verb handler stubs for the template pack (ADR-023 §8).
//!
//! Replace each `unimplemented!()` with real logic. See `crates/khive-pack-kg/src/handlers.rs`
//! for a complete reference implementation.
//!
//! Handler signature pattern:
//!   `async fn handle_<verb>(runtime, token, params) -> Result<Value, RuntimeError>`
//!
//! Params arrive as `serde_json::Value`; deserialize via `serde_json::from_value`.
//! Return a JSON `Value` or a `RuntimeError`. Errors are caught by the registry and
//! returned as `{ ok: false, error: "..." }` without aborting the batch.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

/// `my_verb` — replace with real logic.
pub(crate) async fn handle_my_verb(
    _runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    // TODO: implement
    Ok(json!({ "ok": true, "params": params }))
}
