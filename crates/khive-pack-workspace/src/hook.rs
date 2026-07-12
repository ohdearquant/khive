//! `WorkspaceHook`  -  validates the `workspace` entity kind's required
//! `properties.schema_version` field on create (SPEC-gate ruling 4).
//!
//! `name` is already required and validated by the generic entity-create
//! path (`khive-pack-kg::handlers::create`); this hook only adds the
//! workspace-specific `schema_version` requirement. `filesystem_path` stays
//! an optional, unvalidated property (ruling 4/6: it is a locator, not
//! identity, and may go stale without becoming an error).

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};

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
