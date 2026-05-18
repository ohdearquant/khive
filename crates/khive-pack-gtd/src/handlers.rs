//! Verb handlers for the GTD pack.
//!
//! Each handler: deserialize params from Value → validate → mutate via runtime
//! → serialize a stable response shape (`id` short hex + `full_id` UUID).

use std::str::FromStr;

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::EdgeRelation;

use crate::schema::{
    allowed_transitions, can_transition, is_actionable, is_terminal, is_valid_priority,
    is_valid_status, normalize_status, priority_to_salience,
};
use crate::GtdPack;

// ── param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AssignParams {
    namespace: Option<String>,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct NextParams {
    namespace: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    assignee: Option<String>,
}

#[derive(Deserialize)]
struct CompleteParams {
    namespace: Option<String>,
    id: String,
    #[serde(default)]
    result: Option<String>,
}

#[derive(Deserialize)]
struct TasksParams {
    namespace: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Deserialize)]
struct TransitionParams {
    namespace: Option<String>,
    id: String,
    status: String,
    #[serde(default)]
    note: Option<String>,
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

pub(crate) async fn resolve_uuid(
    s: &str,
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(namespace, s).await? {
            Some(uuid) => Ok(uuid),
            None => Err(RuntimeError::InvalidInput(format!(
                "no record matches prefix: {s:?}"
            ))),
        };
    }
    Err(RuntimeError::InvalidInput(format!(
        "invalid UUID (expected full UUID or 8+ hex prefix): {s:?}"
    )))
}

/// Status used internally on a task. Defaults to "inbox" when missing/empty.
fn task_status(props: Option<&Value>) -> String {
    props
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("inbox")
        .to_string()
}

/// Priority rank used for sorting actionable tasks (lower = higher priority).
/// Unknown / missing priorities sort to "p2" so they don't dominate p0/p1.
fn priority_rank(props: Option<&Value>) -> u8 {
    let raw = props
        .and_then(|p| p.get("priority"))
        .and_then(|v| v.as_str())
        .unwrap_or("p2")
        .to_ascii_lowercase();
    match raw.as_str() {
        "p0" => 0,
        "p1" => 1,
        "p2" => 2,
        "p3" => 3,
        _ => 2,
    }
}

/// Build the response object for any task-shaped operation.
fn render_task(note: &khive_storage::note::Note) -> Value {
    let props = note.properties.clone().unwrap_or(json!({}));
    let title = note
        .name
        .clone()
        .unwrap_or_else(|| note.content.chars().take(80).collect());
    let status = props
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("inbox")
        .to_string();
    let priority = props
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("p2")
        .to_string();
    let assignee = props.get("assignee").cloned().unwrap_or(Value::Null);
    let due = props.get("due").cloned().unwrap_or(Value::Null);
    let uuid_str = note.id.as_hyphenated().to_string();
    json!({
        "id": short_id(note.id),
        "full_id": uuid_str,
        "kind": "task",
        "title": title,
        "status": status,
        "priority": priority,
        "assignee": assignee,
        "due": due,
        "namespace": note.namespace,
        "created_at": ts_to_rfc(note.created_at),
        "updated_at": ts_to_rfc(note.updated_at),
        "properties": props,
    })
}

fn ts_to_rfc(micros: i64) -> String {
    chrono::DateTime::<Utc>::from_timestamp_micros(micros)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

/// Load a task note and verify (a) it exists, (b) namespace matches, (c) it is
/// actually `kind = "task"`. Used by `complete` and `transition`.
async fn load_task(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    raw_id: &str,
) -> Result<(khive_storage::note::Note, String), RuntimeError> {
    let uuid = resolve_uuid(raw_id, runtime, namespace).await?;
    let ns = runtime.ns(namespace);
    let store = runtime.notes(namespace)?;
    let note = store
        .get_note(uuid)
        .await
        .map_err(|e| RuntimeError::Internal(format!("get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("not found: {raw_id}")))?;

    if note.namespace != ns {
        return Err(RuntimeError::NotFound(format!("not found: {raw_id}")));
    }
    if note.kind != "task" {
        return Err(RuntimeError::InvalidInput(format!(
            "expected kind=\"task\", got {:?}",
            note.kind
        )));
    }
    if note.deleted_at.is_some() {
        return Err(RuntimeError::NotFound(format!("deleted: {raw_id}")));
    }

    let current = task_status(note.properties.as_ref());
    Ok((note, current))
}

// ── handlers ─────────────────────────────────────────────────────────────────

impl GtdPack {
    pub(crate) async fn handle_assign(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: AssignParams = deser(params)?;
        if p.title.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("title must not be empty".into()));
        }

        let status_in = p.status.as_deref().unwrap_or("inbox");
        let status = normalize_status(status_in);
        if !is_valid_status(status) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid status {status_in:?} — valid: inbox, next, waiting, someday, active, done, cancelled \
                 (aliases: in_progress, todo, blocked, later, finished)"
            )));
        }
        if let Some(ref pri) = p.priority {
            if !is_valid_priority(pri) {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid priority {pri:?} — valid: p0, p1, p2, p3"
                )));
            }
        }

        let salience = p
            .priority
            .as_deref()
            .map(priority_to_salience)
            .unwrap_or(0.5);

        // Resolve dependency IDs up front so we can both store them in properties
        // and create graph edges referencing the same UUIDs.
        let mut resolved_deps: Vec<Uuid> = Vec::new();
        if let Some(ref deps) = p.depends_on {
            for raw in deps {
                resolved_deps
                    .push(resolve_uuid(raw, self.runtime(), p.namespace.as_deref()).await?);
            }
        }

        let mut props = json!({ "status": status.to_string() });
        if let Some(ref desc) = p.description {
            props["description"] = json!(desc);
        }
        if let Some(ref assignee) = p.assignee {
            props["assignee"] = json!(assignee);
        }
        if let Some(ref pri) = p.priority {
            props["priority"] = json!(pri.to_ascii_lowercase());
        }
        if let Some(ref due) = p.due {
            props["due"] = json!(due);
        }
        if let Some(ref start) = p.start {
            props["start"] = json!(start);
        }
        if let Some(ref end) = p.end {
            props["end"] = json!(end);
        }
        if !resolved_deps.is_empty() {
            let dep_strs: Vec<String> = resolved_deps
                .iter()
                .map(|u| u.as_hyphenated().to_string())
                .collect();
            props["depends_on"] = json!(dep_strs);
        }
        if let Some(ref tags) = p.tags {
            props["tags"] = json!(tags);
        }

        // Content body powers semantic search; title doubles as the searchable text
        // when no description is supplied.
        let content = p.description.clone().unwrap_or_else(|| p.title.clone());

        let note = self
            .runtime()
            .create_note(
                p.namespace.as_deref(),
                "task",
                Some(p.title.as_str()),
                &content,
                salience,
                Some(props),
                Vec::new(),
            )
            .await?;

        // Best-effort: record `depends_on` as `depends_on` graph edges. Failure is
        // non-fatal — the property captures the same information for queries.
        for dep_uuid in resolved_deps {
            if let Err(e) = self
                .runtime()
                .link(
                    p.namespace.as_deref(),
                    note.id,
                    dep_uuid,
                    EdgeRelation::DependsOn,
                    1.0,
                )
                .await
            {
                tracing::warn!("assign: depends_on edge failed (non-fatal): {e}");
            }
        }

        Ok(render_task(&note))
    }

    pub(crate) async fn handle_next(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: NextParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).clamp(1, 200);

        // Pull a broad window of recent tasks, filter in-memory by GTD status.
        // 500 covers typical inbox/next/active backlogs without paging.
        let notes = self
            .runtime()
            .list_notes(p.namespace.as_deref(), Some("task"), 500)
            .await?;

        let mut actionable: Vec<&khive_storage::note::Note> = notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .filter(|n| is_actionable(&task_status(n.properties.as_ref())))
            .filter(|n| match p.assignee.as_deref() {
                None => true,
                Some(want) => {
                    n.properties
                        .as_ref()
                        .and_then(|v| v.get("assignee"))
                        .and_then(|v| v.as_str())
                        == Some(want)
                }
            })
            .collect();

        // Sort: priority ascending (p0 first), then created_at descending (recent first).
        actionable.sort_by(|a, b| {
            let ap = priority_rank(a.properties.as_ref());
            let bp = priority_rank(b.properties.as_ref());
            ap.cmp(&bp).then(b.created_at.cmp(&a.created_at))
        });
        actionable.truncate(limit as usize);

        let result: Vec<Value> = actionable.iter().map(|n| render_task(n)).collect();
        Ok(Value::Array(result))
    }

    pub(crate) async fn handle_complete(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: CompleteParams = deser(params)?;
        let (mut note, current) = load_task(self.runtime(), p.namespace.as_deref(), &p.id).await?;

        if !can_transition(&current, "done") {
            let allowed = allowed_transitions(&current).join(", ");
            return Err(RuntimeError::InvalidInput(format!(
                "cannot transition from {current:?} to \"done\" — allowed: {allowed}"
            )));
        }

        let mut props = note.properties.take().unwrap_or(json!({}));
        if let Some(obj) = props.as_object_mut() {
            obj.insert("status".into(), json!("done"));
            obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
            if let Some(ref result) = p.result {
                obj.insert("result".into(), json!(result));
            }
        }
        note.properties = Some(props);
        note.updated_at = Utc::now().timestamp_micros();

        self.runtime()
            .notes(p.namespace.as_deref())?
            .upsert_note(note.clone())
            .await
            .map_err(|e| RuntimeError::Internal(format!("upsert_note: {e}")))?;

        Ok(json!({
            "completed": true,
            "id": short_id(note.id),
            "full_id": note.id.as_hyphenated().to_string(),
            "from": current,
            "to": "done",
            "is_terminal": is_terminal("done"),
        }))
    }

    pub(crate) async fn handle_tasks(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: TasksParams = deser(params)?;
        let limit = p.limit.unwrap_or(50).clamp(1, 200);
        let offset = p.offset.unwrap_or(0) as usize;

        // Normalize status filter once.
        let status_filter: Option<String> = match p.status.as_deref() {
            None => None,
            Some(s) => {
                let normalized = normalize_status(s);
                if !is_valid_status(normalized) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "invalid status {s:?} — valid: {}",
                        crate::schema::TASK_STATUSES.join(", ")
                    )));
                }
                Some(normalized.to_string())
            }
        };
        if let Some(ref pri) = p.priority {
            if !is_valid_priority(pri) {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid priority {pri:?} — valid: p0, p1, p2, p3"
                )));
            }
        }

        let window = (offset as u32).saturating_add(limit).saturating_add(500);
        let notes = self
            .runtime()
            .list_notes(p.namespace.as_deref(), Some("task"), window)
            .await?;

        let filtered: Vec<&khive_storage::note::Note> = notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .filter(|n| match status_filter.as_deref() {
                None => true,
                Some(want) => task_status(n.properties.as_ref()) == want,
            })
            .filter(|n| match p.assignee.as_deref() {
                None => true,
                Some(want) => {
                    n.properties
                        .as_ref()
                        .and_then(|v| v.get("assignee"))
                        .and_then(|v| v.as_str())
                        == Some(want)
                }
            })
            .filter(|n| match p.priority.as_deref() {
                None => true,
                Some(want) => n
                    .properties
                    .as_ref()
                    .and_then(|v| v.get("priority"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.eq_ignore_ascii_case(want))
                    .unwrap_or(false),
            })
            .collect();

        let result: Vec<Value> = filtered
            .into_iter()
            .skip(offset)
            .take(limit as usize)
            .map(render_task)
            .collect();
        Ok(Value::Array(result))
    }

    pub(crate) async fn handle_transition(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: TransitionParams = deser(params)?;
        let target = normalize_status(&p.status);
        if !is_valid_status(target) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid status {status:?} — valid: inbox, next, waiting, someday, active, done, cancelled \
                 (aliases: in_progress, todo, blocked, later, finished)",
                status = p.status
            )));
        }

        let (mut note, current) = load_task(self.runtime(), p.namespace.as_deref(), &p.id).await?;

        if current == target {
            // Idempotent — no write, no transition.
            return Ok(json!({
                "transitioned": false,
                "id": short_id(note.id),
                "full_id": note.id.as_hyphenated().to_string(),
                "from": current,
                "to": target,
                "note": "already in target status",
            }));
        }
        if !can_transition(&current, target) {
            let allowed = allowed_transitions(&current).join(", ");
            return Err(RuntimeError::InvalidInput(format!(
                "cannot transition from {current:?} to {target:?} — allowed: {allowed}"
            )));
        }

        let mut props = note.properties.take().unwrap_or(json!({}));
        if let Some(obj) = props.as_object_mut() {
            obj.insert("status".into(), json!(target.to_string()));
            if let Some(ref n) = p.note {
                obj.insert("transition_note".into(), json!(n));
            }
            if target == "done" {
                obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
            }
        }
        note.properties = Some(props);
        note.updated_at = Utc::now().timestamp_micros();

        self.runtime()
            .notes(p.namespace.as_deref())?
            .upsert_note(note.clone())
            .await
            .map_err(|e| RuntimeError::Internal(format!("upsert_note: {e}")))?;

        Ok(json!({
            "transitioned": true,
            "id": short_id(note.id),
            "full_id": note.id.as_hyphenated().to_string(),
            "from": current,
            "to": target,
            "is_terminal": is_terminal(target),
        }))
    }
}
