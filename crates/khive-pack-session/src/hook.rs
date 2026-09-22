//! Validate session metadata on shared creation without rewriting caller data.

use async_trait::async_trait;
use khive_runtime::{effective_create_tags, KhiveRuntime, KindHook, RuntimeError};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::handlers::{deser, StoreParams};

#[derive(Debug, Default)]
pub(crate) struct SessionKindHook;

#[async_trait]
impl KindHook for SessionKindHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let properties = args
            .get("properties")
            .filter(|value| !value.is_null())
            .map(|value| {
                value.as_object().ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "session: properties must be an object when provided".into(),
                    )
                })
            })
            .transpose()?;
        let property = |key: &str| properties.and_then(|values| values.get(key));

        // Validate the writers' shared effective value without changing the
        // original metadata or rejecting a value the caller replaces.
        let top_tags: Option<Vec<String>> =
            deser(args.get("tags").cloned().unwrap_or(Value::Null))?;
        let tags = effective_create_tags(top_tags.as_deref(), args.get("properties")).to_value();
        let fields: StoreParams = deser(json!({
            "content": args.get("content"),
            "title": args.get("name"),
            "provider": property("provider"),
            "provider_session_id": property("provider_session_id"),
            "tags": tags,
        }))?;
        fields.validate("session")
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
