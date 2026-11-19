//! Validation for the memory session-visibility fence at the recall boundary.

use std::collections::HashSet;
use std::fmt;

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_types::{Details, KhiveError};
use serde_json::Value;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ModelFence {
    pub(crate) model: String,
    pub(crate) ann_write_log_seq: u64,
}

impl fmt::Debug for ModelFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelFence([REDACTED])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct VisibilityFence {
    pub(crate) namespace: String,
    pub(crate) fences: Vec<ModelFence>,
}

impl fmt::Debug for VisibilityFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VisibilityFence([REDACTED])")
    }
}

impl VisibilityFence {
    pub(crate) fn seq_for_model(&self, model: &str) -> Option<u64> {
        self.fences
            .iter()
            .find(|fence| fence.model == model)
            .map(|fence| fence.ann_write_log_seq)
    }

    pub(crate) fn proof_for_model(&self, model: &str) -> Option<(u64, String)> {
        self.seq_for_model(model)
            .map(|seq| (seq, self.namespace.clone()))
    }
}

fn invalid(message: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("memory.recall {message}"))
}

/// Eventual reads ignore an optional token. Session fences are copied only
/// after runtime custody authenticates scope, requested models and token age.
pub(crate) fn parse_recall_visibility(
    runtime: &KhiveRuntime,
    consistency: Option<&Value>,
    visibility_token: Option<&Value>,
    visible_namespaces: &[&str],
    requested_models: &[String],
) -> Result<Option<VisibilityFence>, RuntimeError> {
    match consistency {
        None => return Ok(None),
        Some(Value::String(value)) if value == "eventual" => return Ok(None),
        Some(Value::String(value)) if value == "session" => {}
        _ => return Err(invalid("consistency must be 'eventual' or 'session'")),
    }

    if visibility_token
        .and_then(Value::as_object)
        .and_then(|object| object.get("version"))
        .and_then(Value::as_u64)
        == Some(1)
    {
        return Err(KhiveError::invalid_input(
            "memory.recall clear visibility receipts are not accepted",
        )
        .with_details(Details::new([("reason", "visibility_token_legacy")]))
        .into());
    }
    let Some(token) = visibility_token.and_then(Value::as_str) else {
        // A write made without receipt custody returns a null token. Handing
        // that null back must name the retryable custody refusal, not a
        // malformed token; only with custody in place is the shape at fault.
        runtime.ensure_visibility_receipt_key()?;
        return Err(invalid(
            "consistency=session requires an opaque visibility_token string",
        ));
    };
    let receipt = runtime.open_visibility_receipt(token, visible_namespaces, requested_models)?;
    let mut seen = HashSet::new();
    let mut fences = Vec::new();
    for model in requested_models {
        if seen.insert(model.as_str()) {
            if let Some(ann_write_log_seq) = receipt.sequence_for_model(model) {
                fences.push(ModelFence {
                    model: model.clone(),
                    ann_write_log_seq,
                });
            }
        }
    }
    Ok(Some(VisibilityFence {
        namespace: receipt.namespace().to_owned(),
        fences,
    }))
}

/// Validate the caller's wait budget; the caller clips it further to the
/// remaining request deadline minus its two-second margin.
pub(crate) fn parse_timeout_ms(raw: Option<&Value>) -> Result<u64, RuntimeError> {
    match raw {
        None => Ok(0),
        Some(value) => match value.as_u64() {
            Some(ms) if ms <= 10_000 => Ok(ms),
            _ => Err(invalid(
                "timeout_ms must be an integer from 0 through 10000",
            )),
        },
    }
}

#[cfg(test)]
#[path = "visibility_tests.rs"]
mod tests;
