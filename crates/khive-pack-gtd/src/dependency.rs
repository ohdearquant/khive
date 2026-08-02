//! GTD dependency validation and read-time diagnostics.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, LinkSpec, NamespaceToken, RuntimeError};
use khive_storage::note::Note;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::EdgeRelation;

const DEPENDENCY_WALK_MAX_NODES: usize = 20_000;

#[derive(Clone, Debug)]
pub(crate) struct TaskDependencyDiagnostic {
    state: &'static str,
    blocked_by: Vec<Value>,
}

impl TaskDependencyDiagnostic {
    pub(crate) fn is_ready(&self) -> bool {
        self.state == "ready"
    }

    pub(crate) fn render(&self, note: &Note) -> Value {
        let mut rendered = crate::handlers::render_task(note);
        let lifecycle_actionable = note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "next" | "active"));
        if let Some(object) = rendered.as_object_mut() {
            object.insert("dependency_state".into(), json!(self.state));
            object.insert(
                "actionable".into(),
                json!(lifecycle_actionable && self.is_ready()),
            );
            object.insert("blocked_by".into(), Value::Array(self.blocked_by.clone()));
        }
        rendered
    }
}

pub(crate) async fn diagnose_tasks(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tasks: &[Note],
) -> Result<Vec<TaskDependencyDiagnostic>, RuntimeError> {
    let mut dependency_ids = HashSet::new();
    for task in tasks {
        let Some(dependencies) = task
            .properties
            .as_ref()
            .and_then(|properties| properties.get("depends_on"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        dependency_ids.extend(dependencies.iter().filter_map(|dependency| {
            dependency
                .as_str()
                .and_then(|raw| Uuid::parse_str(raw).ok())
        }));
    }

    let store = runtime.notes(token)?;
    let ids: Vec<Uuid> = dependency_ids.iter().copied().collect();
    let mut notes_by_id: HashMap<Uuid, Option<Note>> = store
        .get_notes_batch(&ids)
        .await?
        .into_iter()
        .map(|note| (note.id, Some(note)))
        .collect();

    for id in dependency_ids {
        if notes_by_id.contains_key(&id) {
            continue;
        }
        notes_by_id.insert(id, store.get_note_including_deleted(id).await?);
    }

    Ok(tasks
        .iter()
        .map(|task| diagnose_task(task, &notes_by_id))
        .collect())
}

fn diagnose_task(
    task: &Note,
    notes_by_id: &HashMap<Uuid, Option<Note>>,
) -> TaskDependencyDiagnostic {
    let mut blocked_by = Vec::new();
    let mut broken = false;
    let Some(depends_on) = task
        .properties
        .as_ref()
        .and_then(|properties| properties.get("depends_on"))
    else {
        return TaskDependencyDiagnostic {
            state: "ready",
            blocked_by,
        };
    };
    let Some(dependencies) = depends_on.as_array() else {
        return TaskDependencyDiagnostic {
            state: "broken",
            blocked_by: vec![json!({"id": depends_on, "state": "invalid"})],
        };
    };

    for dependency in dependencies {
        let Some(raw) = dependency.as_str() else {
            broken = true;
            blocked_by.push(json!({"id": dependency, "state": "invalid"}));
            continue;
        };
        let Ok(id) = Uuid::parse_str(raw) else {
            broken = true;
            blocked_by.push(json!({"id": raw, "state": "invalid"}));
            continue;
        };
        let Some(Some(blocker)) = notes_by_id.get(&id) else {
            broken = true;
            blocked_by.push(json!({"id": raw, "state": "missing"}));
            continue;
        };
        if blocker.namespace != task.namespace {
            broken = true;
            blocked_by.push(json!({"id": raw, "state": "different_namespace"}));
            continue;
        }
        if blocker.deleted_at.is_some() {
            broken = true;
            blocked_by.push(json!({"id": raw, "state": "soft_deleted"}));
            continue;
        }
        if blocker.kind != "task" {
            broken = true;
            blocked_by.push(json!({"id": raw, "state": "wrong_kind"}));
            continue;
        }
        let blocker_status = blocker
            .properties
            .as_ref()
            .and_then(|properties| properties.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("inbox");
        match blocker_status {
            "done" => {}
            "cancelled" => {
                broken = true;
                blocked_by.push(json!({"id": raw, "state": "cancelled"}));
            }
            status => blocked_by.push(json!({
                "id": raw,
                "state": "pending",
                "status": status,
            })),
        }
    }

    TaskDependencyDiagnostic {
        state: if broken {
            "broken"
        } else if blocked_by.is_empty() {
            "ready"
        } else {
            "blocked"
        },
        blocked_by,
    }
}

pub(crate) async fn validate_property_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    task: &Note,
    properties: Option<&Value>,
) -> Result<(), RuntimeError> {
    let Some(depends_on) = properties
        .and_then(Value::as_object)
        .and_then(|object| object.get("depends_on"))
    else {
        return Ok(());
    };
    let dependencies = depends_on.as_array().ok_or_else(|| {
        RuntimeError::InvalidInput("task properties.depends_on must be an array of UUIDs".into())
    })?;
    if dependencies.len() > DEPENDENCY_WALK_MAX_NODES {
        return Err(RuntimeError::InvalidInput(format!(
            "depends_on cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-edge safety bound"
        )));
    }
    let store = runtime.notes(token)?;
    for dependency in dependencies {
        let raw = dependency.as_str().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "task properties.depends_on entries must be full UUID strings".into(),
            )
        })?;
        let dependency_id = Uuid::parse_str(raw).map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "task properties.depends_on entry {raw:?} must be a full UUID"
            ))
        })?;
        let canonical = dependency_id.as_hyphenated().to_string();
        if raw != canonical {
            return Err(RuntimeError::InvalidInput(format!(
                "task properties.depends_on entry {raw:?} must use the canonical lowercase hyphenated UUID {canonical:?}"
            )));
        }
        if dependency_id == task.id {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on update would create a dependency cycle: task {} cannot depend on itself",
                task.id
            )));
        }
        let blocker = store.get_note(dependency_id).await?.ok_or_else(|| {
            RuntimeError::NotFound(format!(
                "depends_on target {dependency_id} is missing or deleted"
            ))
        })?;
        if blocker.kind != "task" {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on target {dependency_id} must be a task note; got {:?}",
                blocker.kind
            )));
        }
        if blocker.namespace != task.namespace {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on target {dependency_id} must share task namespace {:?}",
                task.namespace
            )));
        }
        if property_path_reaches(
            runtime,
            token,
            dependency_id,
            task.id,
            task.namespace.as_str(),
        )
        .await?
        {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on update would create a dependency cycle: blocker {dependency_id} already reaches task {}",
                task.id
            )));
        }
    }
    Ok(())
}

async fn property_path_reaches(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    start: Uuid,
    goal: Uuid,
    namespace: &str,
) -> Result<bool, RuntimeError> {
    let store = runtime.notes(token)?;
    let mut queue = VecDeque::from([start]);
    let mut visited = HashSet::new();
    let mut traversed_edges = 0usize;
    while let Some(current) = queue.pop_front() {
        if current == goal {
            return Ok(true);
        }
        if !visited.insert(current) {
            continue;
        }
        if visited.len() > DEPENDENCY_WALK_MAX_NODES {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-task safety bound"
            )));
        }
        let Some(note) = store.get_note(current).await? else {
            continue;
        };
        if note.kind != "task" || note.namespace != namespace {
            continue;
        }
        let Some(dependencies) = note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("depends_on"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        traversed_edges = traversed_edges
            .checked_add(dependencies.len())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "depends_on cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-edge safety bound"
                ))
            })?;
        if traversed_edges > DEPENDENCY_WALK_MAX_NODES {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-edge safety bound"
            )));
        }
        queue.extend(dependencies.iter().filter_map(|dependency| {
            dependency
                .as_str()
                .and_then(|raw| Uuid::parse_str(raw).ok())
        }));
    }
    Ok(false)
}

pub(crate) async fn validate_dependency_links(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    links: &[LinkSpec],
) -> Result<(), RuntimeError> {
    let store = runtime.notes(token)?;
    let mut proposed: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut task_links = Vec::new();
    for link in links {
        if link.relation != EdgeRelation::DependsOn {
            continue;
        }
        let Some(source) = store.get_note(link.source_id).await? else {
            continue;
        };
        if source.kind != "task" {
            continue;
        }
        let target = store.get_note(link.target_id).await?.ok_or_else(|| {
            RuntimeError::NotFound(format!("depends_on target {} not found", link.target_id))
        })?;
        if target.kind != "task" {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on target {} must be a task note; got {:?}",
                link.target_id, target.kind
            )));
        }
        proposed
            .entry(link.source_id)
            .or_default()
            .push(link.target_id);
        task_links.push((link.source_id, link.target_id));
    }

    for (source, target) in task_links {
        if source == target {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on link would create a dependency cycle: task {source} cannot depend on itself"
            )));
        }
        if edge_path_reaches(
            runtime,
            token.namespace().as_str(),
            target,
            source,
            &proposed,
        )
        .await?
        {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on link would create a dependency cycle: {source} -> {target} closes a path back to {source}"
            )));
        }
    }
    Ok(())
}

async fn edge_path_reaches(
    runtime: &KhiveRuntime,
    namespace: &str,
    start: Uuid,
    goal: Uuid,
    proposed: &HashMap<Uuid, Vec<Uuid>>,
) -> Result<bool, RuntimeError> {
    let mut reader = runtime.sql().reader().await?;
    let mut queue = VecDeque::from([start]);
    let mut visited = HashSet::new();
    let mut traversed_edges = 0usize;
    while let Some(current) = queue.pop_front() {
        if current == goal {
            return Ok(true);
        }
        if !visited.insert(current) {
            continue;
        }
        if visited.len() > DEPENDENCY_WALK_MAX_NODES {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on edge cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-node safety bound"
            )));
        }
        if let Some(targets) = proposed.get(&current) {
            traversed_edges += targets.len();
            queue.extend(targets.iter().copied());
        }
        let remaining = DEPENDENCY_WALK_MAX_NODES.saturating_sub(traversed_edges);
        if remaining == 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on edge cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-edge safety bound"
            )));
        }
        let rows = reader
            .query_all(SqlStatement {
                // Soft-deleting a task deliberately leaves its incident graph
                // edges in place.  Those edges are not part of the live GTD
                // dependency graph: following them here would make an
                // otherwise-acyclic link fail merely because a tombstoned task
                // sits on an old path.  Require both endpoints of every
                // traversed hop to be live task notes, matching property-walk
                // and read-diagnostic semantics.
                sql: "SELECT edge.target_id FROM graph_edges AS edge \
                      JOIN notes AS source_task \
                        ON source_task.id = edge.source_id \
                       AND source_task.kind = 'task' \
                       AND source_task.deleted_at IS NULL \
                      JOIN notes AS target_task \
                        ON target_task.id = edge.target_id \
                       AND target_task.kind = 'task' \
                       AND target_task.deleted_at IS NULL \
                      WHERE edge.source_id = ?1 AND edge.namespace = ?2 \
                        AND edge.relation = 'depends_on' AND edge.deleted_at IS NULL \
                      ORDER BY edge.target_id LIMIT ?3"
                    .into(),
                params: vec![
                    SqlValue::Text(current.as_hyphenated().to_string()),
                    SqlValue::Text(namespace.to_owned()),
                    SqlValue::Integer((remaining + 1) as i64),
                ],
                label: Some("gtd_dependency_cycle_edges".into()),
            })
            .await?;
        if rows.len() > remaining {
            return Err(RuntimeError::InvalidInput(format!(
                "depends_on edge cycle validation exceeded the {DEPENDENCY_WALK_MAX_NODES}-edge safety bound"
            )));
        }
        traversed_edges += rows.len();
        queue.extend(
            rows.into_iter()
                .filter_map(|row| match row.get("target_id") {
                    Some(SqlValue::Text(raw)) => Uuid::parse_str(raw).ok(),
                    _ => None,
                }),
        );
    }
    Ok(false)
}
