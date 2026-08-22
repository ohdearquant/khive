//! Verb handlers for the GTD pack.
//!
//! Each handler: deserialize params from Value → validate → mutate via runtime
//! → serialize a stable response shape (`id` short hex + `full_id` UUID).
//!
//! FILE SIZE JUSTIFICATION: All five GTD verb handlers (`assign`, `next`, `complete`,
//! `tasks`, `transition`) share internal helpers (`load_task`, `atomic_gtd_transition`,
//! `ensure_audit_schema`, `write_audit_record_with_status`) that access `pub(crate)` symbols and
//! must stay co-located to avoid circular imports within the crate. Splitting by verb
//! would require either making those helpers `pub` (which widens the API surface) or
//! duplicating them. The file is reviewed against this invariant at each significant
//! change; see docs/design.md for the GTD lifecycle contract.

use std::str::FromStr;

use chrono::{DateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, Resolved, RuntimeError};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};

use crate::schema::{
    allowed_transitions, can_transition, is_terminal, is_valid_priority, is_valid_status,
    normalize_status, TASK_LIFECYCLE_HELP,
};
use crate::GtdPack;

// ── lifecycle audit schema ────────────────────────────────────────────────────

/// Ensure `gtd_lifecycle_audit` and its index exist on the given runtime.
///
/// Idempotent (`CREATE TABLE IF NOT EXISTS`). Applied lazily on the first
/// `transition` or `complete` call, on every call rather than gated by a
/// `OnceLock` (fresh in-memory test runtimes each need their own bootstrap).
/// Logs a warning and continues if the DDL fails (e.g. read-only replica) —
/// the audit is best-effort, not load-bearing. `pub`: also called from
/// `kkernel`'s ADR-099 `--atomic` seam. See
/// `docs/api/lifecycle-audit.md#ensure_audit_schema--why-per-call-not-oncelock`.
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
    // Guard-check and upgrade those tables in place so the audit writer's
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

/// Append one row to `gtd_lifecycle_audit`, preserving the original public
/// unit-returning API.
///
/// Best-effort failures remain logged and swallowed for compatibility. New
/// lifecycle response builders call [`write_audit_record_with_status`] when
/// they need to expose degradation to the caller.
pub async fn write_audit_record(
    runtime: &KhiveRuntime,
    note_id: Uuid,
    from: &str,
    to: &str,
    transition_note: Option<&str>,
    namespace: &str,
) {
    let _ = write_audit_record_with_status(runtime, note_id, from, to, transition_note, namespace)
        .await;
}

/// Append one lifecycle-audit row and report whether it persisted.
///
/// The task mutation has already committed, so failure remains non-fatal;
/// callers use the boolean to expose that degraded side effect without
/// changing [`write_audit_record`]'s public return contract.
pub async fn write_audit_record_with_status(
    runtime: &KhiveRuntime,
    note_id: Uuid,
    from: &str,
    to: &str,
    transition_note: Option<&str>,
    namespace: &str,
) -> bool {
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
        Ok(mut w) => match w.execute(stmt).await {
            Ok(affected) if affected > 0 => true,
            Ok(_) => {
                tracing::warn!(
                    note_id = %note_id,
                    from,
                    to,
                    "gtd: audit insert affected no rows (non-fatal)"
                );
                false
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %note_id,
                    from,
                    to,
                    error = %e,
                    "gtd: audit write failed (non-fatal)"
                );
                false
            }
        },
        Err(e) => {
            tracing::warn!(
                note_id = %note_id,
                error = %e,
                "gtd: failed to acquire SQL writer for audit write (non-fatal)"
            );
            false
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
    #[serde(default)]
    include_blocked: Option<bool>,
}

/// `handle_complete`'s deserialization target. `pub` with private fields:
/// `kkernel`'s ADR-099 `--atomic` validation seam reuses this exact struct to
/// validate `gtd.complete` args, needing only the `Result<_, _>` outcome. See
/// `docs/api/lifecycle-audit.md#completeparams--transitionparams--pub-structs-private-fields`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteParams {
    id: String,
    #[serde(default)]
    result: Option<String>,
    /// Honors `status` param — accepts "done" (default) or "cancelled".
    /// Silently ignoring an explicit status arg is the worst outcome for callers
    /// who follow the MCP server hint "complete() defaults to 'done'; pass
    /// status='cancelled' for cancellation."
    #[serde(default)]
    status: Option<String>,
}

/// Validates the target terminal status for `complete()`.
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

/// Resolve a task-create reference as a full UUID or 8+ hex prefix.
/// Prefix lookup stays scoped to the caller's primary namespace per ADR-016;
/// task creation applies its primary-namespace mutation checks after this
/// resolution. Lifecycle by-ID operations use the private
/// `resolve_lifecycle_uuid` helper
/// instead.
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

/// Resolve a lifecycle verb's by-ID reference without a namespace filter.
/// `gtd.transition` and `gtd.complete` follow ADR-007's global by-ID contract
/// for both full UUIDs and short prefixes.
async fn resolve_lifecycle_uuid(s: &str, runtime: &KhiveRuntime) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix_unfiltered(s).await? {
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
            "context_entity_id must be a full UUID because a short prefix would require a \
             primary-namespace resolution and this field stores an explicit stable entity \
             reference; got {raw:?}"
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

/// Status a task is treated as when the `status` property is missing or not a string.
/// Property filters that select this value must use `FilterOp::TextEqOrNonText`
/// (not plain `Eq`) so their SQL predicate mirrors [`task_status`]'s exact
/// `CASE` semantics for absent, JSON-null, and non-text legacy values.
const DEFAULT_STATUS: &str = "inbox";

/// Priority a task is treated as when the `priority` property is missing/empty.
/// An explicit default-priority filter uses `FilterOp::EqOrMissing` so legacy
/// rows without the property remain visible.
const DEFAULT_PRIORITY: &str = "p2";

/// Status used internally on a task. Defaults to "inbox" when missing or non-string.
fn task_status(props: Option<&Value>) -> String {
    props
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_STATUS)
        .to_string()
}

/// Produce a lifecycle-write revision that is strictly newer than the exact
/// note snapshot used to make the transition decision. `updated_at` is the
/// optimistic-concurrency revision for task notes, so equality would let two
/// independently prepared writes appear to share one revision.
fn next_lifecycle_updated_at(snapshot_updated_at: i64) -> Result<i64, RuntimeError> {
    let minimum = snapshot_updated_at.checked_add(1).ok_or_else(|| {
        RuntimeError::Internal(
            "task updated_at is already at i64::MAX and cannot advance".to_string(),
        )
    })?;
    Ok(Utc::now().timestamp_micros().max(minimum))
}

fn lifecycle_write_conflict(
    operation: &str,
    note_id: Uuid,
    expected_status: &str,
    actual_status: &str,
) -> RuntimeError {
    if is_terminal(actual_status) {
        RuntimeError::InvalidInput(format!(
            "task {} is in terminal state {actual_status:?}; no further transitions allowed",
            short_id(note_id)
        ))
    } else if actual_status != expected_status {
        RuntimeError::InvalidInput(format!(
            "{operation}: task {} changed from expected state {expected_status:?} to \
             {actual_status:?}; retry with fresh state",
            short_id(note_id)
        ))
    } else {
        RuntimeError::InvalidInput(format!(
            "{operation}: task {} changed after the lifecycle decision while remaining in \
             state {actual_status:?}; retry with fresh state",
            short_id(note_id)
        ))
    }
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
/// `pub` (widened from private, ADR-099 B3): the
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
/// Accepts full RFC 3339 (e.g. `2026-06-01T00:00:00Z`) — normalized to UTC and
/// otherwise unaffected by `tz` — or date-only (e.g. `2026-06-01`), which
/// resolves to the earliest instant that belongs to that calendar date in
/// `tz` (ADR-169 D1). Returns the canonical RFC 3339 string stored in
/// `properties.due`, in `tz`'s offset spelling for the date-only case
/// (ADR-169 D5) — the value remains a single absolute instant; the offset
/// records which calendar the date was anchored in.
pub(crate) fn parse_due(value: &str, tz: Tz) -> Result<String, RuntimeError> {
    // Try full RFC 3339 / ISO-8601 with time zone first. Already carries an
    // explicit offset or `Z`, so it is unaffected by `tz` and keeps its
    // existing UTC normalization.
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc).to_rfc3339());
    }
    // Fallback: try date-only "YYYY-MM-DD".
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return anchor_date_to_earliest_instant(date, tz)
            .map(|dt| dt.to_rfc3339())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "due date {date} does not exist in the configured display timezone {tz} \
                     (the zone's local calendar skips this date entirely); got {value:?}"
                ))
            });
    }
    Err(RuntimeError::InvalidInput(format!(
        "due must be ISO-8601 (e.g., 2026-06-01T00:00:00Z or 2026-06-01); got {value:?}"
    )))
}

/// Resolve a calendar date to the earliest instant that belongs to it in
/// `tz` (ADR-169 D1). Midnight is not a total function of date and zone: on a
/// zone's own transition date local midnight can fail to exist (the clock
/// jumps over it) or occur twice (the clock repeats it). Both are one rule —
/// take the least instant whose local date, in `tz`, is `date` — not two
/// exceptions to it; neither case may be resolved by unwrapping to UTC,
/// which would silently reintroduce the defect ADR-169 exists to remove.
///
/// Returns `None` only when no instant at all maps to `date` in `tz` (the
/// zone's local calendar skips the date entirely, e.g. a whole-day UTC-offset
/// jump across the international date line) — a case ADR-169 does not name,
/// since its own gap/ambiguity examples are ordinary sub-day transitions.
fn anchor_date_to_earliest_instant(date: chrono::NaiveDate, tz: Tz) -> Option<DateTime<Tz>> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    match tz.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) => Some(dt),
        // Fall-back overlap: two instants map to the same wall-clock time.
        // The earlier one is the least instant whose local date is `date`.
        chrono::LocalResult::Ambiguous(earliest, _latest) => Some(earliest),
        // Spring-forward gap (or a larger jump): local midnight does not
        // exist. Find the transition by probing a point on the previous
        // calendar date guaranteed to fall outside any transition (noon),
        // reading the offset in effect there, and using it to project
        // `midnight` to the UTC instant it would have been under that
        // still-prior offset. Because the local date is a monotonically
        // non-decreasing function of UTC time, re-deriving the actual local
        // time at that UTC instant (an unconditional, never-ambiguous
        // UTC->local conversion) lands exactly on the first instant that
        // exists on `date` — the moment the new offset regime begins.
        chrono::LocalResult::None => {
            let prev_noon = date.pred_opt()?.and_hms_opt(12, 0, 0)?;
            let offset_before = match tz.offset_from_local_datetime(&prev_noon) {
                chrono::LocalResult::Single(o) => o,
                chrono::LocalResult::Ambiguous(o, _) => o,
                chrono::LocalResult::None => return None,
            };
            let candidate_utc =
                midnight - chrono::Duration::seconds(offset_before.fix().local_minus_utc() as i64);
            let resolved = tz.from_utc_datetime(&candidate_utc);
            (resolved.date_naive() == date).then_some(resolved)
        }
    }
}

#[cfg(test)]
mod parse_due_tests {
    use super::*;

    // America/New_York (used for the west-of-UTC and general DST coverage in
    // tests/assign.rs) never transitions at local midnight, so it cannot
    // exercise LocalResult::None or ::Ambiguous — a "DST test" against it
    // passes without touching the rule ADR-169 D1 actually specifies. The
    // zones and dates below are ADR-169's own worked table (measured against
    // the IANA database) of zones that DO transition at 00:00.

    #[test]
    fn date_only_due_resolves_to_first_instant_when_local_midnight_does_not_exist_havana() {
        let tz: Tz = "America/Havana".parse().expect("known IANA zone");
        let result = parse_due("2021-03-14", tz).expect(
            "a gap date must resolve to the first instant of that date, not error (ADR-169 D1)",
        );
        let parsed = DateTime::parse_from_rfc3339(&result).unwrap();
        assert_eq!(
            parsed.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2021, 3, 14).unwrap(),
            "got {result}"
        );
        assert!(
            parsed.time() > chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            "local midnight does not exist on this date, so the resolved instant must be \
             strictly after 00:00 — proving the gap was bridged forward, not unwrapped to UTC; \
             got {result}"
        );
    }

    #[test]
    fn date_only_due_resolves_to_first_instant_when_local_midnight_does_not_exist_santiago() {
        let tz: Tz = "America/Santiago".parse().expect("known IANA zone");
        let result = parse_due("2021-09-05", tz).expect(
            "a gap date must resolve to the first instant of that date, not error (ADR-169 D1)",
        );
        let parsed = DateTime::parse_from_rfc3339(&result).unwrap();
        assert_eq!(
            parsed.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2021, 9, 5).unwrap(),
            "got {result}"
        );
        assert!(
            parsed.time() > chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            "got {result}"
        );
    }

    #[test]
    fn date_only_due_picks_the_earlier_instant_when_local_midnight_occurs_twice_havana() {
        let tz: Tz = "America/Havana".parse().expect("known IANA zone");
        let date = chrono::NaiveDate::from_ymd_opt(2020, 11, 1).unwrap();
        let midnight = date.and_hms_opt(0, 0, 0).unwrap();
        // Derived directly from chrono_tz, independent of parse_due, so this
        // is a cross-check against the library's own contract rather than a
        // restatement of parse_due's internals.
        let (earliest, latest) = match tz.from_local_datetime(&midnight) {
            chrono::LocalResult::Ambiguous(a, b) => (a, b),
            other => panic!(
                "2020-11-01 in America/Havana must be ambiguous per ADR-169's worked table; \
                 got {other:?}"
            ),
        };
        assert!(
            earliest < latest,
            "sanity: chrono's `earliest` must be chronologically first"
        );

        let result =
            parse_due("2020-11-01", tz).expect("an ambiguous date must resolve, not error");
        let parsed = DateTime::parse_from_rfc3339(&result).unwrap();
        assert_eq!(
            parsed, earliest,
            "parse_due must pick the earlier of the two midnight instants (ADR-169 D1); got {result}"
        );
    }

    // Pacific/Apia jumped from UTC-11 to UTC+13 on 2011-12-30, skipping that
    // calendar date entirely in local time — not a sub-day gap but a
    // whole-day one, which ADR-169 D1's worked examples do not name. No
    // instant anywhere maps to this date in this zone, so parse_due errors
    // rather than silently resolving to a neighboring date under a
    // mislabeled due-date.
    #[test]
    fn date_only_due_in_a_skipped_local_day_is_rejected() {
        let tz: Tz = "Pacific/Apia".parse().expect("known IANA zone");
        let err = parse_due("2011-12-30", tz).expect_err("2011-12-30 never existed in Apia");
        let msg = err.to_string();
        assert!(
            msg.contains("2011-12-30"),
            "error must name the unparseable due date; got: {msg}"
        );
    }
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

/// Fetches every `task` note matching `property_filters` in one bounded
/// snapshot query (not a fixed-size unfiltered window — issue #772/#825).
/// Returns `Err(InvalidInput)` rather than a possibly-truncated result if
/// more than `TASK_SCAN_MAX_ROWS` rows match. See
/// `docs/api/task-query.md#fetch_all_matching_tasks--bounded-single-snapshot-scan-issue-772-825`.
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

/// Load a task note by global ID and verify it is actually `kind = "task"`.
/// Used by `complete` and `transition`; the task's stored namespace is
/// attribution and therefore does not gate these by-ID lifecycle operations.
async fn load_task(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw_id: &str,
) -> Result<(khive_storage::note::Note, String), RuntimeError> {
    let uuid = resolve_lifecycle_uuid(raw_id, runtime).await?;
    let store = runtime.notes(token)?;
    let note = store
        .get_note(uuid)
        .await
        .map_err(|e| RuntimeError::Internal(format!("get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("not found: {raw_id}")))?;

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

// ── atomic GTD transition ───────────────────────────────────────────────────

/// Perform an atomic conditional UPDATE on a task's properties, transitioning it
/// from `expected_current` to `target` status.
///
/// Relies on SQLite's atomic single-statement UPDATE plus a conditional WHERE
/// predicate over the exact decision snapshot's revision, deletion marker,
/// and semantic GTD status. This prevents a lifecycle write from replacing a
/// property document changed by a generic task update, even when that update
/// left `properties.status` unchanged. Only one write wins; the other gets 0
/// rows affected and must report an error.
///
/// Returns the number of rows updated (1 = success, 0 = lost race / already moved).
async fn atomic_gtd_transition(
    runtime: &KhiveRuntime,
    snapshot: &khive_storage::note::Note,
    expected_current: &str,
    target: &str,
    new_props: &serde_json::Value,
    updated_at: i64,
) -> Result<u64, RuntimeError> {
    // The conditional UPDATE runs as a single SQLite statement, which is atomic
    // on its own — no explicit transaction is needed because the decision
    // snapshot's revision/deletion/status checks and the write are one DML
    // statement. The GTD status predicate uses the properties column rather
    // than the row-visibility `status` column (which is always "active").
    //
    // Concurrency: if another writer has advanced the note revision, changed
    // semantic status, or soft-deleted the row by the time the predicate is
    // evaluated, it fails with rows_affected = 0. The caller distinguishes
    // that loser path from the pre-load errors returned by `load_task`.
    let statement =
        gtd_transition_statement(snapshot, expected_current, target, new_props, updated_at)?;
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
    snapshot: &khive_storage::note::Note,
    expected_current: &str,
    target: &str,
    new_props: &serde_json::Value,
    updated_at: i64,
) -> Result<SqlStatement, RuntimeError> {
    if snapshot.deleted_at.is_some() {
        return Err(RuntimeError::NotFound(format!(
            "deleted task {}",
            short_id(snapshot.id)
        )));
    }
    if updated_at <= snapshot.updated_at {
        return Err(RuntimeError::Internal(format!(
            "gtd lifecycle updated_at must strictly advance snapshot revision {} (got {updated_at})",
            snapshot.updated_at
        )));
    }
    let props_str = serde_json::to_string(new_props)
        .map_err(|e| RuntimeError::Internal(format!("serialize props: {e}")))?;
    Ok(SqlStatement {
        sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
              WHERE id = ?3 \
              AND updated_at = ?4 \
              AND deleted_at IS ?5 \
              AND ?2 > updated_at \
              AND CASE \
                    WHEN json_type(properties, '$.status') = 'text' \
                    THEN json_extract(properties, '$.status') \
                    ELSE 'inbox' \
                  END = ?6"
            .to_string(),
        params: vec![
            SqlValue::Text(props_str),
            SqlValue::Integer(updated_at),
            SqlValue::Text(snapshot.id.as_hyphenated().to_string()),
            SqlValue::Integer(snapshot.updated_at),
            match snapshot.deleted_at {
                Some(deleted_at) => SqlValue::Integer(deleted_at),
                None => SqlValue::Null,
            },
            SqlValue::Text(expected_current.to_string()),
        ],
        label: Some(format!("gtd_atomic_transition_{target}")),
    })
}

/// Build the guarded, mutation-free assertion used for an atomic
/// same-status transition.
///
/// Atomic prepare classifies `current == target` from a read snapshot, but a
/// preceding op in the same atomic file may transition, update, or delete the
/// task before this op reaches the commit pass. An empty plan would silently
/// discard the snapshot hypothesis. This statement deliberately assigns
/// `updated_at` to itself (so the persisted row is byte-for-byte unchanged)
/// while re-validating the exact revision, deletion marker, and semantic GTD
/// status under the transaction. Its affected-row guard therefore turns any
/// stale no-op into a whole-unit rollback.
pub fn gtd_noop_assertion_statement(
    snapshot: &khive_storage::note::Note,
    expected_current: &str,
) -> Result<SqlStatement, RuntimeError> {
    if snapshot.deleted_at.is_some() {
        return Err(RuntimeError::NotFound(format!(
            "deleted task {}",
            short_id(snapshot.id)
        )));
    }
    Ok(SqlStatement {
        sql: "UPDATE notes SET updated_at = updated_at \
              WHERE id = ?1 \
              AND updated_at = ?2 \
              AND deleted_at IS ?3 \
              AND CASE \
                    WHEN json_type(properties, '$.status') = 'text' \
                    THEN json_extract(properties, '$.status') \
                    ELSE 'inbox' \
                  END = ?4"
            .to_string(),
        params: vec![
            SqlValue::Text(snapshot.id.as_hyphenated().to_string()),
            SqlValue::Integer(snapshot.updated_at),
            match snapshot.deleted_at {
                Some(deleted_at) => SqlValue::Integer(deleted_at),
                None => SqlValue::Null,
            },
            SqlValue::Text(expected_current.to_string()),
        ],
        label: Some("gtd_atomic_noop_assertion".to_string()),
    })
}

/// Outcome of [`prepare_transition`]'s decide step: either no status change
/// (the idempotent `current == target` case) or a fully computed patched
/// `properties` value ready to apply — via
/// `atomic_gtd_transition` (canonical, immediate) or
/// [`gtd_transition_statement`] (ADR-099 atomic, deferred to the commit
/// pass). Canonical dispatch may record a caller note after the no-op decision;
/// atomic v1 instead carries a guarded no-effect assertion and deliberately
/// omits both the note mutation and audit side effect.
pub enum TransitionDecision {
    NoOp {
        /// Exact note snapshot used to classify the no-op.
        note: khive_storage::note::Note,
        current: String,
        target: String,
    },
    Write {
        /// Exact note snapshot whose revision/deletion marker guards apply.
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

    // Carries forward `note.properties`, which was already reservation-checked
    // at task creation (`gtd.assign` writes through `KhiveRuntime::create_note`);
    // every key inserted below is a fixed literal, so no caller input can
    // create or replace the reserved top-level key through this merge.
    let updated_at = next_lifecycle_updated_at(note.updated_at)?;
    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    let obj = props.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidInput("task properties must be a JSON object".to_string())
    })?;
    obj.insert("status".into(), json!(target.to_string()));
    if let Some(n) = note_arg {
        obj.insert("transition_note".into(), json!(n));
    }
    if target == "done" {
        obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
    }

    // #95: `transition_note` above is last-write-wins by design (it's the
    // "latest note" quick-read field every existing caller already
    // depends on) — but that means every note before the last one was
    // gone with no trace anywhere in the record. `gtd_lifecycle_audit`
    // (see the lifecycle-audit helpers above) already persists the full
    // from/to/note/at history in SQL, so the storage-side history this
    // issue asks about already exists; it just isn't surfaced back to a
    // caller reading the task. `transition_history` mirrors that same
    // per-transition record onto the note's own `properties` blob (a
    // free-form JSON column — no schema/storage change needed) so a
    // plain `get`/`tasks` read of the task, not just a raw SQL query
    // against the audit table, shows the accumulated history.
    let entry = json!({
        "from": current,
        "to": target,
        "note": note_arg,
        "at": micros_to_iso(updated_at),
    });
    match obj.get_mut("transition_history") {
        Some(Value::Array(history)) => history.push(entry),
        _ => {
            obj.insert("transition_history".into(), json!([entry]));
        }
    }

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
    /// Exact note snapshot whose revision/deletion marker guards apply.
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
/// terminal/lifecycle guards, and computes the patched `properties` value,
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
    if !can_transition(&current, target) {
        let allowed = allowed_transitions(&current);
        let allowed_display = if allowed.is_empty() {
            "(none)".to_string()
        } else {
            allowed.join(", ")
        };
        return Err(RuntimeError::InvalidInput(format!(
            "complete: cannot transition from {current:?} to {target:?}; \
             allowed from {current:?}: {allowed_display}. Full lifecycle: {TASK_LIFECYCLE_HELP}"
        )));
    }

    // Carries forward `note.properties`, which was already reservation-checked
    // at task creation (`gtd.assign` writes through `KhiveRuntime::create_note`);
    // every key inserted below is a fixed literal, so no caller input can
    // create or replace the reserved top-level key through this merge.
    let completed_at = Utc::now().to_rfc3339();
    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    let obj = props.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidInput("task properties must be a JSON object".to_string())
    })?;
    obj.insert("status".into(), json!(target));
    obj.insert("completed_at".into(), json!(completed_at));
    if let Some(result) = result_arg {
        obj.insert("result".into(), json!(result));
    }
    let updated_at = next_lifecycle_updated_at(note.updated_at)?;

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

        let diagnostics = crate::dependency::diagnose_tasks(self.runtime(), token, &notes).await?;
        let include_blocked = p.include_blocked.unwrap_or(false);
        let mut actionable: Vec<_> = notes
            .iter()
            .zip(diagnostics)
            .filter(|(_, diagnostic)| include_blocked || diagnostic.is_ready())
            .collect();

        // Sort: priority ascending (p0 first), then created_at descending (recent first),
        // then UUID ascending as a deterministic tie-breaker for equal-priority equal-timestamp
        // tasks so callers always observe a stable ordering.
        actionable.sort_by(|(a, a_diagnostic), (b, b_diagnostic)| {
            let a_blocked = !a_diagnostic.is_ready();
            let b_blocked = !b_diagnostic.is_ready();
            let ap = priority_rank(a.properties.as_ref());
            let bp = priority_rank(b.properties.as_ref());
            a_blocked
                .cmp(&b_blocked)
                .then(ap.cmp(&bp))
                .then(b.created_at.cmp(&a.created_at))
                .then(a.id.cmp(&b.id))
        });
        actionable.truncate(limit as usize);

        let result: Vec<Value> = actionable
            .iter()
            .map(|(note, diagnostic)| diagnostic.render(note))
            .collect();
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
        // the terminal/lifecycle guards, and computes the patched
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
            note,
            current,
            target,
            props,
            updated_at,
            completed_at,
        } = decision;

        // Guard the complete against the exact note snapshot that produced
        // `props`, not merely its old GTD status. A concurrent generic update
        // may leave status unchanged while changing description/content or
        // other task properties; replacing its property document would lose
        // that write and can break the task-body mirror invariant.
        let rows_affected =
            atomic_gtd_transition(self.runtime(), &note, &current, target, &props, updated_at)
                .await?;

        if rows_affected == 0 {
            // Re-read status for a precise conflict class. The snapshot guard
            // can also fail while status stays unchanged (for example, a
            // concurrent generic task update advanced the note revision).
            let (_, actual_now) = load_task(self.runtime(), token, &p.id).await?;
            return Err(lifecycle_write_conflict(
                "complete",
                note.id,
                &current,
                &actual_now,
            ));
        }

        // Write lifecycle audit record (best-effort, explicitly reported).
        ensure_audit_schema(self.runtime()).await;
        let audit_persisted = write_audit_record_with_status(
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
            "audit_persisted": audit_persisted,
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
                // A legacy task whose stored `status` is absent OR non-text is
                // treated as `inbox` everywhere else in this pack
                // (`task_status`, `render_task`). Use the storage predicate
                // whose SQL CASE expression reproduces that exact read model;
                // `EqOrMissing` alone would still exclude booleans, numbers,
                // arrays, and objects.
                op: if want == DEFAULT_STATUS {
                    FilterOp::TextEqOrNonText
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
            property_filters: property_filters.clone(),
            namespaces: namespaces.clone(),
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

        let diagnostics =
            crate::dependency::diagnose_tasks(self.runtime(), token, &page.items).await?;
        let result: Vec<Value> = page
            .items
            .iter()
            .zip(diagnostics.iter())
            .map(|(note, diagnostic)| diagnostic.render(note))
            .collect();

        // #96: a bare `[]` is indistinguishable from "no such task" when the
        // *default* terminal-status exclusion is what emptied the result —
        // the common case a caller hits right after `gtd.complete`. Probe for
        // a terminal task with the same namespace/assignee/priority filters
        // before changing the response shape; other empty results keep the
        // established bare array.
        if result.is_empty() && status_filter.is_none() {
            property_filters[0] = PropertyFilter {
                json_path: "$.status".to_string(),
                op: FilterOp::In(vec![
                    SqlValue::Text("done".to_string()),
                    SqlValue::Text("cancelled".to_string()),
                ]),
                value: SqlValue::Null,
            };
            let terminal_filter = NoteFilter {
                kind: Some("task".to_string()),
                property_filters,
                namespaces,
                ..Default::default()
            };
            let terminal_page = self
                .runtime()
                .notes(token)?
                .query_notes_filtered(
                    token.namespace().as_str(),
                    &terminal_filter,
                    PageRequest {
                        limit: 1,
                        offset: 0,
                    },
                )
                .await
                .map_err(|e| RuntimeError::Internal(format!("query_notes_filtered: {e}")))?;

            if !terminal_page.items.is_empty() {
                return Ok(json!({
                    "tasks": result,
                    "filter_excluded": ["done", "cancelled"],
                    "hint": "no tasks matched, but the default filter excludes done/cancelled \
                              tasks — pass status=\"done\" or status=\"cancelled\" to check \
                              whether a completed task exists before concluding it doesn't",
                }));
            }
        }
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

        let (note, current, target, audit_persisted) = match decision {
            TransitionDecision::NoOp {
                note,
                current,
                target,
            } => {
                // Idempotent by status (current == target) — but a caller-supplied
                // `note` was being silently discarded here. `transitioned`
                // stays an accurate statement about status; persisting the note is a
                // separate effect.
                let mut note_recorded = None;
                let mut audit_persisted = None;
                if let Some(n) = p.note.as_deref() {
                    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
                    let obj = props.as_object_mut().ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "task properties must be a JSON object".to_string(),
                        )
                    })?;
                    obj.insert("transition_note".into(), json!(n));
                    let updated_at = next_lifecycle_updated_at(note.updated_at)?;
                    // Same conditional UPDATE `atomic_gtd_transition` uses elsewhere —
                    // here expected == target, so it only wins if the status is still
                    // what `prepare_transition` just observed (loses the race to a
                    // concurrent real transition without clobbering it).
                    let rows_affected = atomic_gtd_transition(
                        self.runtime(),
                        &note,
                        &current,
                        &target,
                        &props,
                        updated_at,
                    )
                    .await?;
                    if rows_affected > 0 {
                        ensure_audit_schema(self.runtime()).await;
                        audit_persisted = Some(
                            write_audit_record_with_status(
                                self.runtime(),
                                note.id,
                                &current,
                                &target,
                                Some(n),
                                token.namespace().as_str(),
                            )
                            .await,
                        );
                    }
                    note_recorded = Some(rows_affected > 0);
                }

                let mut response = json!({
                    "transitioned": false,
                    "id": short_id(note.id),
                    "full_id": note.id.as_hyphenated().to_string(),
                    "from": current,
                    "to": target,
                    "note": "already in target status",
                });
                if let Some(recorded) = note_recorded {
                    response["note_recorded"] = json!(recorded);
                }
                if let Some(persisted) = audit_persisted {
                    response["audit_persisted"] = json!(persisted);
                }
                return Ok(response);
            }
            TransitionDecision::Write {
                mut note,
                current,
                target,
                props,
                updated_at,
                transition_note,
            } => {
                // The shared conditional write guards the exact snapshot
                // revision/deletion marker as well as semantic status, so a
                // concurrent generic update cannot be replaced by stale
                // lifecycle properties.
                let rows_affected = atomic_gtd_transition(
                    self.runtime(),
                    &note,
                    &current,
                    &target,
                    &props,
                    updated_at,
                )
                .await?;

                if rows_affected == 0 {
                    let (_, actual_now) = load_task(self.runtime(), token, &p.id).await?;
                    return Err(lifecycle_write_conflict(
                        "transition",
                        note.id,
                        &current,
                        &actual_now,
                    ));
                }

                note.properties = Some(props);
                // notes.status is row-visibility (always "active" for live
                // rows); GTD status lives in properties.status and W1-G's
                // remap surfaces it at data.status in the response.
                note.updated_at = updated_at;

                // Write lifecycle audit record (best-effort, explicitly reported).
                ensure_audit_schema(self.runtime()).await;
                let audit_persisted = write_audit_record_with_status(
                    self.runtime(),
                    note.id,
                    &current,
                    &target,
                    transition_note.as_deref(),
                    token.namespace().as_str(),
                )
                .await;

                (note, current, target, audit_persisted)
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
            "audit_persisted": audit_persisted,
        }))
    }
}

#[cfg(test)]
mod lifecycle_snapshot_tests {
    use super::*;

    #[test]
    fn lifecycle_revision_strictly_advances_a_future_snapshot() {
        let future_snapshot = Utc::now()
            .timestamp_micros()
            .checked_add(86_400_000_000)
            .expect("one-day future timestamp is representable");
        assert_eq!(
            next_lifecycle_updated_at(future_snapshot).expect("advance lifecycle revision"),
            future_snapshot + 1
        );
    }

    async fn seeded_task() -> (KhiveRuntime, NamespaceToken, khive_storage::note::Note) {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize local");
        let mut task =
            khive_storage::note::Note::new("local", "task", "canonical-lifecycle-test-task");
        task.name = Some("canonical-lifecycle-test-task".to_string());
        task.properties = Some(json!({"status": "inbox", "priority": "p2"}));
        runtime
            .notes(&token)
            .expect("note store")
            .upsert_note(task.clone())
            .await
            .expect("seed task");
        (runtime, token, task)
    }

    async fn install_concurrent_mirrored_update(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        snapshot: &khive_storage::note::Note,
    ) -> i64 {
        // This is the canonical generic-note CAS seam after the task hook has
        // synchronized the two body spellings. It starts from the very same
        // snapshot as the lifecycle decision, then commits first.
        let (concurrent, _) = runtime
            .update_note_from_snapshot_with_embedding_report(
                token,
                snapshot.clone(),
                khive_runtime::NotePatch::new(
                    None,
                    Some("concurrent mirrored body".to_string()),
                    None,
                    None,
                    Some(json!({"description": "concurrent mirrored body"})),
                ),
            )
            .await
            .expect("commit concurrent canonical update");
        concurrent.updated_at
    }

    async fn assert_concurrent_update_survived(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        task_id: Uuid,
        concurrent_revision: i64,
    ) {
        let persisted = runtime
            .notes(token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        assert_eq!(persisted.updated_at, concurrent_revision);
        assert_eq!(persisted.content, "concurrent mirrored body");
        assert_eq!(
            persisted
                .properties
                .as_ref()
                .and_then(|properties| properties.get("description"))
                .and_then(Value::as_str),
            Some("concurrent mirrored body")
        );
        assert_eq!(task_status(persisted.properties.as_ref()), "inbox");
    }

    #[tokio::test]
    async fn canonical_transition_refuses_stale_decision_snapshot() {
        let (runtime, token, task) = seeded_task().await;
        let decision = prepare_transition(
            &runtime,
            &token,
            &task.id.as_hyphenated().to_string(),
            "next",
            None,
        )
        .await
        .expect("prepare transition");
        let TransitionDecision::Write {
            note: snapshot,
            current,
            target,
            props,
            updated_at,
            ..
        } = decision
        else {
            panic!("inbox -> next must be a write decision");
        };
        assert!(updated_at > snapshot.updated_at);

        let concurrent_revision =
            install_concurrent_mirrored_update(&runtime, &token, &snapshot).await;
        let affected =
            atomic_gtd_transition(&runtime, &snapshot, &current, &target, &props, updated_at)
                .await
                .expect("execute guarded transition");
        assert_eq!(affected, 0, "stale transition snapshot must lose its CAS");
        assert_concurrent_update_survived(&runtime, &token, task.id, concurrent_revision).await;
    }

    #[tokio::test]
    async fn canonical_transition_guard_closes_soft_delete_without_revision_change() {
        let (runtime, token, task) = seeded_task().await;
        let decision = prepare_transition(
            &runtime,
            &token,
            &task.id.as_hyphenated().to_string(),
            "next",
            None,
        )
        .await
        .expect("prepare transition");
        let TransitionDecision::Write {
            note: snapshot,
            current,
            target,
            props,
            updated_at,
            ..
        } = decision
        else {
            panic!("inbox -> next must be a write decision");
        };

        assert!(runtime
            .notes(&token)
            .expect("note store")
            .delete_note(task.id, khive_storage::DeleteMode::Soft)
            .await
            .expect("soft delete task"));
        let tombstone = runtime
            .notes(&token)
            .expect("note store")
            .get_note_including_deleted(task.id)
            .await
            .expect("read tombstone")
            .expect("tombstone exists");
        assert_eq!(
            tombstone.updated_at, snapshot.updated_at,
            "legacy soft delete deliberately demonstrates why deletion marker is a separate guard"
        );
        assert!(tombstone.deleted_at.is_some());

        let affected =
            atomic_gtd_transition(&runtime, &snapshot, &current, &target, &props, updated_at)
                .await
                .expect("execute guarded transition");
        assert_eq!(affected, 0, "soft-deleted snapshot must lose its CAS");
    }

    #[tokio::test]
    async fn canonical_complete_refuses_stale_decision_snapshot() {
        let (runtime, token, task) = seeded_task().await;
        let decision = prepare_complete(
            &runtime,
            &token,
            &task.id.as_hyphenated().to_string(),
            None,
            Some("shipped"),
        )
        .await
        .expect("prepare complete");
        assert!(decision.updated_at > decision.note.updated_at);

        let concurrent_revision =
            install_concurrent_mirrored_update(&runtime, &token, &decision.note).await;
        let affected = atomic_gtd_transition(
            &runtime,
            &decision.note,
            &decision.current,
            decision.target,
            &decision.props,
            decision.updated_at,
        )
        .await
        .expect("execute guarded complete");
        assert_eq!(affected, 0, "stale complete snapshot must lose its CAS");
        assert_concurrent_update_survived(&runtime, &token, task.id, concurrent_revision).await;
    }
}
