//! Shared task-create normalization/validation.
//!
//! Single source of truth for both `gtd.assign` (`GtdPack::handle_assign`) and
//! the generic `create(kind="note", note_kind="task")` path (`TaskHook`).
//! Before #625/#626 these two paths independently re-derived status/priority
//! validation, dependency-target resolution, and context-entity handling and
//! had drifted: only `gtd.assign` populated `context_entity_id`/`annotates`.
//! Routing both through [`prepare_task_create`] and [`link_depends_on_edges`]
//! closes that gap without changing either verb's public parameter or
//! response shape.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, Resolved, RuntimeError};
use khive_storage::EdgeRelation;

use crate::handlers::{parse_due, resolve_context_entity_id, resolve_uuid};
use crate::schema::{
    is_terminal, is_valid_priority, is_valid_status, normalize_status, priority_to_salience,
};

/// Task-create fields, already extracted from either `gtd.assign`'s top-level
/// params or the generic create path's args. `properties` carries the
/// generic path's nested-properties object (empty for `gtd.assign`, which has
/// no nested-properties calling convention) — [`prepare_task_create`] falls
/// back to it for `priority`/`depends_on`/`context_entity_id` exactly as
/// `TaskHook::prepare_create` did before unification, so `create(kind="note",
/// note_kind="task", properties={"priority": "p1"})` keeps working.
#[derive(Debug, Default)]
pub(crate) struct TaskCreateInput {
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) assignee: Option<String>,
    pub(crate) priority: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) due: Option<String>,
    pub(crate) start: Option<String>,
    pub(crate) end: Option<String>,
    pub(crate) depends_on: Option<Vec<String>>,
    pub(crate) context_entity_id: Option<String>,
    pub(crate) tags: Option<Value>,
    pub(crate) properties: Value,
}

impl TaskCreateInput {
    /// Build an input from the generic `create` verb's raw args, honoring
    /// the same `title`/`name` fallback `TaskHook::prepare_create` used.
    pub(crate) fn from_create_args(args: &Value) -> Result<Self, RuntimeError> {
        let title = args
            .get("title")
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("kind=note + note_kind=task requires 'title'".into())
            })?;

        let depends_on = match args.get("depends_on") {
            Some(value) => {
                let arr = value.as_array().ok_or_else(|| {
                    RuntimeError::InvalidInput("depends_on must be an array of strings".into())
                })?;
                Some(
                    arr.iter()
                        .map(|v| {
                            v.as_str().map(str::to_string).ok_or_else(|| {
                                RuntimeError::InvalidInput(
                                    "depends_on entries must be strings".into(),
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
            None => None,
        };

        Ok(Self {
            title,
            description: args
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            assignee: args
                .get("assignee")
                .and_then(Value::as_str)
                .map(str::to_string),
            priority: args
                .get("priority")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: args
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            due: args.get("due").and_then(Value::as_str).map(str::to_string),
            start: args
                .get("start")
                .and_then(Value::as_str)
                .map(str::to_string),
            end: args.get("end").and_then(Value::as_str).map(str::to_string),
            depends_on,
            context_entity_id: args
                .get("context_entity_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tags: args.get("tags").cloned(),
            properties: args
                .get("properties")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({})),
        })
    }
}

/// Fully normalized/validated task ready to persist as a note.
pub(crate) struct PreparedTaskCreate {
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) salience: f64,
    pub(crate) properties: Value,
    pub(crate) annotates: Vec<Uuid>,
}

impl PreparedTaskCreate {
    /// Write the prepared fields back into the generic create path's args in
    /// the shape `khive-pack-kg`'s `create` handler expects (`name`,
    /// `content`, `salience`, `properties`, top-level `annotates`).
    pub(crate) fn apply_to_create_args(&self, args: &mut Value) -> Result<(), RuntimeError> {
        let root = args
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Internal("create args must be a JSON object".into()))?;
        root.insert("name".into(), json!(self.title));
        root.insert("content".into(), json!(self.content));
        root.insert("salience".into(), json!(self.salience));
        root.insert("properties".into(), self.properties.clone());
        if !self.annotates.is_empty() {
            root.insert(
                "annotates".into(),
                json!(self
                    .annotates
                    .iter()
                    .map(|u| u.as_hyphenated().to_string())
                    .collect::<Vec<_>>()),
            );
        }
        Ok(())
    }
}

/// Validate and normalize a task-create request: status/priority checks,
/// `depends_on` target resolution (task-note + primary-namespace
/// pre-validation, so neither path ever leaves an orphaned task whose
/// post-write link is rejected), and `context_entity_id` resolution
/// (fail-closed before the write, and recorded as an `annotates` edge target
/// so both paths produce the same task→context edge).
pub(crate) async fn prepare_task_create(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    input: TaskCreateInput,
) -> Result<PreparedTaskCreate, RuntimeError> {
    if input.title.trim().is_empty() {
        return Err(RuntimeError::InvalidInput("title must not be empty".into()));
    }

    let status_in = input.status.as_deref().unwrap_or("inbox");
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

    let mut props = input.properties;
    if !props.is_object() {
        props = json!({});
    }
    let obj = props
        .as_object_mut()
        .expect("props is object after replacement");

    let priority = input.priority.clone().or_else(|| {
        obj.get("priority")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    if let Some(ref p) = priority {
        if !is_valid_priority(p) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid priority {p:?} — valid: p0, p1, p2, p3"
            )));
        }
    }
    let salience = priority.as_deref().map(priority_to_salience).unwrap_or(0.5);
    let priority_value = priority
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "p2".to_string());

    let depends_on_source = input
        .depends_on
        .clone()
        .map(|deps| json!(deps))
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
            let uuid = resolve_uuid(raw, runtime, token).await?;
            // Mutation rule: depends_on targets must be in the PRIMARY namespace.
            // A visible-only (foreign) task is NotFound here — callers must own
            // both sides of a dependency edge. NotFound (not Forbidden) per
            // ADR-007:215-219. Pre-validated before any storage write so a bad
            // target never leaves an orphaned task behind (after_create /
            // link_depends_on_edges are best-effort and don't propagate).
            match runtime.resolve_primary(token, uuid).await? {
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

    let mut annotates = Vec::new();
    let context_entity_source = input.context_entity_id.clone().or_else(|| {
        obj.get("context_entity_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    if let Some(raw) = context_entity_source {
        let context_uuid = resolve_context_entity_id(&raw, runtime, token).await?;
        obj.insert(
            "context_entity_id".into(),
            json!(context_uuid.as_hyphenated().to_string()),
        );
        annotates.push(context_uuid);
    }

    obj.insert("status".into(), json!(status.to_string()));
    obj.insert("priority".into(), json!(priority_value));
    if let Some(ref desc) = input.description {
        obj.insert("description".into(), json!(desc));
    }
    if let Some(ref assignee) = input.assignee {
        obj.insert("assignee".into(), json!(assignee));
    }
    if let Some(ref due) = input.due {
        obj.insert("due".into(), json!(parse_due(due)?));
    }
    if let Some(ref start) = input.start {
        obj.insert("start".into(), json!(start));
    }
    if let Some(ref end) = input.end {
        obj.insert("end".into(), json!(end));
    }
    if depends_on_present {
        if resolved_deps.is_empty() {
            obj.remove("depends_on");
        } else {
            obj.insert("depends_on".into(), json!(resolved_deps));
        }
    }
    if let Some(tags) = input.tags.clone() {
        obj.insert("tags".into(), tags);
    }

    let content = input
        .description
        .clone()
        .unwrap_or_else(|| input.title.clone());
    Ok(PreparedTaskCreate {
        title: input.title,
        content,
        salience,
        properties: props,
        annotates,
    })
}

/// Post-write `depends_on` edge creation, shared by `GtdPack::handle_assign`
/// and `TaskHook::after_create`. Best-effort: failures are logged, not
/// propagated — the task note is already committed by the time this runs, so
/// surfacing a link failure to the caller would misleadingly report a task
/// that's already on disk as failed.
pub(crate) async fn link_depends_on_edges(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    task_id: Uuid,
    properties: &Value,
    log_context: &'static str,
) {
    let Some(arr) = properties.get("depends_on").and_then(Value::as_array) else {
        return;
    };
    for entry in arr {
        let Some(raw) = entry.as_str() else { continue };
        let target = match Uuid::parse_str(raw) {
            Ok(u) => u,
            Err(_) => {
                tracing::warn!(
                    target = raw,
                    "{log_context}: depends_on entry is not a UUID"
                );
                continue;
            }
        };
        if let Err(e) = runtime
            .link(token, task_id, target, EdgeRelation::DependsOn, 1.0, None)
            .await
        {
            tracing::warn!(
                from = %task_id,
                to = %target,
                error = %e,
                "{log_context}: depends_on edge failed after task write (non-fatal)"
            );
        }
    }
}
