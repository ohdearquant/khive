//! Per-operation audit context carried across the MCP/runtime dispatch seam.
//!
//! The transport owns syntax-level facts such as an operation's request-group position and
//! whether an argument contains `$prev`. Pack handlers own the final canonical argument shape.
//! Tokio task-local scopes let both contribute to one audit row without widening
//! [`crate::RequestIdentity`] or leaking mutable request state through the warm registry.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;

use khive_gate::{ArgumentOrigin, AuditArgumentIdentity};
use serde_json::Value;

const MAX_AUDIT_ARGUMENT_KEYS: usize = 64;
const MAX_AUDIT_ARGUMENT_KEY_CHARS: usize = 128;

/// Syntax-level provenance for one operation in a request group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationAuditContext {
    /// Zero-based position in the parsed batch or chain.
    pub index: u32,
    /// Origin of every top-level parsed argument.
    pub argument_origins: BTreeMap<String, ArgumentOrigin>,
}

tokio::task_local! {
    static OPERATION_CONTEXT: RefCell<Option<OperationAuditContext>>;
    static EFFECTIVE_ARGUMENTS: RefCell<Value>;
}

/// Run one operation with its syntax-level audit provenance attached.
pub async fn scope_operation<F>(context: OperationAuditContext, future: F) -> F::Output
where
    F: Future,
{
    OPERATION_CONTEXT
        .scope(RefCell::new(Some(context)), future)
        .await
}

pub(crate) fn current_operation() -> Option<OperationAuditContext> {
    OPERATION_CONTEXT
        .try_with(|context| context.borrow_mut().take())
        .ok()
        .flatten()
}

/// Capture the final canonical arguments recorded by a handler during `future`.
///
/// `fallback` is the envelope initially handed to the pack/coordinator. Handlers that perform
/// additional canonicalization replace it with [`record_effective_arguments`].
pub(crate) async fn capture_effective_arguments<F>(fallback: Value, future: F) -> (F::Output, Value)
where
    F: Future,
{
    EFFECTIVE_ARGUMENTS
        .scope(RefCell::new(fallback), async move {
            let output = future.await;
            let effective = EFFECTIVE_ARGUMENTS.with(|args| args.borrow().clone());
            (output, effective)
        })
        .await
}

/// Replace the current operation's effective-argument envelope after canonicalization.
///
/// Calls outside a registry dispatch scope are harmless no-ops. This makes the hook usable by
/// shared validators that also serve direct/unit-test entry points.
pub fn record_effective_arguments(arguments: &Value) {
    let _ = EFFECTIVE_ARGUMENTS.try_with(|current| {
        *current.borrow_mut() = arguments.clone();
    });
}

/// Build a bounded, non-reversible argument identity suitable for durable audit storage.
pub(crate) fn argument_identity(arguments: &Value) -> AuditArgumentIdentity {
    let canonical = canonicalize(arguments);
    let encoded = serde_json::to_vec(&canonical).unwrap_or_default();
    let masked = crate::secret_gate::mask_secrets(
        std::str::from_utf8(&encoded).unwrap_or("<invalid-json-encoding>"),
    );
    let digest = format!(
        "blake3:{}",
        khive_types::Hash32::from_blake3(masked.as_bytes())
    );

    let mut keys = match arguments {
        Value::Object(map) => map
            .keys()
            .map(|key| {
                crate::secret_gate::mask_secrets(key)
                    .chars()
                    .take(MAX_AUDIT_ARGUMENT_KEY_CHARS)
                    .collect::<String>()
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    keys.sort_unstable();
    keys.dedup();
    let keys_truncated = keys.len() > MAX_AUDIT_ARGUMENT_KEYS;
    keys.truncate(MAX_AUDIT_ARGUMENT_KEYS);

    AuditArgumentIdentity {
        digest,
        keys,
        keys_truncated,
    }
}

/// Recursively sort object keys before hashing so semantically identical JSON objects have the
/// same identity regardless of construction/insertion order.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let sorted = map
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_value(sorted).unwrap_or(Value::Null)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_identity_is_order_independent_and_never_contains_values() {
        let left = serde_json::json!({"b": 2, "a": "private-value"});
        let right = serde_json::json!({"a": "private-value", "b": 2});

        let left_identity = argument_identity(&left);
        let right_identity = argument_identity(&right);

        assert_eq!(left_identity, right_identity);
        assert_eq!(left_identity.keys, vec!["a", "b"]);
        assert!(!left_identity.digest.contains("private-value"));
    }
}
