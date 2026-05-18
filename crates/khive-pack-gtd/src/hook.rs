//! `TaskHook` — gtd's per-kind specialization for the `task` note kind (ADR-030).
//!
//! Registered via `GtdPack::kind_hook("task")`, this hook layers GTD semantics
//! over kg's shared `create` path:
//!
//! - `prepare_create` normalizes user-facing fields (`title`, `priority`,
//!   `status`, `depends_on`, …) into the kg-shape (`name`, `content`,
//!   `properties`, `salience`) that the shared CRUD writes to storage.
//! - `after_create` creates `depends_on` graph edges from the new task to each
//!   resolved dependency (best-effort — failures are logged, not propagated).
//!
//! The gtd `assign` verb (handlers.rs) remains as a flavored convenience —
//! both paths now produce equivalent task notes; `create(kind="note",
//! note_kind="task", ...)` is the canonical CRUD route.

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};
use khive_storage::EdgeRelation;

use crate::handlers::resolve_uuid;
use crate::schema::{is_valid_priority, is_valid_status, normalize_status, priority_to_salience};

#[derive(Debug, Default)]
pub struct TaskHook;

#[async_trait]
impl KindHook for TaskHook {
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let title = args
            .get("title")
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("kind=note + note_kind=task requires 'title'".into())
            })?;
        if title.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("title must not be empty".into()));
        }

        let status_in = args
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("inbox");
        let status = normalize_status(status_in);
        if !is_valid_status(status) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid status {status_in:?} — valid: inbox, next, waiting, someday, active, done, cancelled \
                 (aliases: in_progress, todo, blocked, later, finished)"
            )));
        }

        let priority = args
            .get("priority")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref p) = priority {
            if !is_valid_priority(p) {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid priority {p:?} — valid: p0, p1, p2, p3"
                )));
            }
        }
        let salience = priority.as_deref().map(priority_to_salience).unwrap_or(0.5);

        let namespace = args
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::to_string);

        // Resolve depends_on entries (full UUID or 8+ hex prefix) to canonical
        // UUID strings — matches the shape gtd's `assign` produces.
        let mut resolved_deps: Vec<String> = Vec::new();
        if let Some(arr) = args.get("depends_on").and_then(Value::as_array) {
            for entry in arr {
                let raw = entry.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput("depends_on entries must be strings".into())
                })?;
                let uuid = resolve_uuid(raw, runtime, namespace.as_deref()).await?;
                resolved_deps.push(uuid.as_hyphenated().to_string());
            }
        }

        // Start from any user-supplied `properties` (object only) so foreign
        // keys survive; task-controlled keys are overwritten with normalized
        // values. If `properties` is present but not an object, replace it.
        let mut props = args
            .get("properties")
            .cloned()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        let obj = props
            .as_object_mut()
            .expect("props is object by construction");
        obj.insert("status".into(), json!(status.to_string()));
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref desc) = description {
            obj.insert("description".into(), json!(desc));
        }
        if let Some(v) = args.get("assignee").and_then(Value::as_str) {
            obj.insert("assignee".into(), json!(v));
        }
        if let Some(ref pri) = priority {
            obj.insert("priority".into(), json!(pri.to_ascii_lowercase()));
        }
        if let Some(v) = args.get("due").and_then(Value::as_str) {
            obj.insert("due".into(), json!(v));
        }
        if let Some(v) = args.get("start").and_then(Value::as_str) {
            obj.insert("start".into(), json!(v));
        }
        if let Some(v) = args.get("end").and_then(Value::as_str) {
            obj.insert("end".into(), json!(v));
        }
        if !resolved_deps.is_empty() {
            obj.insert("depends_on".into(), json!(resolved_deps));
        }
        if let Some(tags) = args.get("tags").cloned() {
            obj.insert("tags".into(), tags);
        }

        let content = description.unwrap_or_else(|| title.clone());

        let root = args
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Internal("create args must be a JSON object".into()))?;
        root.insert("name".into(), json!(title));
        root.insert("content".into(), json!(content));
        root.insert("salience".into(), json!(salience));
        root.insert("properties".into(), props);

        Ok(())
    }

    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError> {
        let deps = args
            .get("properties")
            .and_then(|p| p.get("depends_on"))
            .and_then(Value::as_array);

        if let Some(arr) = deps {
            let namespace = args.get("namespace").and_then(Value::as_str);
            for entry in arr {
                let Some(raw) = entry.as_str() else { continue };
                let target = match Uuid::parse_str(raw) {
                    Ok(u) => u,
                    Err(_) => {
                        tracing::warn!(target = raw, "task depends_on entry is not a UUID");
                        continue;
                    }
                };
                if let Err(e) = runtime
                    .link(namespace, id, target, EdgeRelation::DependsOn, 1.0)
                    .await
                {
                    tracing::warn!(
                        from = %id,
                        to = %target,
                        error = %e,
                        "failed to create depends_on edge"
                    );
                }
            }
        }

        Ok(())
    }
}
