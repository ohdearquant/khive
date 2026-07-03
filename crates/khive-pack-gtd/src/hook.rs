//! `TaskHook` — gtd's per-kind specialization for the `task` note kind.
//!
//! Implements the `KindHook` extension point for the pack standard. Normalises
//! user-facing GTD fields into the kg storage shape on `prepare_create`, and
//! creates `depends_on` graph edges on `after_create` (best-effort). GTD
//! lifecycle semantics are documented in `docs/design.md`.

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, Namespace, Resolved, RuntimeError};
use khive_storage::EdgeRelation;

use crate::handlers::{parse_due, resolve_uuid};
use crate::schema::{
    is_terminal, is_valid_priority, is_valid_status, normalize_status, priority_to_salience,
};

#[derive(Debug, Default)]
/// KindHook implementation for the `task` note kind; normalises GTD fields on create.
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
        if is_terminal(status) {
            return Err(RuntimeError::InvalidInput(format!(
                "cannot create task in terminal state {status:?}; \
                 use one of: inbox, next, waiting, someday, active"
            )));
        }

        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        // Start from any user-supplied `properties` (object only) so foreign
        // keys survive; task-controlled keys are overwritten with normalized
        // values. If `properties` is present but not an object, replace it.
        // Set up `props` before extracting priority/depends_on so both
        // top-level args and nested `properties` fields are considered —
        // the ADR-019 generic `create(kind="note", note_kind="task", ...)`
        // form must accept task-controlled fields nested under `properties`
        // the same way `gtd.assign` accepts them at the top level.
        let mut props = args
            .get("properties")
            .cloned()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        let obj = props
            .as_object_mut()
            .expect("props is object by construction");

        // Precedence: top-level wins if present, else nested `properties`,
        // else the field's default. This matches the documented top-level
        // API surface taking priority while still letting the generic
        // properties-only form round-trip without being silently defaulted.
        let priority = args
            .get("priority")
            .and_then(Value::as_str)
            .or_else(|| obj.get("priority").and_then(Value::as_str))
            .map(str::to_string);
        if let Some(ref p) = priority {
            if !is_valid_priority(p) {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid priority {p:?} — valid: p0, p1, p2, p3"
                )));
            }
        }
        let salience = priority.as_deref().map(priority_to_salience).unwrap_or(0.5);

        // Resolve depends_on entries (full UUID or 8+ hex prefix) to canonical
        // UUID strings — matches the shape gtd's `assign` produces. Also
        // pre-validate each target is a task note in the primary namespace
        // before the storage write, using the same `resolve_primary`
        // resolver as `gtd.assign` and runtime link-mutation validation, so
        // this create path never leaves a task persisted with a
        // `properties.depends_on` pointing at a non-task or a visible-only
        // (non-primary-namespace) note that runtime link creation would
        // reject (the GTD pack edge rule only legalises task→task
        // `depends_on` within the primary namespace).
        let depends_on_source = args
            .get("depends_on")
            .cloned()
            .or_else(|| obj.get("depends_on").cloned());
        let depends_on_present = depends_on_source.is_some();
        let mut resolved_deps: Vec<String> = Vec::new();
        if let Some(value) = depends_on_source {
            let arr = value.as_array().ok_or_else(|| {
                RuntimeError::InvalidInput("depends_on must be an array of strings".into())
            })?;
            for entry in arr {
                let raw = entry.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput("depends_on entries must be strings".into())
                })?;
                let uuid = resolve_uuid(raw, runtime, &token).await?;
                match runtime.resolve_primary(&token, uuid).await? {
                    Some(Resolved::Note(n)) if n.kind == "task" => {}
                    Some(Resolved::Note(n)) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "depends_on target {uuid} must be a task note for relation depends_on \
                             (got note kind {:?}); the GTD pack edge rule is task→task only",
                            n.kind
                        )));
                    }
                    Some(_) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "depends_on target {uuid} must be a task note for relation depends_on \
                             (got non-note substrate); the GTD pack edge rule is task→task only"
                        )));
                    }
                    None => {
                        return Err(RuntimeError::NotFound(format!(
                            "depends_on target {uuid} not found in namespace"
                        )));
                    }
                }
                resolved_deps.push(uuid.as_hyphenated().to_string());
            }
        }

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
        // Always persist priority (defaults to "p2") so listing filters can
        // match defaulted tasks via `properties.priority`.
        let priority_value = priority
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "p2".to_string());
        obj.insert("priority".into(), json!(priority_value));
        if let Some(v) = args.get("due").and_then(Value::as_str) {
            obj.insert("due".into(), json!(parse_due(v)?));
        }
        if let Some(v) = args.get("start").and_then(Value::as_str) {
            obj.insert("start".into(), json!(v));
        }
        if let Some(v) = args.get("end").and_then(Value::as_str) {
            obj.insert("end".into(), json!(v));
        }
        // Write back only the canonicalized dependency list (or clear a
        // stale/invalid nested value) so `after_create` always consumes
        // validated data regardless of which input location supplied it.
        if depends_on_present {
            if resolved_deps.is_empty() {
                obj.remove("depends_on");
            } else {
                obj.insert("depends_on".into(), json!(resolved_deps));
            }
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
            let token = args
                .get("namespace")
                .and_then(Value::as_str)
                .and_then(|s| Namespace::parse(s).ok())
                .map(|ns| runtime.authorize(ns))
                .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;
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
                    .link(&token, id, target, EdgeRelation::DependsOn, 1.0, None)
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
