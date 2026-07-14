//! `KindHook` implementations for the three note kinds this pack contributes.
//! Validation only — provenance edges are supplied by the caller and linked
//! by the runtime's `create_note` path itself. See
//! crates/khive-pack-git/docs/hooks.md for why no `after_create` edge work
//! is needed here.

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

/// The governed `state_reason` value set for `issue` (ADR-088 §3). See
/// crates/khive-pack-git/docs/hooks.md#issuelikehook for why this is
/// `pub(crate)`.
pub(crate) const ISSUE_STATE_REASONS: &[&str] =
    &["completed", "not_planned", "reopened", "duplicate"];

/// `KindHook` shared by `issue` and `pull_request` — both require
/// `properties.number` (integer) and `properties.project_id` (UUID), and,
/// when present, validate `properties.state_reason` (governed to a fixed
/// set for `issue` per ADR-088 §3; only checked for non-emptiness for
/// `pull_request`). See crates/khive-pack-git/docs/hooks.md#issuelikehook
/// for why `project_id` is required here rather than left to caller
/// discipline.
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

        let project_id = props
            .get("project_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!("{} requires properties.project_id", self.kind))
            })?;
        if Uuid::parse_str(project_id).is_err() {
            return Err(RuntimeError::InvalidInput(format!(
                "{} properties.project_id {project_id:?} must be a UUID",
                self.kind
            )));
        }

        if let Some(reason) = props.get("state_reason").and_then(Value::as_str) {
            // The raw value is never interpolated into this error: it is
            // caller-controlled (for `issue`, sourced from GitHub) and may be
            // credential-shaped. Only the static governed set is echoed.
            if self.kind == "issue" && !ISSUE_STATE_REASONS.contains(&reason) {
                return Err(RuntimeError::InvalidInput(format!(
                    "issue properties.state_reason is not one of the governed values — valid: {}",
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
