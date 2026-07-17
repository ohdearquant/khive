//! Workspace-specific `properties.schema_version` validation for generic creation.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};

/// Generic-create hook requiring an integer workspace schema version.
///
/// See `crates/khive-pack-workspace/docs/api/workspace-registration.md`.
#[derive(Debug, Default)]
pub struct WorkspaceHook;

#[async_trait]
impl KindHook for WorkspaceHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let has_schema_version = args
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|props| props.get("schema_version"))
            .is_some_and(|v| v.is_i64() || v.is_u64());
        if !has_schema_version {
            return Err(RuntimeError::InvalidInput(
                "workspace entity requires properties.schema_version (integer)".into(),
            ));
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
