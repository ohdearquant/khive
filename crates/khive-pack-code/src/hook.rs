//! `FindingHook` — validates and defaults the `finding` note kind on the
//! shared `create(kind="finding", ...)` path.

use async_trait::async_trait;
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};

use crate::vocab::{is_valid_confidence, is_valid_finding_status, is_valid_severity};

#[derive(Debug, Default)]
pub(crate) struct FindingHook;

#[async_trait]
impl KindHook for FindingHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let title = args
            .get("title")
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "kind=note + note_kind=finding requires 'title' or 'name'".into(),
                )
            })?;
        if title.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("title must not be empty".into()));
        }

        let mut props = match args.get("properties") {
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(Value::Null) | None => json!({}),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(
                    "properties must be an object".into(),
                ))
            }
        };
        let obj = props
            .as_object_mut()
            .expect("props is object by construction");

        for key in [
            "severity",
            "confidence",
            "categories",
            "source_run",
            "standard",
            "evidence",
            "refs",
        ] {
            if let Some(v) = args.get(key) {
                obj.insert(key.to_string(), v.clone());
            }
        }

        if let Some(v) = args.get("kind_status") {
            obj.insert("kind_status".into(), v.clone());
        } else if !obj.contains_key("kind_status") {
            obj.insert("kind_status".into(), json!("open"));
        }

        let kind_status = obj
            .get("kind_status")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RuntimeError::InvalidInput("kind_status must be a string".into()))?;
        if !is_valid_finding_status(&kind_status) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid kind_status {kind_status:?}; valid: open, resolved, wontfix, invalid"
            )));
        }

        if let Some(v) = obj.get("severity") {
            let s = v
                .as_str()
                .ok_or_else(|| RuntimeError::InvalidInput("severity must be a string".into()))?;
            if !is_valid_severity(s) {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid severity {s:?}; valid: critical, high, medium, low, info"
                )));
            }
        }

        if let Some(v) = obj.get("confidence") {
            let s = v
                .as_str()
                .ok_or_else(|| RuntimeError::InvalidInput("confidence must be a string".into()))?;
            if !is_valid_confidence(s) {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid confidence {s:?}; valid: high, medium, low"
                )));
            }
        }

        if let Some(v) = obj.get("evidence") {
            if !v.is_array() {
                return Err(RuntimeError::InvalidInput(
                    "evidence must be an array".into(),
                ));
            }
        }

        let content = args
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| title.clone());

        let root = args
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Internal("create args must be a JSON object".into()))?;
        root.insert("name".into(), json!(title));
        root.insert("content".into(), json!(content));
        root.insert("properties".into(), props);

        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: uuid::Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}
