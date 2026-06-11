//! Verb handlers for the GTD pack.
//!
//! Each handler: deserialize params from Value → validate → mutate via runtime
//! → serialize a stable response shape (`id` short hex + `full_id` UUID).
//!
//! FILE SIZE JUSTIFICATION: All five GTD verb handlers (`assign`, `next`, `complete`,
//! `tasks`, `transition`) share internal helpers (`load_task`, `atomic_gtd_transition`,
//! `ensure_audit_schema`, `write_audit_record`) that access `pub(crate)` symbols and
//! must stay co-located to avoid circular imports within the crate. Splitting by verb
//! would require either making those helpers `pub` (which widens the API surface) or
//! duplicating them. The file is reviewed against this invariant at each significant
//! change; see docs/design.md for the GTD lifecycle contract.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, Resolved, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::EdgeRelation;

use crate::schema::{
    allowed_transitions, can_transition, is_actionable, is_terminal, is_valid_priority,
    is_valid_status, normalize_status, priority_to_salience, TASK_LIFECYCLE_HELP,
};
use crate::GtdPack;

// ── lifecycle audit schema ────────────────────────────────────────────────────

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
    let Ok(mut w) = runtime.sql().writer().await else {
        tracing::warn!("gtd: failed to acquire SQL writer for audit schema (non-fatal)");
        return;
    };
    for stmt in &crate::GTD_SCHEMA_PLAN_STMTS {
        if let Err(e) = w.execute_script(stmt.to_string()).await {
            tracing::warn!(error = %e, stmt, "gtd: failed to apply lifecycle_audit schema stmt (non-fatal)");
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
    namespace: &str,
) {
    let now = Utc::now().timestamp_micros();
    let stmt = SqlStatement {
        sql: "INSERT INTO gtd_lifecycle_audit \
              (note_id, from_state, to_state, note, at, namespace) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
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
            SqlValue::Text(namespace.to_string()),
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

// ue-errors C1 (cross-pack): deny_unknown_fields so typo kwargs are rejected
// at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    context_entity_id: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NextParams {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    assignee: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteParams {
    id: String,
    #[serde(default)]
    result: Option<String>,
    /// CC-1: honor `status` param — accepts "done" (default) or "cancelled".
    /// Silently ignoring an explicit status arg is the worst outcome for callers
    /// who follow the MCP server hint "complete() defaults to 'done'; pass
    /// status='cancelled' for cancellation."
    #[serde(default)]
    status: Option<String>,
}

/// CC-1 helper: validate the target terminal status for `complete()`.
/// Returns the canonical target (`"done"` or `"cancelled"`) or an error.
fn complete_target_status(status: Option<&str>) -> Result<&'static str, RuntimeError> {
    match status {
        None | Some("done") => Ok("done"),
        Some("cancelled") => Ok("cancelled"),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "complete: status must be \"done\" or \"cancelled\"; got {other:?}"
        ))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Validate `context_entity_id`: must be a full UUID that resolves to a KG entity.
/// Rejects short prefixes intentionally — prefix resolution would silently canonicalize
/// a field meant to preserve an explicit, stable KG entity ID.
async fn resolve_context_entity_id(
    raw: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    let uuid = Uuid::from_str(raw).map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "context_entity_id must be a full UUID; got {raw:?}"
        ))
    })?;

    match runtime.resolve(token, uuid).await? {
        Some(Resolved::Entity(_)) => Ok(uuid),
        Some(Resolved::Note(n)) => Err(RuntimeError::InvalidInput(format!(
            "context_entity_id {uuid} must reference a KG entity; got note kind {:?}",
            n.kind
        ))),
        Some(Resolved::Event(_)) => Err(RuntimeError::InvalidInput(format!(
            "context_entity_id {uuid} must reference a KG entity; got event"
        ))),
        None => Err(RuntimeError::NotFound(format!(
            "context_entity_id {uuid} not found in namespace"
        ))),
    }
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
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("[{}]", note.kind.as_str()));
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
    let context_entity_id = props
        .get("context_entity_id")
        .cloned()
        .unwrap_or(Value::Null);
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
        "context_entity_id": context_entity_id,
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
    micros_to_iso(micros)
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

// ── atomic GTD transition (ue-dsl-parallel C2) ──────────────────────────────

/// Perform an atomic conditional UPDATE on a task's properties, transitioning it
/// from `expected_current` to `target` status.
///
/// Relies on SQLite's atomic single-statement UPDATE plus a conditional WHERE
/// predicate (`json_extract(properties,'$.status') = ?`) so that concurrent
/// `complete()` or `transition()` calls on the same task in a parallel batch do
/// NOT both report success. Only one write wins; the other gets 0 rows affected
/// and must report an error.
///
/// Returns the number of rows updated (1 = success, 0 = lost race / already moved).
async fn atomic_gtd_transition(
    runtime: &KhiveRuntime,
    note_id: Uuid,
    expected_current: &str,
    target: &str,
    new_props: &serde_json::Value,
    updated_at: i64,
) -> Result<u64, RuntimeError> {
    let props_str = serde_json::to_string(new_props)
        .map_err(|e| RuntimeError::Internal(format!("serialize props: {e}")))?;
    let id_str = note_id.as_hyphenated().to_string();
    let target_owned = target.to_string();
    let current_owned = expected_current.to_string();

    // The conditional UPDATE runs as a single SQLite statement, which is atomic
    // on its own — no explicit transaction is needed because we never split the
    // read-check from the write. The WHERE predicate goes through json_extract
    // on the properties column to check the GTD status rather than the
    // row-visibility `status` column (which is always "active").
    //
    // Concurrency: if another writer has already written `target` (or any other
    // terminal state) by the time the WHERE predicate is evaluated, the predicate
    // fails and rows_affected = 0. Caller distinguishes the rows-affected-0 loser
    // path from the pre-load terminal-state error returned by `load_task`.
    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("sql writer: {e}")))?;
    let affected = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
                  WHERE id = ?3 \
                  AND json_extract(properties, '$.status') = ?4 \
                  AND deleted_at IS NULL"
                .to_string(),
            params: vec![
                SqlValue::Text(props_str),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id_str),
                SqlValue::Text(current_owned),
            ],
            label: Some(format!("gtd_atomic_transition_{target_owned}")),
        })
        .await
        .map_err(|e| RuntimeError::Internal(format!("atomic transition update: {e}")))?;

    Ok(affected)
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
        // pack edge rule only allows `depends_on` between two task notes — if a
        // caller passes a non-task UUID, fail upfront so we don't leave an orphaned
        // task row whose post-write `link` is rejected (after_create is
        // non-propagating, so propagating a link failure here would diverge `assign`
        // from `create(note_kind="task")` and violate the "no failure after
        // successful write" rule).
        for dep_uuid in &resolved_deps {
            match self.runtime().resolve(token, *dep_uuid).await? {
                Some(Resolved::Note(n)) if n.kind == "task" => {}
                Some(Resolved::Note(n)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "depends_on target {dep_uuid} must be a task note for relation depends_on \
                         (got note kind {:?}); the GTD pack edge rule is task→task only",
                        n.kind
                    )));
                }
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "depends_on target {dep_uuid} must be a task note for relation depends_on \
                         (got non-note substrate); the GTD pack edge rule is task→task only"
                    )));
                }
                None => {
                    return Err(RuntimeError::NotFound(format!(
                        "depends_on target {dep_uuid} not found in namespace"
                    )));
                }
            }
        }

        let context_entity_uuid = match p.context_entity_id.as_deref() {
            Some(raw) => Some(resolve_context_entity_id(raw, self.runtime(), token).await?),
            None => None,
        };

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
        if let Some(uuid) = context_entity_uuid.as_ref() {
            props["context_entity_id"] = json!(uuid.as_hyphenated().to_string());
        }
        if let Some(ref tags) = p.tags {
            props["tags"] = json!(tags);
        }

        // Content body powers semantic search; title doubles as the searchable text
        // when no description is supplied.
        let content = p.description.clone().unwrap_or_else(|| p.title.clone());

        let annotates: Vec<Uuid> = context_entity_uuid.iter().copied().collect();
        let note = self
            .runtime()
            .create_note(
                token,
                "task",
                Some(p.title.as_str()),
                &content,
                Some(salience),
                Some(props),
                annotates,
            )
            .await?;

        // Record `depends_on` as graph edges (the GTD pack's `EDGE_RULES` extends
        // the entity-default contract to allow task→task). Endpoints were
        // pre-validated above, so the only way this fails is a storage hiccup
        // after the task is already persisted — log and continue rather than
        // mislead the caller with `ok: false` for a task that's already on disk.
        // The property captures the same dependency information for queries that
        // bypass the graph.
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
                    "assign: depends_on edge failed after task write (non-fatal, best-effort)"
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

        // Build a quick lookup map of task UUID → GTD status so dependency
        // filtering (scenario-gtd C2) can check blocker states in O(1).
        use std::collections::HashMap;
        let mut status_by_id: HashMap<uuid::Uuid, String> = notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .map(|n| (n.id, task_status(n.properties.as_ref())))
            .collect();

        // Collect actionable candidates (before dependency check) so we can
        // determine which blocker UUIDs are outside the 500-task window.
        let candidates: Vec<&khive_storage::note::Note> = notes
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

        // Gather all dependency UUIDs referenced by candidates that are not
        // already in status_by_id — these are blockers older than the 500-task
        // scan window.  Fetch them in one batch so the dependency filter below
        // can evaluate their status correctly regardless of window position.
        let missing_dep_ids: Vec<uuid::Uuid> = candidates
            .iter()
            .flat_map(|n| {
                n.properties
                    .as_ref()
                    .and_then(|p| p.get("depends_on"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| uuid::Uuid::parse_str(v.as_str().unwrap_or("")).ok())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .filter(|id| !status_by_id.contains_key(id))
            .collect();

        if !missing_dep_ids.is_empty() {
            let ns = token.namespace().as_str();
            let fetched = self
                .runtime()
                .notes(token)?
                .get_notes_batch(&missing_dep_ids)
                .await
                .map_err(|e| RuntimeError::Internal(format!("get_notes_batch: {e}")))?;
            for n in fetched {
                // Enforce namespace isolation: ignore notes from other tenants.
                if n.namespace == ns {
                    status_by_id.insert(n.id, task_status(n.properties.as_ref()));
                }
            }
        }

        // scenario-gtd C2: exclude tasks whose `depends_on` contains any
        // blocker that is NOT in the `done` terminal state.
        // Dangling UUIDs (not found in status_by_id even after batch fetch)
        // are treated as incomplete (blocker unknown = not done → keep blocked).
        let mut actionable: Vec<&khive_storage::note::Note> = candidates
            .into_iter()
            .filter(|n| {
                let deps = n
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("depends_on"))
                    .and_then(|v| v.as_array());
                match deps {
                    None => true, // no dependencies → not blocked
                    Some(arr) if arr.is_empty() => true,
                    Some(arr) => arr.iter().all(|dep| {
                        let dep_str = dep.as_str().unwrap_or("");
                        let dep_uuid = uuid::Uuid::parse_str(dep_str).ok();
                        match dep_uuid.and_then(|id| status_by_id.get(&id)) {
                            Some(s) => s == "done",
                            // Dep not found or non-UUID → treat as blocked.
                            None => false,
                        }
                    }),
                }
            })
            .collect();

        // Sort: priority ascending (p0 first), then created_at descending (recent first),
        // then UUID ascending as a deterministic tie-breaker for equal-priority equal-timestamp
        // tasks so callers always observe a stable ordering.
        actionable.sort_by(|a, b| {
            let ap = priority_rank(a.properties.as_ref());
            let bp = priority_rank(b.properties.as_ref());
            ap.cmp(&bp)
                .then(b.created_at.cmp(&a.created_at))
                .then(a.id.cmp(&b.id))
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

        // CC-1: validate the target terminal status before any DB work.
        // Accepts "done" (default) or "cancelled"; rejects anything else.
        let target = complete_target_status(p.status.as_deref())?;

        let (mut note, current) = load_task(self.runtime(), token, &p.id).await?;

        if is_terminal(&current) {
            return Err(RuntimeError::InvalidInput(format!(
                "task {} is in terminal state {current:?}; no further transitions allowed",
                short_id(note.id)
            )));
        }
        // UE2-H1: complete() is restricted to actionable states (next, active) only.
        // Tasks in inbox/waiting/someday must be explicitly transitioned to an
        // actionable state first. Use transition(status=done) to bypass this check.
        if !is_actionable(&current) {
            return Err(RuntimeError::InvalidInput(format!(
                "complete: task in {current:?}; transition to 'next' or 'active' first, \
                 or use transition(status=done) explicitly"
            )));
        }

        let completed_at = Utc::now().to_rfc3339();
        let mut props = note.properties.take().unwrap_or(json!({}));
        if let Some(obj) = props.as_object_mut() {
            // CC-1: write the caller-supplied target (done or cancelled).
            obj.insert("status".into(), json!(target));
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

        // ue-dsl-parallel C2: atomic transition — use a conditional SQL UPDATE
        // so that a concurrent complete() on the same task loses the race
        // cleanly rather than both reporting success.
        let rows_affected = atomic_gtd_transition(
            self.runtime(),
            note.id,
            &current,
            target,
            note.properties.as_ref().unwrap(),
            note.updated_at,
        )
        .await?;

        if rows_affected == 0 {
            // Another concurrent op already transitioned this task away from `current`.
            // Re-read the actual current state to give a precise error.
            let (_, actual_now) = load_task(self.runtime(), token, &p.id).await?;
            let message = if is_terminal(&actual_now) {
                format!(
                    "task {} is in terminal state {actual_now:?}; no further transitions allowed",
                    short_id(note.id)
                )
            } else {
                format!(
                    "complete: task {} changed from expected state {current:?} to {actual_now:?}; retry with fresh state",
                    short_id(note.id)
                )
            };
            return Err(RuntimeError::InvalidInput(message));
        }

        // Write lifecycle audit record (best-effort).
        ensure_audit_schema(self.runtime()).await;
        write_audit_record(
            self.runtime(),
            note.id,
            &current,
            target,
            None,
            token.namespace().as_str(),
        )
        .await;

        Ok(json!({
            "completed": true,
            "id": short_id(note.id),
            "full_id": note.id.as_hyphenated().to_string(),
            "from": current,
            "to": target,
            "completed_at": completed_at,
            "is_terminal": is_terminal(target),
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

        // When no status= is provided, exclude terminal states (done, cancelled)
        // so the default listing shows only active work. Pass status= explicitly
        // to see terminal tasks (e.g. tasks(status="done") for review).
        let filtered: Vec<&khive_storage::note::Note> = notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .filter(|n| match status_filter.as_deref() {
                None => !is_terminal(&task_status(n.properties.as_ref())),
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
        // Secret gate: scan the caller-supplied transition note before any write.
        if let Some(ref n) = p.note {
            khive_runtime::secret_gate::check(n)?;
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
            let allowed = allowed_transitions(&current);
            let allowed_display = if allowed.is_empty() {
                "(none)".to_string()
            } else {
                allowed.join(", ")
            };
            return Err(RuntimeError::InvalidInput(format!(
                "cannot transition from {current:?} to {target:?}; \
                 allowed from {current:?}: {allowed_display}. Full lifecycle: {TASK_LIFECYCLE_HELP}"
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

        // ue-dsl-parallel C2: atomic transition — conditional SQL UPDATE so
        // concurrent transitions in the same parallel batch only one wins.
        let rows_affected = atomic_gtd_transition(
            self.runtime(),
            note.id,
            &current,
            target,
            note.properties.as_ref().unwrap(),
            note.updated_at,
        )
        .await?;

        if rows_affected == 0 {
            let (_, actual_now) = load_task(self.runtime(), token, &p.id).await?;
            return Err(RuntimeError::InvalidInput(format!(
                "task {} is in terminal state {actual_now:?}; no further transitions allowed",
                short_id(note.id)
            )));
        }

        // Write lifecycle audit record (best-effort).
        ensure_audit_schema(self.runtime()).await;
        write_audit_record(
            self.runtime(),
            note.id,
            &current,
            target,
            p.note.as_deref(),
            token.namespace().as_str(),
        )
        .await;

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
