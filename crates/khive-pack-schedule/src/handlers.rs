//! Verb handler implementations for the schedule pack (ADR-040).
//!
//! All four verbs (`remind`, `schedule`, `agenda`, `cancel`) store and query
//! `scheduled_event` notes. Trigger evaluation is NOT performed by the pack —
//! the pack only stores intent. See ADR-040 §Trigger evaluation for execution modes.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};
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

/// Validate a cron expression (5-field) — only basic structure check in v1.
fn validate_repeat(repeat: &str) -> Result<(), RuntimeError> {
    match repeat {
        "daily" | "weekly" | "monthly" => Ok(()),
        cron => {
            let fields: Vec<&str> = cron.split_whitespace().collect();
            if fields.len() == 5 {
                Ok(())
            } else {
                Err(RuntimeError::InvalidInput(format!(
                    "invalid repeat expression {cron:?}: must be \"daily\", \"weekly\", \
                     \"monthly\", or a 5-field cron expression"
                )))
            }
        }
    }
}

/// Validate that `action` is parseable DSL via `khive_request::parse_request`.
///
/// This catches garbage like `"x"` or `"bogus-not-a-valid-verb()"` at write
/// time rather than at trigger time, when nobody is watching.
fn validate_action(action: &str) -> Result<(), RuntimeError> {
    khive_request::parse_request(action).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "schedule.action: invalid DSL ({e}); \
             provide a valid verb call (e.g. \"remind(content=\\\"hello\\\")\")"
        ))
    })?;
    Ok(())
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

/// `remind` — create a time-triggered reminder (ADR-040 §remind).
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

/// `schedule` — schedule a future verb dispatch (ADR-040 §schedule).
pub(crate) async fn handle_schedule(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
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
    validate_action(p.action.trim())?;

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

/// `agenda` — list upcoming scheduled events (ADR-040 §agenda).
pub(crate) async fn handle_agenda(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: AgendaParams = deser(params)?;
    let limit = p.limit.unwrap_or(20).clamp(1, 200);

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
    let mut offset: u32 = 0;
    let mut events: Vec<(DateTime<Utc>, Value)> = Vec::new();

    loop {
        let page = store
            .query_notes_filtered(
                namespace,
                &filter,
                PageRequest {
                    limit: PAGE_SIZE,
                    offset: offset.into(),
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
            events.push((instant, note_to_event_json(n)));
        }

        // Stop when the storage page is exhausted.
        if page_len < PAGE_SIZE {
            break;
        }
        offset += PAGE_SIZE;
    }

    // Sort ascending by parsed timestamp — correct regardless of tz format (H1).
    events.sort_by_key(|(ts, _)| *ts);

    let events: Vec<Value> = events
        .into_iter()
        .map(|(_, v)| v)
        .take(limit as usize)
        .collect();
    let count = events.len();

    Ok(json!({ "events": events, "count": count }))
}

/// `cancel` — cancel a scheduled event (ADR-040 §cancel).
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

    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
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
