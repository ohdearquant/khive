//! Verb handler implementations for the schedule pack.
//!
//! All four verbs (`remind`, `schedule`, `agenda`, `cancel`) store and query
//! `scheduled_event` notes. Trigger evaluation is NOT performed by the pack —
//! the pack only stores intent. See `docs/design.md` for execution modes.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter, SortDir};
use khive_storage::types::{PageRequest, SqlValue};

fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

/// Resolve a raw id string to a full UUID.
///
/// Accepts a 36-char hyphenated UUID or an 8+ hex-char short prefix.
/// The prefix is resolved via `runtime.resolve_prefix` (namespace-scoped).
async fn resolve_id(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
    verb: &str,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = raw.parse::<Uuid>() {
        return Ok(uuid);
    }
    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(token, raw).await? {
            Some(uuid) => Ok(uuid),
            None => Err(RuntimeError::InvalidInput(format!(
                "{verb}: no record matches prefix: {raw:?}"
            ))),
        };
    }
    Err(RuntimeError::InvalidInput(format!(
        "{verb}: invalid id {raw:?}; expected full UUID or 8-char hex prefix"
    )))
}

fn note_to_event_json(note: &Note) -> Value {
    json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "kind": "scheduled_event",
        "content": note.content,
        "namespace": note.namespace,
        "properties": note.properties,
        "created_at": micros_to_iso(note.created_at),
        "updated_at": micros_to_iso(note.updated_at),
    })
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// Validate that `at` is a valid RFC 3339 timestamp and lies in the future.
///
/// Accepts any RFC 3339 string that `chrono` can parse as a `DateTime<Utc>`
/// (e.g. "2027-01-01T00:00:00Z" or "2027-01-01T00:00:00+05:30").
///
/// Returns the parsed UTC instant so callers can use it for comparisons
/// without re-parsing. The original string is preserved by callers who want
/// to store it as-is (see H5 fix below).
///
/// Rejects:
/// - Unparseable strings (not RFC 3339).
/// - Timestamps that lie in the past relative to `Utc::now()`.
fn validate_at(verb: &str, at: &str) -> Result<DateTime<Utc>, RuntimeError> {
    let parsed = at.parse::<DateTime<Utc>>().map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "{verb}.at: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), got {at:?}"
        ))
    })?;
    if parsed <= Utc::now() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}.at: cannot schedule in the past (got {at:?}); \
             use a future timestamp"
        )));
    }
    Ok(parsed)
}

/// Validate a cron expression (5-field).
///
/// Accepts the literals `daily`, `weekly`, `monthly`, and standard five-field
/// cron expressions of the form `MIN HOUR DOM MON DOW`, where each field is
/// either `*` or a non-negative integer within the accepted range:
/// - MIN  0–59
/// - HOUR 0–23
/// - DOM  1–31
/// - MON  1–12
/// - DOW  0–7
///
/// Malformed fields (non-numeric, out-of-range) are rejected with
/// `RuntimeError::InvalidInput` rather than silently accepted.
fn validate_repeat(repeat: &str) -> Result<(), RuntimeError> {
    match repeat {
        "daily" | "weekly" | "monthly" => return Ok(()),
        _ => {}
    }

    let fields: Vec<&str> = repeat.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(RuntimeError::InvalidInput(format!(
            "invalid repeat expression {repeat:?}: must be \"daily\", \"weekly\", \
             \"monthly\", or a 5-field cron expression (MIN HOUR DOM MON DOW)"
        )));
    }

    // (field_name, min_val, max_val)
    let ranges: [(&str, u64, u64); 5] = [
        ("minute", 0, 59),
        ("hour", 0, 23),
        ("day-of-month", 1, 31),
        ("month", 1, 12),
        ("day-of-week", 0, 7),
    ];
    for (field, (name, lo, hi)) in fields.iter().zip(ranges.iter()) {
        if *field == "*" {
            continue;
        }
        match field.parse::<u64>() {
            Ok(v) if v >= *lo && v <= *hi => {}
            Ok(v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid repeat expression {repeat:?}: cron {name} field {v} is out of \
                     range {lo}–{hi}"
                )));
            }
            Err(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid repeat expression {repeat:?}: cron {name} field {field:?} is not \
                     \"*\" or a non-negative integer"
                )));
            }
        }
    }
    Ok(())
}

/// Validate that `action` is parseable DSL via `khive_request::parse_request`.
///
/// This catches garbage like `"x"` or `"bogus-not-a-valid-verb()"` at write
/// time rather than at trigger time, when nobody is watching. Returns the
/// parsed request so callers can inspect the verb names without re-parsing.
fn validate_action(action: &str) -> Result<khive_request::ParsedRequest, RuntimeError> {
    khive_request::parse_request(action).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "schedule.action: invalid DSL ({e}); \
             provide a valid verb call (e.g. \"remind(content=\\\"hello\\\")\")"
        ))
    })
}

// ── param structs ────────────────────────────────────────────────────────────

// ue-errors C1 (cross-pack): deny_unknown_fields so typo kwargs are rejected
// at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemindParams {
    pub content: String,
    pub at: String,
    #[serde(default)]
    pub repeat: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleParams {
    pub action: String,
    pub at: String,
    #[serde(default)]
    pub repeat: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgendaParams {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelParams {
    pub id: String,
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// `remind` — create a time-triggered reminder.
pub(crate) async fn handle_remind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: RemindParams = deser(params)?;
    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "remind: `content` must not be empty".into(),
        ));
    }
    if p.at.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "remind: `at` must not be empty".into(),
        ));
    }
    // Validate RFC 3339 and reject past timestamps (C3).
    // Preserve the caller's original string as `trigger_at` so the
    // submitted wall time and offset are round-tripped faithfully (H5).
    // The UTC instant is used only for comparison/ordering.
    let trigger_at_original = p.at.trim().to_string();
    let _trigger_utc = validate_at("remind", &trigger_at_original)?;

    if let Some(ref r) = p.repeat {
        validate_repeat(r)?;
    }

    let properties = json!({
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
        "event_type": "remind",
        "payload": null,
        "fired_at": null,
        "cancelled_at": null,
    });

    let note = runtime
        .create_note(
            token,
            "scheduled_event",
            None,
            &p.content,
            None,
            Some(properties),
            Vec::new(),
        )
        .await?;

    Ok(json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "event_type": "remind",
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
    }))
}

/// `schedule` — schedule a future verb dispatch.
pub(crate) async fn handle_schedule(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ScheduleParams = deser(params)?;
    if p.action.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "schedule: `action` must not be empty".into(),
        ));
    }
    if p.at.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "schedule: `at` must not be empty".into(),
        ));
    }
    // Validate DSL parseability at write time (C4). Garbage like "x" or
    // "bogus-not-a-valid-verb()" is rejected before it enters storage.
    let parsed = validate_action(p.action.trim())?;
    // Validate that each verb in the action string is registered. This catches
    // nonexistent verbs at schedule-creation time rather than at trigger time
    // when nobody is watching.
    for op in &parsed.ops {
        registry.describe_verb(&op.tool).map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "schedule.action: verb {:?} is not registered; \
                 provide a valid verb call (e.g. \"remind(content=\\\"hello\\\")\")",
                op.tool
            ))
        })?;
    }

    // Validate RFC 3339 and reject past timestamps (C3).
    // Preserve the caller's original string as `trigger_at` so the
    // submitted wall time and offset are round-tripped faithfully (H5).
    // The UTC instant is used only for comparison/ordering.
    let trigger_at_original = p.at.trim().to_string();
    let _trigger_utc = validate_at("schedule", &trigger_at_original)?;

    if let Some(ref r) = p.repeat {
        validate_repeat(r)?;
    }

    let properties = json!({
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
        "event_type": "schedule",
        "payload": p.action,
        "fired_at": null,
        "cancelled_at": null,
    });

    let note = runtime
        .create_note(
            token,
            "scheduled_event",
            None,
            &p.action,
            None,
            Some(properties),
            Vec::new(),
        )
        .await?;

    Ok(json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "event_type": "schedule",
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
    }))
}

/// `agenda` — list upcoming scheduled events.
pub(crate) async fn handle_agenda(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: AgendaParams = deser(params)?;
    let limit: u32 = match p.limit {
        None => 20,
        Some(0) => {
            return Err(RuntimeError::InvalidInput(
                "agenda: `limit` must be between 1 and 200 (inclusive); got 0".into(),
            ));
        }
        Some(n) if n > 200 => {
            return Err(RuntimeError::InvalidInput(format!(
                "agenda: `limit` must be between 1 and 200 (inclusive); got {n}"
            )));
        }
        Some(n) => n,
    };

    // Parse from/to bounds as instants so comparison is correct regardless of
    // timezone offset or DST. Reject non-RFC-3339 filter values (H1).
    let from_instant: Option<DateTime<Utc>> = match p.from {
        Some(ref s) => {
            let ts = s.parse::<DateTime<Utc>>().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "agenda.from: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), \
                     got {s:?}"
                ))
            })?;
            Some(ts)
        }
        None => None,
    };
    let to_instant: Option<DateTime<Utc>> = match p.to {
        Some(ref s) => {
            let ts = s.parse::<DateTime<Utc>>().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "agenda.to: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), \
                     got {s:?}"
                ))
            })?;
            Some(ts)
        }
        None => None,
    };

    // Push kind + status filter into SQL so SQLite can use idx_schedule_trigger
    // (declared in lib.rs on json_extract(properties,'$.trigger_at')).
    // The RFC3339 from/to window comparison and the Rust sort by parsed DateTime<Utc>
    // are kept in Rust to preserve timezone-correct ordering and handle corrupt legacy rows.
    let store = runtime.notes(token)?;
    let namespace = token.namespace().as_str();
    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: Some(("$.trigger_at".to_string(), SortDir::Asc)),
    };

    const PAGE_SIZE: u32 = 200;
    // Use u64 for offset so it cannot overflow for very large stores (SCH-AUD-006).
    let mut offset: u64 = 0;
    // Bounded top-k: keep only the `limit` earliest events while scanning
    // so we avoid full allocation + sort of an unbounded set (SCH-AUD-004).
    // BinaryHeap requires Ord on the element; serde_json::Value does not
    // implement Ord, so we maintain a max-heap over just the timestamp and
    // pair it with a separate Vec for the serialized payloads.
    use std::collections::BinaryHeap;
    // Max-heap over timestamps: the root is always the latest (worst) entry.
    let mut ts_heap: BinaryHeap<DateTime<Utc>> = BinaryHeap::new();
    // Parallel vec of serialized events, kept in the same insertion order.
    // After scanning we zip ts_heap (drained) with this vec and sort.
    let mut ts_vec: Vec<DateTime<Utc>> = Vec::new();
    let mut ev_vec: Vec<Value> = Vec::new();

    loop {
        let page = store
            .query_notes_filtered(
                namespace,
                &filter,
                PageRequest {
                    limit: PAGE_SIZE,
                    offset,
                },
            )
            .await?;
        let page_len = page.items.len() as u32;

        for n in &page.items {
            // Parse trigger_at as an instant. Skip rows with unparseable
            // trigger_at — these are legacy corrupt rows (H1, H2).
            let trigger_at_str = n
                .properties
                .as_ref()
                .and_then(|p| p.get("trigger_at"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let instant = match trigger_at_str.parse::<DateTime<Utc>>() {
                Ok(ts) => ts,
                Err(_) => continue,
            };

            // Apply from/to window using parsed instants (H1).
            if let Some(from) = from_instant {
                if instant < from {
                    continue;
                }
            }
            if let Some(to) = to_instant {
                if instant > to {
                    continue;
                }
            }

            // Maintain bounded top-k (SCH-AUD-004):
            // if we already have `limit` items and this one is not earlier
            // than the current worst (maximum), skip it entirely.
            if ts_heap.len() < limit as usize {
                ts_heap.push(instant);
                ts_vec.push(instant);
                ev_vec.push(note_to_event_json(n));
            } else if let Some(&max_ts) = ts_heap.peek() {
                if instant < max_ts {
                    // Evict the worst entry and insert the better one.
                    // We need to remove max_ts from ts_vec/ev_vec too; find
                    // its last occurrence (insertion order, LIFO for ties).
                    ts_heap.pop();
                    if let Some(pos) = ts_vec.iter().rposition(|t| *t == max_ts) {
                        ts_vec.remove(pos);
                        ev_vec.remove(pos);
                    }
                    ts_heap.push(instant);
                    ts_vec.push(instant);
                    ev_vec.push(note_to_event_json(n));
                }
            }
        }

        // Stop when the storage page is exhausted.
        if page_len < PAGE_SIZE {
            break;
        }
        // Checked addition — extremely unlikely to overflow u64 for personal
        // schedule data, but the standard coding policy requires it (SCH-AUD-006).
        offset = offset
            .checked_add(u64::from(PAGE_SIZE))
            .ok_or_else(|| RuntimeError::Internal("agenda: pagination offset overflow".into()))?;
    }

    // Sort ascending by parsed timestamp (sort only the selected ≤ limit items).
    let mut selected: Vec<(DateTime<Utc>, Value)> = ts_vec.into_iter().zip(ev_vec).collect();
    selected.sort_by_key(|(ts, _)| *ts);

    let events: Vec<Value> = selected.into_iter().map(|(_, v)| v).collect();
    let count = events.len();

    Ok(json!({ "events": events, "count": count }))
}

/// `cancel` — cancel a scheduled event.
pub(crate) async fn handle_cancel(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: CancelParams = deser(params)?;
    let id = resolve_id(runtime, token, &p.id, "cancel").await?;

    let store = runtime.notes(token)?;
    let mut note = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("cancel: event {id} not found")))?;

    if note.namespace != token.namespace().as_str() {
        return Err(RuntimeError::NotFound(format!(
            "cancel: event {id} not found"
        )));
    }
    if note.kind != "scheduled_event" {
        return Err(RuntimeError::InvalidInput(format!(
            "cancel: note {id} is kind {:?}, expected \"scheduled_event\"",
            note.kind
        )));
    }

    // Require properties to be a JSON object (or absent — treated as `{}`).
    // Mutable string-key indexing on a non-object value panics in serde_json;
    // reject corrupt notes here with a clear error instead (SCH-AUD-001).
    let raw_props = note.properties.clone().unwrap_or_else(|| json!({}));
    let mut props = match raw_props {
        Value::Object(_) => raw_props,
        ref other => {
            let type_name = match other {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => unreachable!(),
            };
            return Err(RuntimeError::InvalidInput(format!(
                "cancel: event {id} has malformed properties (expected JSON object, got \
                 {type_name}); cannot mutate"
            )));
        }
    };
    if props.get("status").and_then(Value::as_str) == Some("cancelled") {
        return Err(RuntimeError::InvalidInput(format!(
            "cancel: event {id} is already cancelled"
        )));
    }

    let cancelled_at = Utc::now().to_rfc3339();
    props["status"] = json!("cancelled");
    props["cancelled_at"] = json!(cancelled_at);
    note.properties = Some(props.clone());
    note.updated_at = Utc::now().timestamp_micros();

    store
        .upsert_note(note)
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: upsert_note: {e}")))?;

    Ok(json!({
        "id": short_id(id),
        "full_id": id.as_hyphenated().to_string(),
        "status": "cancelled",
        "cancelled_at": cancelled_at,
        "properties": props,
    }))
}
