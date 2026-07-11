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
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};

use crate::schema::{
    allowed_transitions, can_transition, is_actionable, is_terminal, is_valid_priority,
    is_valid_status, normalize_status, TASK_LIFECYCLE_HELP,
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
/// `pub` (rather than the module-private visibility every other helper in
/// this file uses): the ADR-099 `--atomic` CLI surface's `gtd.transition`/
/// `gtd.complete` prepare functions live in `kkernel` (a crate that already
/// depends on both `khive-runtime` and `khive-pack-gtd` — see that crate's
/// `atomic_apply` module doc for the crate-direction rationale), and the B3
/// fix round (GAP-5) applies this exact function as a deferred post-commit
/// effect so atomic transitions/completes write the same best-effort
/// lifecycle audit row the canonical handlers do, rather than re-deriving
/// the DDL/INSERT here a second time.
pub async fn ensure_audit_schema(runtime: &KhiveRuntime) {
    let Ok(mut w) = runtime.sql().writer().await else {
        tracing::warn!("gtd: failed to acquire SQL writer for audit schema (non-fatal)");
        return;
    };
    for stmt in &crate::GTD_SCHEMA_PLAN_STMTS {
        if let Err(e) = w.execute_script(stmt.to_string()).await {
            tracing::warn!(error = %e, stmt, "gtd: failed to apply lifecycle_audit schema stmt (non-fatal)");
        }
    }

    // `CREATE TABLE IF NOT EXISTS` above is a no-op on databases that already
    // have `gtd_lifecycle_audit` from before the `namespace` column existed.
    // Guard-check and upgrade those tables in place so `write_audit_record`'s
    // `INSERT ... namespace` doesn't silently fail on legacy schemas.
    let rows = match w
        .query_all(SqlStatement {
            sql: "PRAGMA table_info(gtd_lifecycle_audit)".into(),
            params: vec![],
            label: Some("gtd_audit_schema_info".into()),
        })
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "gtd: failed to inspect lifecycle_audit schema (non-fatal)");
            return;
        }
    };

    let has_namespace = rows
        .iter()
        .any(|row| matches!(row.get("name"), Some(SqlValue::Text(name)) if name == "namespace"));

    if !has_namespace {
        if let Err(e) = w
            .execute_script("ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace TEXT".into())
            .await
        {
            tracing::warn!(
                error = %e,
                "gtd: failed to add lifecycle_audit.namespace column (non-fatal)"
            );
        }
    }
}

/// Append one row to `gtd_lifecycle_audit`.
///
/// Best-effort: failures are logged and swallowed.  The note's successful
/// write has already happened; a missing audit row is degraded, not a failure.
///
/// `pub` for the same reason as `ensure_audit_schema` above — the ADR-099
/// `--atomic` surface's `kkernel`-side post-commit pass calls this directly
/// (B3 fix round, GAP-5) rather than re-deriving the INSERT.
pub async fn write_audit_record(
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

/// ADR-099 B3: `pub` (not module-private) SPECIFICALLY so `kkernel`'s
/// `--atomic` validation seam (`atomic_apply::validate_atomic_args`) can
/// deserialize an op's args through the SAME canonical struct
/// `handle_complete` uses, reproducing `deny_unknown_fields` rejection with
/// zero duplicated field lists. Fields stay private — the atomic seam only
/// needs the `Result<_, _>` outcome, never field access.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteParams {
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

/// ADR-099 B3: `pub` for the same reason as `CompleteParams` above —
/// reused by the atomic seam to validate `gtd.transition` args.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionParams {
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

/// `pub` (widened from `pub(crate)`, ADR-099 B3 fix round 5, finding 3): the
/// `--atomic` seam in `kkernel` reuses this exact resolver (full UUID or 8+
/// hex prefix, namespace-scoped via `resolve_prefix`) to resolve `gtd.transition`
/// / `gtd.complete` `id` args before atomic prepare, matching what
/// `handle_transition`/`handle_complete` use — reproducing canonical short-id
/// acceptance without a duplicated resolution helper.
pub async fn resolve_uuid(
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
///
/// `pub(crate)` so `task_create::prepare_task_create` (#625/#626 unification)
/// can share this resolver between `gtd.assign` and the generic
/// `create(kind="note", note_kind="task")` path.
pub(crate) async fn resolve_context_entity_id(
    raw: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    let uuid = Uuid::from_str(raw).map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "context_entity_id must be a full UUID; got {raw:?}"
        ))
    })?;

    // Mutation rule: the annotated entity must live in the PRIMARY namespace.
    // A visible-only (foreign) entity returns NotFound here per ADR-007:215-219.
    match runtime.resolve_primary(token, uuid).await? {
        Some(Resolved::Entity(_)) => Ok(uuid),
        Some(Resolved::Note(n)) => Err(RuntimeError::InvalidInput(format!(
            "context_entity_id {uuid} must reference a KG entity; got note kind {:?}",
            n.kind
        ))),
        Some(Resolved::Event(_)) => Err(RuntimeError::InvalidInput(format!(
            "context_entity_id {uuid} must reference a KG entity; got event"
        ))),
        Some(Resolved::PackRecord { pack, kind, .. }) => Err(RuntimeError::InvalidInput(format!(
            "context_entity_id {uuid} must reference a KG entity; got pack-private record \
             (pack={pack:?}, kind={kind:?})"
        ))),
        None => Err(RuntimeError::NotFound(format!(
            "context_entity_id {uuid} not found in namespace"
        ))),
    }
}

/// Status a task is treated as when the `status` property is missing/empty.
/// Property filters that select this value must use `FilterOp::EqOrMissing`
/// (not plain `Eq`) so legacy rows without a stored `status` still match —
/// `json_extract` on an absent path is SQL `NULL`, which never equals a text
/// literal.
const DEFAULT_STATUS: &str = "inbox";

/// Priority a task is treated as when the `priority` property is missing/empty.
/// Same `EqOrMissing` rule as [`DEFAULT_STATUS`] applies.
const DEFAULT_PRIORITY: &str = "p2";

/// Status used internally on a task. Defaults to "inbox" when missing/empty.
fn task_status(props: Option<&Value>) -> String {
    props
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_STATUS)
        .to_string()
}

/// Priority rank used for sorting actionable tasks (lower = higher priority).
/// Unknown / missing priorities sort to "p2" so they don't dominate p0/p1.
fn priority_rank(props: Option<&Value>) -> u8 {
    let raw = props
        .and_then(|p| p.get("priority"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_PRIORITY)
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
///
/// `pub` (widened from private, ADR-099 B3 fix round 5, finding 4): the
/// `--atomic` seam in `kkernel` reuses this exact renderer, post-commit, to
/// build the `result` payload for a committed `gtd.transition`/`gtd.complete`
/// op — matching `handle_transition`/`handle_complete`'s response shape
/// field-for-field without a duplicated renderer.
pub fn render_task(note: &khive_storage::note::Note) -> Value {
    let props = note.properties.clone().unwrap_or(json!({}));
    let title = note
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("[{}]", note.kind.as_str()));
    let status = props
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_STATUS)
        .to_string();
    let priority = props
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_PRIORITY)
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

/// Safety cap on matching rows [`fetch_all_matching_tasks`] will accept in a
/// single snapshot query before refusing the request outright. A query
/// matching more than this many rows is rejected before any priority sort
/// runs — sorting a partial candidate set can hide an older, higher-priority
/// task that fell outside the scan window.
const TASK_SCAN_MAX_ROWS: u32 = 20_000;

/// Fetch every `task` note matching `property_filters` in a single bounded
/// snapshot query, instead of pre-fetching a fixed-size unfiltered window.
/// The predicate is pushed into SQL via `query_notes_filtered_bounded`, so
/// the candidate set this returns is bounded by how many tasks actually
/// match — not by how many task notes of any status exist (the #772 bug: a
/// fixed unfiltered window could be entirely filled by newer non-matching
/// churn, hiding older matching tasks regardless of priority).
///
/// `query_notes_filtered_bounded` fetches at most `TASK_SCAN_MAX_ROWS + 1`
/// rows in one SQL statement with deterministic ordering — one consistent
/// snapshot, not a `COUNT(*)` followed by independent paged reads that a
/// concurrent insert could split across (issue #825 round 2: the prior
/// page-loop version re-queried the store per page with no transaction
/// spanning them, so a row inserted between pages could appear duplicated
/// across a page boundary, or the scan could hit its cap and still return
/// `Ok` with an incomplete set). If `TASK_SCAN_MAX_ROWS + 1` rows come back,
/// this returns `Err(InvalidInput)` instead of ever returning a possibly
/// truncated result — callers must narrow the filters (e.g. add `assignee`)
/// so the result stays complete.
async fn fetch_all_matching_tasks(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    property_filters: Vec<PropertyFilter>,
) -> Result<Vec<khive_storage::note::Note>, RuntimeError> {
    let namespaces = if token.visible_namespaces().len() > 1 {
        token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect()
    } else {
        Vec::new()
    };
    let filter = NoteFilter {
        kind: Some("task".to_string()),
        property_filters,
        namespaces,
        ..Default::default()
    };
    let store = runtime.notes(token)?;

    let notes = store
        .query_notes_filtered_bounded(token.namespace().as_str(), &filter, TASK_SCAN_MAX_ROWS)
        .await
        .map_err(|e| RuntimeError::Internal(format!("query_notes_filtered_bounded: {e}")))?;

    if notes.len() as u32 > TASK_SCAN_MAX_ROWS {
        return Err(RuntimeError::InvalidInput(format!(
            "gtd: more than {TASK_SCAN_MAX_ROWS} tasks match this query, which exceeds the \
             {TASK_SCAN_MAX_ROWS}-row scan bound; narrow the filters \
             (e.g. specify assignee) and retry so results stay complete \
             and priority-ordered instead of being silently truncated"
        )));
    }

    Ok(notes)
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
    let statement =
        gtd_transition_statement(note_id, expected_current, target, new_props, updated_at)?;
    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("sql writer: {e}")))?;
    let affected = writer
        .execute(statement)
        .await
        .map_err(|e| RuntimeError::Internal(format!("atomic transition update: {e}")))?;

    Ok(affected)
}

/// The exact conditional-UPDATE DML `atomic_gtd_transition` issues, as a
/// plain [`SqlStatement`] — the single source of truth shared with the
/// ADR-099 `--atomic` `gtd.transition`/`gtd.complete` prepare functions in
/// `kkernel` (`crate::atomic_apply`, that crate — not this one — since
/// `kkernel` depends on both `khive-runtime` and `khive-pack-gtd`). Canonical
/// executes it immediately via the writer above; the atomic path turns it
/// into a `PlanStatement` for the synchronous commit pass instead.
pub fn gtd_transition_statement(
    note_id: Uuid,
    expected_current: &str,
    target: &str,
    new_props: &serde_json::Value,
    updated_at: i64,
) -> Result<SqlStatement, RuntimeError> {
    let props_str = serde_json::to_string(new_props)
        .map_err(|e| RuntimeError::Internal(format!("serialize props: {e}")))?;
    Ok(SqlStatement {
        sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
              WHERE id = ?3 \
              AND json_extract(properties, '$.status') = ?4 \
              AND deleted_at IS NULL"
            .to_string(),
        params: vec![
            SqlValue::Text(props_str),
            SqlValue::Integer(updated_at),
            SqlValue::Text(note_id.as_hyphenated().to_string()),
            SqlValue::Text(expected_current.to_string()),
        ],
        label: Some(format!("gtd_atomic_transition_{target}")),
    })
}

/// Outcome of [`prepare_transition`]'s decide step: either nothing to write
/// (the idempotent `current == target` case, canonical's early return) or a
/// fully computed patched `properties` value ready to apply — via
/// `atomic_gtd_transition` (canonical, immediate) or
/// [`gtd_transition_statement`] (ADR-099 atomic, deferred to the commit
/// pass).
pub enum TransitionDecision {
    NoOp {
        note: khive_storage::note::Note,
        current: String,
        target: String,
    },
    Write {
        note: khive_storage::note::Note,
        current: String,
        target: String,
        props: Value,
        updated_at: i64,
        transition_note: Option<String>,
    },
}

/// Decide step of `gtd.transition` (ADR-099 B3 r6 second pass): normalizes
/// and validates the target status, secret-gates the caller-supplied
/// transition note, loads the task, and either returns the idempotent no-op
/// case or the fully computed patch — all WITHOUT writing. `GtdPack::
/// handle_transition` and the ADR-099 `--atomic` `gtd.transition` prepare
/// function in `kkernel` both call this ONE function; only the apply
/// mechanism differs.
pub async fn prepare_transition(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw_id: &str,
    raw_status: &str,
    note_arg: Option<&str>,
) -> Result<TransitionDecision, RuntimeError> {
    let target = normalize_status(raw_status);
    if !is_valid_status(target) {
        return Err(RuntimeError::InvalidInput(format!(
            "invalid status {raw_status:?} — valid: inbox, next, waiting, someday, active, done, cancelled \
             (aliases: in_progress, todo, blocked, later, finished)"
        )));
    }
    if let Some(n) = note_arg {
        khive_runtime::secret_gate::check(n)?;
    }

    let (note, current) = load_task(runtime, token, raw_id).await?;

    if current == target {
        return Ok(TransitionDecision::NoOp {
            note,
            current,
            target: target.to_string(),
        });
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

    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = props.as_object_mut() {
        obj.insert("status".into(), json!(target.to_string()));
        if let Some(n) = note_arg {
            obj.insert("transition_note".into(), json!(n));
        }
        if target == "done" {
            obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
        }
    }
    let updated_at = Utc::now().timestamp_micros();

    Ok(TransitionDecision::Write {
        note,
        current,
        target: target.to_string(),
        props,
        updated_at,
        transition_note: note_arg.map(str::to_string),
    })
}

/// Outcome of [`prepare_complete`]'s decide step: the fully computed patched
/// `properties` value ready to apply. Unlike [`TransitionDecision`], there is
/// no idempotent no-op case — `complete()` always writes when it succeeds.
pub struct CompleteDecision {
    pub note: khive_storage::note::Note,
    pub current: String,
    pub target: &'static str,
    pub props: Value,
    pub updated_at: i64,
    pub completed_at: String,
}

/// Decide step of `gtd.complete` (ADR-099 B3 r6 second pass) — same split as
/// [`prepare_transition`] above: validates the target terminal status,
/// secret-gates the caller-supplied result, loads the task, checks the
/// terminal/actionable guards, and computes the patched `properties` value,
/// all WITHOUT writing. `GtdPack::handle_complete` and the ADR-099 `--atomic`
/// `gtd.complete` prepare function in `kkernel` both call this ONE function.
pub async fn prepare_complete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw_id: &str,
    status_arg: Option<&str>,
    result_arg: Option<&str>,
) -> Result<CompleteDecision, RuntimeError> {
    let target = complete_target_status(status_arg)?;

    if let Some(result) = result_arg {
        khive_runtime::secret_gate::check(result)?;
    }

    let (note, current) = load_task(runtime, token, raw_id).await?;

    if is_terminal(&current) {
        return Err(RuntimeError::InvalidInput(format!(
            "task {} is in terminal state {current:?}; no further transitions allowed",
            short_id(note.id)
        )));
    }
    if !is_actionable(&current) {
        return Err(RuntimeError::InvalidInput(format!(
            "complete: task in {current:?}; transition to 'next' or 'active' first, \
             or use transition(status=done) explicitly"
        )));
    }

    let completed_at = Utc::now().to_rfc3339();
    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = props.as_object_mut() {
        obj.insert("status".into(), json!(target));
        obj.insert("completed_at".into(), json!(completed_at));
        if let Some(result) = result_arg {
            obj.insert("result".into(), json!(result));
        }
    }
    let updated_at = Utc::now().timestamp_micros();

    Ok(CompleteDecision {
        note,
        current,
        target,
        props,
        updated_at,
        completed_at,
    })
}

// ── handlers ─────────────────────────────────────────────────────────────────

impl GtdPack {
    pub(crate) async fn handle_assign(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: AssignParams = deser(params)?;

        // #625/#626: `gtd.assign` and the generic `create(kind="note",
        // note_kind="task")` path (`TaskHook`, see hook.rs) share one
        // normalization/validation routine so status/priority checks,
        // dependency-target resolution, and context-entity handling can't
        // drift between the two entry points again.
        let input = crate::task_create::TaskCreateInput {
            title: p.title,
            description: p.description,
            assignee: p.assignee,
            priority: p.priority,
            status: p.status,
            due: p.due,
            start: p.start,
            end: p.end,
            depends_on: p.depends_on,
            context_entity_id: p.context_entity_id,
            tags: p.tags.map(|tags| json!(tags)),
            properties: json!({}),
        };
        let prepared =
            crate::task_create::prepare_task_create(self.runtime(), token, input).await?;

        let note = self
            .runtime()
            .create_note(
                token,
                "task",
                Some(prepared.title.as_str()),
                &prepared.content,
                Some(prepared.salience),
                Some(prepared.properties.clone()),
                prepared.annotates.clone(),
            )
            .await?;

        // Record `depends_on` as graph edges (the GTD pack's `EDGE_RULES` extends
        // the entity-default contract to allow task→task). Endpoints were
        // pre-validated above, so the only way this fails is a storage hiccup
        // after the task is already persisted — log and continue rather than
        // mislead the caller with `ok: false` for a task that's already on disk.
        // The property captures the same dependency information for queries that
        // bypass the graph.
        crate::task_create::link_depends_on_edges(
            self.runtime(),
            token,
            note.id,
            &prepared.properties,
            "assign",
        )
        .await;

        Ok(render_task(&note))
    }

    pub(crate) async fn handle_next(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: NextParams = deser(params)?;
        // #744: this clamp is silent by design here — the response shape is a bare
        // JSON array (`Value::Array`), consumed directly via `.as_array()` by every
        // caller in this crate and beyond (kkernel, li surfaces). Adding a sibling
        // `truncated` field would require wrapping the response in an object, which
        // is a breaking shape change, not an additive one. The cap is documented on
        // the `limit` ParamDef instead (issue #744 fallback ask 1).
        let limit = p.limit.unwrap_or(10).clamp(1, 200);

        // #772: push the actionable-status (+ optional assignee) predicate into
        // SQL via `query_notes_filtered` and scan every matching page, instead
        // of pre-fetching a fixed unfiltered recency window and filtering in
        // Rust — a fixed window silently drops actionable tasks once enough
        // newer non-matching task notes (any status) fill it. The candidate
        // set is now bounded by how many `next`/`active` tasks actually exist.
        let mut property_filters = vec![PropertyFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::In(vec![
                SqlValue::Text("next".to_string()),
                SqlValue::Text("active".to_string()),
            ]),
            value: SqlValue::Null,
        }];
        if let Some(want) = p.assignee.as_deref() {
            property_filters.push(PropertyFilter {
                json_path: "$.assignee".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text(want.to_string()),
            });
        }
        let notes = fetch_all_matching_tasks(self.runtime(), token, property_filters).await?;

        // Build a quick lookup map of task UUID → GTD status so dependency
        // filtering (scenario-gtd C2) can check blocker states in O(1). Every
        // note here already passed the actionable(+assignee) SQL filter above
        // (and `deleted_at IS NULL`, enforced unconditionally by
        // `query_notes_filtered`).
        use std::collections::HashMap;
        let mut status_by_id: HashMap<uuid::Uuid, String> = notes
            .iter()
            .map(|n| (n.id, task_status(n.properties.as_ref())))
            .collect();

        let candidates: Vec<&khive_storage::note::Note> = notes.iter().collect();

        // Gather all dependency UUIDs referenced by candidates that are not
        // already in status_by_id — these are blockers whose status isn't
        // `next`/`active` (the common case: a `done` blocker).  Fetch them in
        // one batch so the dependency filter below can evaluate their status
        // correctly regardless of scan-page position.
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

        // Decide step (ADR-099 B3 r6 second pass): validates the target
        // terminal status, secret-gates the result, loads the task, checks
        // the terminal/actionable guards, and computes the patched
        // `properties` value — the SAME function the ADR-099 `--atomic`
        // `gtd.complete` prepare path in `kkernel` calls.
        let decision = prepare_complete(
            self.runtime(),
            token,
            &p.id,
            p.status.as_deref(),
            p.result.as_deref(),
        )
        .await?;
        let CompleteDecision {
            mut note,
            current,
            target,
            props,
            updated_at,
            completed_at,
        } = decision;
        note.properties = Some(props);
        // notes.status is row-visibility (always "active" for live rows);
        // GTD status lives in properties.status and W1-G's remap surfaces it
        // at data.status in the response.
        note.updated_at = updated_at;

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
        // #744: silent clamp, documented rather than signaled — see the identical
        // note in `handle_next` above (bare-array response shape rules out an
        // additive `truncated` field).
        let limit = p.limit.unwrap_or(50).clamp(1, 200);
        let offset = p.offset.unwrap_or(0);

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

        // #772: push status/assignee/priority predicates into SQL via
        // `query_notes_filtered` and use its real `PageRequest{limit, offset}`
        // for pagination. The previous `list_notes(..., window, 0)` always
        // refetched from offset 0 and grew an unfiltered window by a fixed
        // +500 fudge factor — a fixed number of newer non-matching tasks could
        // still hide older matches (e.g. `tasks(status="done")` returning
        // empty even though done tasks exist), and deep pages re-scanned the
        // same rows since the underlying fetch offset never advanced.
        //
        // When no status= is provided, exclude terminal states (done,
        // cancelled) so the default listing shows only active work, while
        // still counting a task with no `status` property yet as `inbox`
        // (non-terminal, included) — hence `NotInOrMissing` rather than `Ne`,
        // which would silently drop rows where `$.status` is absent.
        let mut property_filters = vec![match status_filter.as_deref() {
            Some(want) => PropertyFilter {
                json_path: "$.status".to_string(),
                // A legacy task with no stored `status` property is treated as
                // `inbox` everywhere else in this pack (`task_status`,
                // `render_task`). `json_extract` on an absent path is SQL
                // NULL, which `Eq` never matches, so an explicit
                // `status="inbox"` query would silently exclude those rows —
                // `EqOrMissing` restores the "absent counts as default" rule.
                op: if want == DEFAULT_STATUS {
                    FilterOp::EqOrMissing
                } else {
                    FilterOp::Eq
                },
                value: SqlValue::Text(want.to_string()),
            },
            None => PropertyFilter {
                json_path: "$.status".to_string(),
                op: FilterOp::NotInOrMissing(vec![
                    SqlValue::Text("done".to_string()),
                    SqlValue::Text("cancelled".to_string()),
                ]),
                value: SqlValue::Null,
            },
        }];
        if let Some(want) = p.assignee.as_deref() {
            property_filters.push(PropertyFilter {
                json_path: "$.assignee".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text(want.to_string()),
            });
        }
        if let Some(want) = p.priority.as_deref() {
            // Priorities are always stored lowercase (`task_create`/
            // `prepare_transition` normalize via `to_ascii_lowercase`), so an
            // exact-match SQL predicate on the lowercased input reproduces
            // the prior `eq_ignore_ascii_case` behavior. A legacy task with no
            // stored `priority` renders as `p2` (`priority_rank`,
            // `render_task`), so `priority="p2"` must also match the missing
            // case via `EqOrMissing` — plain `Eq` never matches SQL NULL.
            let want = want.to_ascii_lowercase();
            property_filters.push(PropertyFilter {
                json_path: "$.priority".to_string(),
                op: if want == DEFAULT_PRIORITY {
                    FilterOp::EqOrMissing
                } else {
                    FilterOp::Eq
                },
                value: SqlValue::Text(want),
            });
        }

        let namespaces = if token.visible_namespaces().len() > 1 {
            token
                .visible_namespaces()
                .iter()
                .map(|ns| ns.as_str().to_owned())
                .collect()
        } else {
            Vec::new()
        };
        let filter = NoteFilter {
            kind: Some("task".to_string()),
            property_filters,
            namespaces,
            ..Default::default()
        };
        let page = self
            .runtime()
            .notes(token)?
            .query_notes_filtered(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    limit,
                    offset: offset.into(),
                },
            )
            .await
            .map_err(|e| RuntimeError::Internal(format!("query_notes_filtered: {e}")))?;

        let result: Vec<Value> = page.items.iter().map(render_task).collect();
        Ok(Value::Array(result))
    }

    pub(crate) async fn handle_transition(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TransitionParams = deser(params)?;

        // Decide step (ADR-099 B3 r6 second pass): normalizes/validates the
        // target status, secret-gates the transition note, loads the task,
        // and either returns the idempotent no-op case or the fully computed
        // patch — the SAME function the ADR-099 `--atomic` `gtd.transition`
        // prepare path in `kkernel` calls.
        let decision =
            prepare_transition(self.runtime(), token, &p.id, &p.status, p.note.as_deref()).await?;

        let (note, current, target) = match decision {
            TransitionDecision::NoOp {
                note,
                current,
                target,
            } => {
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
            TransitionDecision::Write {
                mut note,
                current,
                target,
                props,
                updated_at,
                transition_note,
            } => {
                note.properties = Some(props);
                // notes.status is row-visibility (always "active" for live
                // rows); GTD status lives in properties.status and W1-G's
                // remap surfaces it at data.status in the response.
                note.updated_at = updated_at;

                // ue-dsl-parallel C2: atomic transition — conditional SQL
                // UPDATE so concurrent transitions in the same parallel
                // batch only one wins.
                let rows_affected = atomic_gtd_transition(
                    self.runtime(),
                    note.id,
                    &current,
                    &target,
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
                    &target,
                    transition_note.as_deref(),
                    token.namespace().as_str(),
                )
                .await;

                (note, current, target)
            }
        };

        let task = render_task(&note);
        Ok(json!({
            "transitioned": true,
            "id": task["id"],
            "full_id": task["full_id"],
            "from": current,
            "to": target,
            "is_terminal": is_terminal(&target),
            "title": task["title"],
            "priority": task["priority"],
            "assignee": task["assignee"],
            "due": task["due"],
        }))
    }
}
