//! Verb handlers for the GTD pack.
//!
//! Each handler: deserialize params from Value → validate → mutate via runtime
//! → serialize a stable response shape (`id` short hex + `full_id` UUID).

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, Resolved, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::EdgeRelation;

use crate::schema::{
    allowed_transitions, can_transition, is_actionable, is_terminal, is_valid_priority,
    is_valid_status, normalize_status, priority_to_salience,
};
use crate::GtdPack;

// ── lifecycle audit schema (ADR-019 §schema_plan) ───────────────────────────

/// Ensure `gtd_lifecycle_audit` and its index exist on the given runtime.
///
/// Idempotent (`CREATE TABLE IF NOT EXISTS`).  Applied lazily on the first
/// `transition` or `complete` call.  Logs a warning and continues if the DDL
/// fails (e.g. read-only replica) — the audit is best-effort, not load-bearing.
///
/// We intentionally apply the DDL on each call rather than using a global
/// `OnceLock`, because each `KhiveRuntime::memory()` in tests creates a fresh
/// in-memory database that needs its own schema bootstrap.  In production the
/// DDL is idempotent and cheap (SQLite skips `IF NOT EXISTS` tables instantly).
async fn ensure_audit_schema(runtime: &KhiveRuntime) {
    let script = crate::GTD_SCHEMA_PLAN_STMTS.join(";");
    match runtime.sql().writer().await {
        Ok(mut w) => {
            if let Err(e) = w.execute_script(script).await {
                tracing::warn!(error = %e, "gtd: failed to apply lifecycle_audit schema (non-fatal)");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "gtd: failed to acquire SQL writer for audit schema (non-fatal)");
        }
    }
}

/// Append one row to `gtd_lifecycle_audit`.
///
/// Best-effort: failures are logged and swallowed.  The note's successful
/// write has already happened; a missing audit row is degraded, not a failure.
async fn write_audit_record(
    runtime: &KhiveRuntime,
    note_id: Uuid,
    from: &str,
    to: &str,
    transition_note: Option<&str>,
) {
    let now = Utc::now().timestamp_micros();
    let stmt = SqlStatement {
        sql: "INSERT INTO gtd_lifecycle_audit (note_id, from_state, to_state, note, at) \
              VALUES (?1, ?2, ?3, ?4, ?5)"
            .into(),
        params: vec![
            SqlValue::Text(note_id.as_hyphenated().to_string()),
            SqlValue::Text(from.to_string()),
            SqlValue::Text(to.to_string()),
            match transition_note {
                Some(n) => SqlValue::Text(n.to_string()),
                None => SqlValue::Null,
            },
            SqlValue::Integer(now),
        ],
        label: Some("gtd_audit".into()),
    };
    match runtime.sql().writer().await {
        Ok(mut w) => {
            if let Err(e) = w.execute(stmt).await {
                tracing::warn!(
                    note_id = %note_id,
                    from,
                    to,
                    error = %e,
                    "gtd: audit write failed (non-fatal)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                note_id = %note_id,
                error = %e,
                "gtd: failed to acquire SQL writer for audit write (non-fatal)"
            );
        }
    }
}

// ── param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AssignParams {
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
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    assignee: Option<String>,
}

#[derive(Deserialize)]
struct CompleteParams {
    id: String,
    #[serde(default)]
    result: Option<String>,
}

#[derive(Deserialize)]
struct TasksParams {
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
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(token, s).await? {
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

/// Parse a user-supplied due-date string as an ISO-8601 / RFC 3339 timestamp.
///
/// Accepts full RFC 3339 (e.g. `2026-06-01T00:00:00Z`) or date-only
/// (e.g. `2026-06-01`) by appending midnight UTC if necessary.
/// Returns the canonical RFC 3339 string stored in `properties.due`.
/// Shared with `hook.rs`.
pub(crate) fn parse_due(value: &str) -> Result<String, RuntimeError> {
    // Try full RFC 3339 / ISO-8601 with time zone first.
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc).to_rfc3339());
    }
    // Fallback: try date-only "YYYY-MM-DD", treat as midnight UTC.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let dt = date
            .and_hms_opt(0, 0, 0)
            .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                "due must be ISO-8601 (e.g., 2026-06-01T00:00:00Z or 2026-06-01); got {value:?}"
            ))
            })?;
        return Ok(dt.to_rfc3339());
    }
    Err(RuntimeError::InvalidInput(format!(
        "due must be ISO-8601 (e.g., 2026-06-01T00:00:00Z or 2026-06-01); got {value:?}"
    )))
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
    token: &NamespaceToken,
    raw_id: &str,
) -> Result<(khive_storage::note::Note, String), RuntimeError> {
    let uuid = resolve_uuid(raw_id, runtime, token).await?;
    let ns = token.namespace().as_str();
    let store = runtime.notes(token)?;
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
    pub(crate) async fn handle_assign(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
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
        if is_terminal(status) {
            return Err(RuntimeError::InvalidInput(format!(
                "cannot create task in terminal state {status:?}; \
                 use one of: inbox, next, waiting, someday, active"
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
                resolved_deps.push(resolve_uuid(raw, self.runtime(), token).await?);
            }
        }

        // Pre-validate each dependency target before any storage write. The GTD
        // pack's ADR-031 edge rule only allows `depends_on` between two task
        // notes — if a caller passes a non-task UUID, fail upfront so we don't
        // leave an orphaned task row whose post-write `link` is rejected (the
        // ADR-030 contract makes after_create non-propagating, so propagating a
        // link failure here would diverge `assign` from `create(note_kind="task")`
        // and violate the "no failure after successful write" rule).
        for dep_uuid in &resolved_deps {
            match self.runtime().resolve(token, *dep_uuid).await? {
                Some(Resolved::Note(n)) if n.kind == "task" => {}
                Some(Resolved::Note(n)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "depends_on target {dep_uuid} must be a task note for relation depends_on \
                         (got note kind {:?}); the GTD pack's ADR-031 edge rule is task→task only",
                        n.kind
                    )));
                }
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "depends_on target {dep_uuid} must be a task note for relation depends_on \
                         (got non-note substrate); the GTD pack's ADR-031 edge rule is task→task only"
                    )));
                }
                None => {
                    return Err(RuntimeError::NotFound(format!(
                        "depends_on target {dep_uuid} not found in namespace"
                    )));
                }
            }
        }

        // Always persist priority (defaults to "p2") so listing filters can
        // match defaulted tasks via `properties.priority`. The render layer
        // already shows "p2" for unset priority, so making it explicit on
        // disk keeps render / sort / filter aligned.
        let priority = p
            .priority
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "p2".to_string());

        let mut props = json!({
            "status": status.to_string(),
            "priority": priority,
        });
        if let Some(ref desc) = p.description {
            props["description"] = json!(desc);
        }
        if let Some(ref assignee) = p.assignee {
            props["assignee"] = json!(assignee);
        }
        if let Some(ref due) = p.due {
            props["due"] = json!(parse_due(due)?);
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
                token,
                "task",
                Some(p.title.as_str()),
                &content,
                Some(salience),
                Some(props),
                Vec::new(),
            )
            .await?;

        // Record `depends_on` as `depends_on` graph edges (ADR-031: the GTD
        // pack's `EDGE_RULES` extends the entity-default contract to allow
        // task→task here). Endpoints were pre-validated above, so the only way
        // this fails is a storage hiccup after the task is already persisted —
        // per ADR-030, log and continue rather than mislead the caller with
        // `ok: false` for a task that's already on disk. The property captures
        // the same dependency information for queries that bypass the graph.
        for dep_uuid in resolved_deps {
            if let Err(e) = self
                .runtime()
                .link(token, note.id, dep_uuid, EdgeRelation::DependsOn, 1.0, None)
                .await
            {
                tracing::warn!(
                    from = %note.id,
                    to = %dep_uuid,
                    error = %e,
                    "assign: depends_on edge failed after task write (non-fatal, ADR-030)"
                );
            }
        }

        Ok(render_task(&note))
    }

    pub(crate) async fn handle_next(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: NextParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).clamp(1, 200);

        // Pull a broad window of recent tasks, filter in-memory by GTD status.
        // 500 covers typical inbox/next/active backlogs without paging.
        let notes = self
            .runtime()
            .list_notes(token, Some("task"), 500, 0)
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

    pub(crate) async fn handle_complete(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: CompleteParams = deser(params)?;
        let (mut note, current) = load_task(self.runtime(), token, &p.id).await?;

        if is_terminal(&current) {
            return Err(RuntimeError::InvalidInput(format!(
                "task {} is in terminal state {current:?}; no further transitions allowed",
                short_id(note.id)
            )));
        }
        if !can_transition(&current, "done") {
            let allowed = allowed_transitions(&current).join(", ");
            return Err(RuntimeError::InvalidInput(format!(
                "cannot transition from {current:?} to \"done\" — allowed: {allowed}"
            )));
        }

        let completed_at = Utc::now().to_rfc3339();
        let mut props = note.properties.take().unwrap_or(json!({}));
        if let Some(obj) = props.as_object_mut() {
            obj.insert("status".into(), json!("done"));
            obj.insert("completed_at".into(), json!(completed_at));
            if let Some(ref result) = p.result {
                obj.insert("result".into(), json!(result));
            }
        }
        note.properties = Some(props);
        // notes.status is row-visibility (always "active" for live rows);
        // GTD status lives in properties.status and W1-G's remap surfaces it
        // at data.status in the response.
        note.updated_at = Utc::now().timestamp_micros();

        self.runtime()
            .notes(token)?
            .upsert_note(note.clone())
            .await
            .map_err(|e| RuntimeError::Internal(format!("upsert_note: {e}")))?;

        // ADR-019: write lifecycle audit record (best-effort).
        ensure_audit_schema(self.runtime()).await;
        write_audit_record(self.runtime(), note.id, &current, "done", None).await;

        Ok(json!({
            "completed": true,
            "id": short_id(note.id),
            "full_id": note.id.as_hyphenated().to_string(),
            "from": current,
            "to": "done",
            "completed_at": completed_at,
            "is_terminal": is_terminal("done"),
        }))
    }

    pub(crate) async fn handle_tasks(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
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
            .list_notes(token, Some("task"), window, 0)
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

    pub(crate) async fn handle_transition(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TransitionParams = deser(params)?;
        let target = normalize_status(&p.status);
        if !is_valid_status(target) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid status {status:?} — valid: inbox, next, waiting, someday, active, done, cancelled \
                 (aliases: in_progress, todo, blocked, later, finished)",
                status = p.status
            )));
        }

        let (mut note, current) = load_task(self.runtime(), token, &p.id).await?;

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
        if is_terminal(&current) {
            return Err(RuntimeError::InvalidInput(format!(
                "task {} is in terminal state {current:?}; no further transitions allowed",
                short_id(note.id)
            )));
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
        // notes.status is row-visibility (always "active" for live rows);
        // GTD status lives in properties.status and W1-G's remap surfaces it
        // at data.status in the response.
        note.updated_at = Utc::now().timestamp_micros();

        self.runtime()
            .notes(token)?
            .upsert_note(note.clone())
            .await
            .map_err(|e| RuntimeError::Internal(format!("upsert_note: {e}")))?;

        // ADR-019 + ADR-101: write lifecycle audit record (best-effort).
        ensure_audit_schema(self.runtime()).await;
        write_audit_record(self.runtime(), note.id, &current, target, p.note.as_deref()).await;

        let task = render_task(&note);
        Ok(json!({
            "transitioned": true,
            "id": task["id"],
            "full_id": task["full_id"],
            "from": current,
            "to": target,
            "is_terminal": is_terminal(target),
            "title": task["title"],
            "priority": task["priority"],
            "assignee": task["assignee"],
            "due": task["due"],
        }))
    }
}
