//! `TaskHook` — gtd's per-kind specialization for the `task` note kind.
//!
//! Implements the `KindHook` extension point for the pack standard. Normalises
//! user-facing GTD fields into the kg storage shape on `prepare_create`, keeps
//! task body mirrors aligned on `prepare_note_update`, and creates `depends_on`
//! graph edges on `after_create` (best-effort). GTD lifecycle semantics are
//! documented in `docs/design.md`.

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, LinkSpec, Namespace, NamespaceToken, RuntimeError};
use khive_storage::Note;

use crate::task_create::{link_depends_on_edges, prepare_task_create, TaskCreateInput};

#[derive(Debug, Default)]
/// KindHook implementation for the `task` note kind; normalises GTD fields on create.
pub struct TaskHook;

fn synchronize_description(note: &Note, args: &mut Value) -> Result<(), RuntimeError> {
    let root = args
        .as_object_mut()
        .ok_or_else(|| RuntimeError::InvalidInput("update args must be an object".into()))?;

    let content_patch = root
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string);
    let name_patch = match root.get("name") {
        None => None,
        Some(Value::String(name)) => Some(name.clone()),
        Some(Value::Null) => {
            return Err(RuntimeError::InvalidInput(
                "task title cannot be cleared; `name` must be a non-empty string".into(),
            ));
        }
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "task update field `name` must be a string; got {other}"
            )));
        }
    };
    let description_patch = match root.get("properties") {
        None | Some(Value::Null) => None,
        Some(Value::Object(properties)) => match properties.get("description") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(description)) => Some(Some(description.clone())),
            Some(other) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "properties.description must be a string or null; got {other}"
                )))
            }
        },
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "properties on a `task` note must be patched with an object; got {other}"
            )))
        }
    };

    let effective_title = name_patch
        .as_deref()
        .or(note.name.as_deref())
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| RuntimeError::InvalidInput("task title must not be empty".into()))?;

    match (content_patch, description_patch) {
        (Some(content), Some(Some(description))) if content != description => {
            return Err(RuntimeError::InvalidInput(
                "task update fields `content` and `properties.description` must match when both are supplied"
                    .into(),
            ));
        }
        (Some(_), Some(None)) => {
            return Err(RuntimeError::InvalidInput(
                "task update cannot set `content` while clearing `properties.description`".into(),
            ));
        }
        (Some(content), _) => {
            if root.get("properties").is_none_or(Value::is_null) {
                root.insert("properties".into(), json!({}));
            }
            let properties = root
                .get_mut("properties")
                .expect("properties was inserted")
                .as_object_mut()
                .expect("properties was validated as object or inserted as object");
            properties.insert("description".into(), json!(content));
        }
        (None, Some(Some(description))) => {
            root.insert("content".into(), json!(description));
        }
        (None, Some(None)) => {
            root.insert("content".into(), json!(effective_title));
        }
        (None, None)
            if name_patch.is_some()
                && note
                    .properties
                    .as_ref()
                    .and_then(|properties| properties.get("description"))
                    .and_then(Value::as_str)
                    .is_none() =>
        {
            root.insert("content".into(), json!(effective_title));
        }
        (None, None) => {}
    }

    Ok(())
}

#[async_trait]
impl KindHook for TaskHook {
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        // #625/#626: this generic `create(kind="note", note_kind="task")`
        // entry point and `gtd.assign` (`GtdPack::handle_assign` in
        // handlers.rs) both normalize/validate through
        // `task_create::prepare_task_create` so status/priority checks,
        // `depends_on` resolution, and `context_entity_id` handling can't
        // drift between the two paths again.
        let input = TaskCreateInput::from_create_args(args)?;
        let prepared = prepare_task_create(runtime, &token, input).await?;
        prepared.apply_to_create_args(args)?;

        Ok(())
    }

    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError> {
        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        if let Some(properties) = args.get("properties") {
            link_depends_on_edges(runtime, &token, id, properties, "task hook").await;
        }

        Ok(())
    }

    async fn validate_note_update(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        crate::dependency::validate_property_update(runtime, token, note, properties).await
    }

    async fn prepare_note_update(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        synchronize_description(note, args)?;
        let properties = args.get("properties").filter(|value| !value.is_null());
        crate::dependency::validate_property_update(runtime, token, note, properties).await
    }

    async fn validate_links(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        links: &[LinkSpec],
    ) -> Result<(), RuntimeError> {
        crate::dependency::validate_dependency_links(runtime, token, links).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[tokio::test]
    async fn normalized_task_update_refuses_a_stale_note_snapshot() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let mut task = Note::new("local", "task", "original body");
        task.name = Some("original title".to_string());
        task.properties = Some(json!({"description": "original body", "status": "inbox"}));
        let task_id = task.id;
        runtime
            .notes(&token)
            .expect("note store")
            .upsert_note(task)
            .await
            .expect("seed task");

        let snapshot = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        let mut args = json!({"content": "hook-derived body"});
        synchronize_description(&snapshot, &mut args).expect("normalize mirrors");

        // Deterministically land another writer after hook normalization but
        // before persistence from the original snapshot.
        let mut concurrent = snapshot.clone();
        concurrent.name = Some("concurrent title".to_string());
        concurrent.content = "concurrent body".to_string();
        concurrent.properties = Some(json!({"description": "concurrent body", "status": "inbox"}));
        concurrent.updated_at = snapshot.updated_at.saturating_add(10);
        runtime
            .notes(&token)
            .expect("note store")
            .upsert_note(concurrent)
            .await
            .expect("concurrent write");

        let patch = khive_runtime::NotePatch::new(
            None,
            args.get("content")
                .and_then(Value::as_str)
                .map(str::to_string),
            None,
            None,
            args.get("properties").cloned(),
        );
        let err = runtime
            .update_note_from_snapshot_with_embedding_report(&token, snapshot, patch)
            .await
            .expect_err("stale hook snapshot must not overwrite the concurrent task");
        assert!(
            err.to_string().contains("changed concurrently"),
            "expected explicit stale-snapshot refusal; got: {err}"
        );

        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read persisted task")
            .expect("task exists");
        assert_eq!(persisted.name.as_deref(), Some("concurrent title"));
        assert_eq!(persisted.content, "concurrent body");
        assert_eq!(
            persisted
                .properties
                .as_ref()
                .and_then(|props| props.get("description"))
                .and_then(Value::as_str),
            Some("concurrent body")
        );
    }
}
