//! `KindHook` implementations for the three note kinds this pack contributes.
//!
//! Validation only — this pack introduces no new edges at `after_create` time.
//! Provenance edges (`annotates` -> project / document / merging PR) are
//! supplied by the caller (the ingester, see `src/ingest.rs`) as part of the
//! generic `create(kind=..., annotates=[...])` call; the runtime's own
//! `create_note` path validates and links them atomically, so no
//! `after_create` edge-creation logic is needed here (unlike gtd's
//! `TaskHook::after_create`).

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};

/// A 40-character lowercase-hex string, the shape of a full git commit SHA-1.
fn is_40_hex(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn properties_obj(args: &Value) -> Result<&serde_json::Map<String, Value>, RuntimeError> {
    args.get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "kind=commit|issue|pull_request requires a `properties` object".into(),
            )
        })
}

/// `KindHook` for the immutable `commit` note kind.
///
/// Validates `properties.sha` (required, 40-hex) and, when present,
/// `properties.parents` (array of 40-hex strings). Commits have no lifecycle
/// and no `after_create` edge work.
#[derive(Debug, Default)]
pub struct CommitHook;

#[async_trait]
impl KindHook for CommitHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let props = properties_obj(args)?;

        let sha = props
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("commit requires properties.sha".into()))?;
        if !is_40_hex(sha) {
            return Err(RuntimeError::InvalidInput(format!(
                "commit properties.sha {sha:?} must be a 40-character hex string"
            )));
        }

        if let Some(parents) = props.get("parents") {
            let arr = parents.as_array().ok_or_else(|| {
                RuntimeError::InvalidInput("commit properties.parents must be an array".into())
            })?;
            for (idx, p) in arr.iter().enumerate() {
                let s = p.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "commit properties.parents[{idx}] must be a string"
                    ))
                })?;
                if !is_40_hex(s) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "commit properties.parents[{idx}] {s:?} must be a 40-character hex string"
                    )));
                }
            }
        }

        if let Some(short) = props.get("short_sha").and_then(Value::as_str) {
            if short.is_empty() || !sha.starts_with(short) {
                return Err(RuntimeError::InvalidInput(format!(
                    "commit properties.short_sha {short:?} must be a non-empty prefix of sha {sha:?}"
                )));
            }
        }

        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// The governed `state_reason` value set for `issue` (ADR-088 §3).
const ISSUE_STATE_REASONS: &[&str] = &["completed", "not_planned", "reopened", "duplicate"];

/// `KindHook` shared by `issue` and `pull_request` — both require
/// `properties.number` and, when present, validate `properties.state_reason`.
/// `issue`'s `state_reason` is governed to a fixed set (ADR-088 §3); v0 does
/// not document a fixed set for `pull_request`'s `state_reason`, so it is only
/// checked for non-emptiness there.
#[derive(Debug)]
pub struct IssueLikeHook {
    /// The note kind this instance validates: `"issue"` or `"pull_request"`.
    pub kind: &'static str,
}

#[async_trait]
impl KindHook for IssueLikeHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let props = properties_obj(args)?;

        let number = props.get("number").ok_or_else(|| {
            RuntimeError::InvalidInput(format!("{} requires properties.number", self.kind))
        })?;
        if !number.is_u64() && !number.is_i64() {
            return Err(RuntimeError::InvalidInput(format!(
                "{} properties.number must be an integer",
                self.kind
            )));
        }

        if let Some(reason) = props.get("state_reason").and_then(Value::as_str) {
            if self.kind == "issue" && !ISSUE_STATE_REASONS.contains(&reason) {
                return Err(RuntimeError::InvalidInput(format!(
                    "issue properties.state_reason {reason:?} invalid — valid: {}",
                    ISSUE_STATE_REASONS.join(", ")
                )));
            }
            if reason.trim().is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{} properties.state_reason must not be empty when present",
                    self.kind
                )));
            }
        }

        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}
