use chrono::{DateTime, Utc};
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use khive_storage::Event;
use khive_types::{EventKind, KhiveError, SubstrateKind};

const SCOPES: &[&str] = &["outbound", "inbound"];
const KINDS: &[&str] = &["address", "domain", "recipient_key"];
const SOURCES: &[&str] = &["operator", "mailto_token", "one_click", "reply_stop"];
const OPT_OUT_SOURCES: &[&str] = &["mailto_token", "one_click", "reply_stop"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppressionAddParams {
    scope: String,
    kind: String,
    value: String,
    source: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppressionListParams {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppressionRemoveParams {
    id: String,
    reason: String,
    #[serde(default)]
    evidence_inbound_message_id: Option<String>,
}

#[derive(Clone)]
struct SuppressionRecord {
    id: Uuid,
    scope: String,
    kind: String,
    value: String,
    source: String,
    created_by: String,
    created_at: i64,
    reason: String,
}

pub(crate) async fn handle_suppression_add(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: SuppressionAddParams = khive_runtime::deser_params(params)?;
    ensure_suppression_caller_allowed(runtime, &token.actor().id)?;
    let scope = validate_closed_set("suppression_add", "scope", &params.scope, SCOPES)?;
    let kind = validate_closed_set("suppression_add", "kind", &params.kind, KINDS)?;
    let source = validate_closed_set("suppression_add", "source", &params.source, SOURCES)?;
    let value = normalize_suppression_value(kind, &params.value)?;
    let reason = params.reason.unwrap_or_default();
    let id = Uuid::new_v4();
    let created_at = Utc::now().timestamp_micros();

    let inserted = insert_suppression(
        runtime,
        SuppressionInsert {
            id,
            scope,
            kind,
            value: &value,
            source,
            created_by: token.actor().id.as_str(),
            created_at,
            reason: &reason,
        },
    )
    .await?;
    if !inserted {
        return Err(RuntimeError::Khive(KhiveError::conflict(
            "suppression_add: an entry already exists for this scope, kind, value, and source",
        )));
    }

    let record = SuppressionRecord {
        id,
        scope: scope.into(),
        kind: kind.into(),
        value,
        source: source.into(),
        created_by: token.actor().id.clone(),
        created_at,
        reason,
    };
    record_json(&record)
}

pub(crate) async fn handle_suppression_list(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: SuppressionListParams = khive_runtime::deser_params(params)?;
    ensure_suppression_caller_allowed(runtime, &token.actor().id)?;
    let scope = params
        .scope
        .as_deref()
        .map(|value| validate_closed_set("suppression_list", "scope", value, SCOPES))
        .transpose()?;
    let kind = params
        .kind
        .as_deref()
        .map(|value| validate_closed_set("suppression_list", "kind", value, KINDS))
        .transpose()?;
    let limit = params.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(RuntimeError::InvalidInput(
            "suppression_list: `limit` must be in 1..=500".into(),
        ));
    }
    let offset = params.offset.unwrap_or(0);
    let offset = i64::try_from(offset).map_err(|_| {
        RuntimeError::InvalidInput("suppression_list: `offset` exceeds i64::MAX".into())
    })?;

    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let rows = writer
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_list").into(),
            params: vec![
                scope.map_or(SqlValue::Null, |value| SqlValue::Text(value.into())),
                kind.map_or(SqlValue::Null, |value| SqlValue::Text(value.into())),
                SqlValue::Integer(limit as i64),
                SqlValue::Integer(offset),
            ],
            label: Some("comm_suppression_list".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    let items = rows
        .iter()
        .map(|row| parse_record(row, "suppression_list").and_then(|record| record_json(&record)))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(json!({
        "items": items,
        "limit": limit,
        "offset": offset,
    }))
}

pub(crate) async fn handle_suppression_remove(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: SuppressionRemoveParams = khive_runtime::deser_params(params)?;
    if runtime.config().actor_id.as_deref() != Some(token.actor().id.as_str()) {
        return Err(RuntimeError::Khive(KhiveError::unauthorized(
            "suppression_remove is restricted to the configured maintainer",
        )));
    }
    if params.reason.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "suppression_remove: `reason` must not be empty".into(),
        ));
    }
    let id = parse_uuid("suppression_remove", "id", &params.id)?;
    let evidence_id = params
        .evidence_inbound_message_id
        .as_deref()
        .map(|value| parse_uuid("suppression_remove", "evidence_inbound_message_id", value))
        .transpose()?;

    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let row = writer
        .query_row(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_remove_select").into(),
            params: vec![
                SqlValue::Text(id.to_string()),
                evidence_id.map_or(SqlValue::Null, |value| SqlValue::Text(value.to_string())),
            ],
            label: Some("comm_suppression_remove_select".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?
        .ok_or_else(|| RuntimeError::NotFound(format!("suppression entry {id}")))?;
    let record = parse_record(&row, "suppression_remove")?;
    if OPT_OUT_SOURCES.contains(&record.source.as_str()) {
        validate_opt_out_evidence(&record, &row, evidence_id)?;
    }

    drop(writer);

    let event = Event::new(
        token.namespace().as_str(),
        "comm.suppression_remove",
        EventKind::Audit,
        SubstrateKind::Event,
        token.actor().id.clone(),
    )
    .with_target(id)
    .with_payload(json!({
        "action": "suppression_remove_requested",
        "entry_id": id,
        "scope": record.scope,
        "kind": record.kind,
        "value": record.value,
        "source": record.source,
        "created_by": record.created_by,
        "entry_reason": record.reason,
        "reason": params.reason,
        "evidence_inbound_message_id": evidence_id,
    }));
    let audit_event_id = event.id;
    runtime
        .events(token)?
        .append_event(event)
        .await
        .map_err(RuntimeError::Storage)?;

    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    let deleted = writer
        .execute(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_remove_delete").into(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("comm_suppression_remove".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    if deleted != 1 {
        return Err(RuntimeError::NotFound(format!("suppression entry {id}")));
    }

    Ok(json!({
        "id": id,
        "removed": true,
        "audit_event_id": audit_event_id,
    }))
}

/// This is the actor-scope extension point for the external-sender allowlist.
fn ensure_suppression_caller_allowed(
    runtime: &KhiveRuntime,
    caller_actor: &str,
) -> Result<(), RuntimeError> {
    if runtime.config().actor_id.as_deref() == Some(caller_actor)
        || runtime
            .outbound_email_policy()
            .allows_external_sender(caller_actor)
    {
        Ok(())
    } else {
        Err(RuntimeError::Khive(KhiveError::unauthorized(
            "suppression operation is not allowed for this caller actor",
        )))
    }
}

/// Read outbound suppression entries through the comm store for delivery and admission checks.
pub async fn load_outbound_email_suppressions(
    runtime: &KhiveRuntime,
) -> Result<Vec<khive_runtime::EmailSuppression>, RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let rows = writer
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_outbound_list").into(),
            params: vec![],
            label: Some("comm_suppression_outbound_list".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    rows.iter()
        .map(|row| {
            let record = parse_record(row, "outbound_suppression_read")?;
            let scope = match record.scope.as_str() {
                "outbound" => khive_runtime::EmailSuppressionScope::Outbound,
                _ => {
                    return Err(RuntimeError::Internal(
                        "outbound suppression query returned a non-outbound entry".into(),
                    ));
                }
            };
            let kind = match record.kind.as_str() {
                "address" => khive_runtime::EmailSuppressionKind::Address,
                "domain" => khive_runtime::EmailSuppressionKind::Domain,
                "recipient_key" => khive_runtime::EmailSuppressionKind::RecipientKey,
                _ => {
                    return Err(RuntimeError::Internal(
                        "outbound suppression query returned an invalid kind".into(),
                    ));
                }
            };
            Ok(khive_runtime::EmailSuppression {
                scope,
                kind,
                value: record.value,
            })
        })
        .collect()
}

/// Read inbound suppression entries through the comm store for the inbound disposition gate.
///
/// Only `address` and `domain` kinds can match an inbound sender; a `recipient_key` entry is an
/// outbound identity form and is never returned here.
pub async fn load_inbound_email_suppressions(
    runtime: &KhiveRuntime,
) -> Result<Vec<khive_runtime::EmailSuppression>, RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let rows = writer
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_inbound_list").into(),
            params: vec![],
            label: Some("comm_suppression_inbound_list".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    rows.iter()
        .map(|row| {
            let record = parse_record(row, "inbound_suppression_read")?;
            if record.scope != "inbound" {
                return Err(RuntimeError::Internal(
                    "inbound suppression query returned a non-inbound entry".into(),
                ));
            }
            let kind = match record.kind.as_str() {
                "address" => khive_runtime::EmailSuppressionKind::Address,
                "domain" => khive_runtime::EmailSuppressionKind::Domain,
                _ => {
                    return Err(RuntimeError::Internal(
                        "inbound suppression query returned an invalid kind".into(),
                    ));
                }
            };
            Ok(khive_runtime::EmailSuppression {
                scope: khive_runtime::EmailSuppressionScope::Inbound,
                kind,
                value: record.value,
            })
        })
        .collect()
}

/// The fields of one suppression entry to write, borrowed so both the verb and the channel-side
/// opt-out recording reach the same store function.
struct SuppressionInsert<'a> {
    id: Uuid,
    scope: &'a str,
    kind: &'a str,
    value: &'a str,
    source: &'a str,
    created_by: &'a str,
    created_at: i64,
    reason: &'a str,
}

/// The one SQL path every suppression entry takes: the suppression verbs' add path and the
/// channel's mailto opt-out recording both funnel here, so there is a single insert statement for
/// the list. Uniqueness is `(scope, kind, value, source)`, so a repeated entry inserts nothing and
/// reports `Ok(false)` rather than failing.
async fn insert_suppression(
    runtime: &KhiveRuntime,
    entry: SuppressionInsert<'_>,
) -> Result<bool, RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let inserted = writer
        .execute(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_insert").into(),
            params: vec![
                SqlValue::Text(entry.id.to_string()),
                SqlValue::Text(entry.scope.to_string()),
                SqlValue::Text(entry.kind.to_string()),
                SqlValue::Text(entry.value.to_string()),
                SqlValue::Text(entry.source.to_string()),
                SqlValue::Text(entry.created_by.to_string()),
                SqlValue::Integer(entry.created_at),
                SqlValue::Text(entry.reason.to_string()),
            ],
            label: Some("comm_suppression_add".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(inserted == 1)
}

/// Record the suppression entry for a verified mailto unsubscribe (ADR-200 §5.4 entry point 1).
///
/// The token is the authority, so this runs before sender authentication. `recipient_key` is the
/// 64-lowercase-hex key text the caller's [`khive_runtime::OutboundEmailPolicy`] verified from the
/// token, and `created_by` attributes the write to the poller's actor. The same store function the
/// suppression verbs use performs the insert, so an entry this path and an operator entry with the
/// same `(scope, kind, value, source)` do not duplicate. The `reason` names no address, because the
/// token identifies the recipient without the opt-out ever holding it.
pub async fn record_mailto_unsubscribe_suppression(
    runtime: &KhiveRuntime,
    recipient_key: &str,
    created_by: &str,
) -> Result<bool, RuntimeError> {
    insert_suppression(
        runtime,
        SuppressionInsert {
            id: Uuid::new_v4(),
            scope: "outbound",
            kind: "recipient_key",
            value: recipient_key,
            source: "mailto_token",
            created_by,
            created_at: Utc::now().timestamp_micros(),
            reason: "mailto unsubscribe token",
        },
    )
    .await
}

/// Record an authenticated delivered reply's opt-out (ADR-200 §5.4 entry point 3).
pub async fn record_reply_opt_out_suppression(
    runtime: &KhiveRuntime,
    from_address: &str,
    created_by: &str,
) -> Result<bool, RuntimeError> {
    let address = normalize_suppression_value("address", from_address)?;
    insert_suppression(
        runtime,
        SuppressionInsert {
            id: Uuid::new_v4(),
            scope: "outbound",
            kind: "address",
            value: &address,
            source: "reply_stop",
            created_by,
            created_at: Utc::now().timestamp_micros(),
            reason: "authenticated reply opt-out",
        },
    )
    .await
}

/// Apply one successful read of the remote suppression mirror (ADR-200 §6).
///
/// Every key of the complete remote set that the local list does not already hold as a
/// `one_click` entry is added as one. Nothing is removed and nothing is written back: the local
/// list stays the one source that admission and delivery read. The refresh time is stamped only
/// after every key is in, so a failure part way keeps the previous stamp, which can only make
/// admission refuse sooner. A key that is not 64 lowercase hex characters refuses the whole
/// set before anything is written. Returns the number of keys added.
pub async fn apply_suppression_mirror_refresh(
    runtime: &KhiveRuntime,
    remote_as_of: &str,
    recipient_keys: &[String],
    created_by: &str,
) -> Result<usize, RuntimeError> {
    if remote_as_of.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "suppression mirror read carries no as_of time".into(),
        ));
    }
    if let Some(position) = recipient_keys
        .iter()
        .position(|key| key.len() != 64 || !khive_types::is_lowercase_hex(key))
    {
        return Err(RuntimeError::InvalidInput(format!(
            "suppression mirror key at position {position} is not 64 lowercase hex characters"
        )));
    }
    let remote_count = i64::try_from(recipient_keys.len()).map_err(|_| {
        RuntimeError::InvalidInput("suppression mirror set is larger than a count can hold".into())
    })?;
    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let rows = writer
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_outbound_list").into(),
            params: vec![],
            label: Some("comm_suppression_mirror_held".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    let mut held = std::collections::HashSet::new();
    for row in &rows {
        let record = parse_record(row, "suppression_mirror_refresh")?;
        if record.kind == "recipient_key" && record.source == "one_click" {
            held.insert(record.value);
        }
    }
    let reason = format!("one-click unsubscribe, remote mirror as of {remote_as_of}");
    let mut added = 0;
    for key in recipient_keys {
        if held.contains(key) {
            continue;
        }
        let inserted = writer
            .execute(SqlStatement {
                sql: khive_runtime::sql!("comm_suppression_insert").into(),
                params: vec![
                    SqlValue::Text(Uuid::new_v4().to_string()),
                    SqlValue::Text("outbound".to_string()),
                    SqlValue::Text("recipient_key".to_string()),
                    SqlValue::Text(key.clone()),
                    SqlValue::Text("one_click".to_string()),
                    SqlValue::Text(created_by.to_string()),
                    SqlValue::Integer(Utc::now().timestamp_micros()),
                    SqlValue::Text(reason.clone()),
                ],
                label: Some("comm_suppression_mirror_add".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;
        if inserted == 1 {
            added += 1;
        }
    }
    writer
        .execute(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_mirror_upsert").into(),
            params: vec![
                SqlValue::Integer(Utc::now().timestamp_micros()),
                SqlValue::Text(remote_as_of.to_string()),
                SqlValue::Integer(remote_count),
            ],
            label: Some("comm_suppression_mirror_stamp".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(added)
}

/// The time of the last successful remote suppression mirror refresh, or `None` when none is
/// recorded (ADR-200 §6). External admission reads this from the local store, with no network
/// I/O.
pub async fn suppression_mirror_last_refresh(
    runtime: &KhiveRuntime,
) -> Result<Option<DateTime<Utc>>, RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let rows = writer
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_mirror_select").into(),
            params: vec![],
            label: Some("comm_suppression_mirror_select".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let micros = integer_column(row, "refreshed_at", "suppression_mirror_read")?;
    DateTime::<Utc>::from_timestamp_micros(micros)
        .map(Some)
        .ok_or_else(|| {
            RuntimeError::Internal("suppression mirror refresh time is out of range".into())
        })
}

fn validate_closed_set<'a>(
    verb: &str,
    field: &str,
    value: &'a str,
    allowed: &[&str],
) -> Result<&'a str, RuntimeError> {
    if allowed.contains(&value) {
        Ok(value)
    } else {
        let value = mask_for_redaction_surface(RedactionSurface::GateProbe, value);
        Err(RuntimeError::InvalidInput(format!(
            "{verb}: invalid `{field}` {value:?}; expected {}",
            allowed.join(", ")
        )))
    }
}

fn normalize_suppression_value(kind: &str, value: &str) -> Result<String, RuntimeError> {
    match kind {
        "address" => khive_types::email_address::normalize_email_recipient(value).ok_or_else(|| {
            RuntimeError::InvalidInput(
                "suppression_add: `value` must contain a valid email addr-spec for kind `address`".into(),
            )
        }),
        "domain" => {
            let normalized = value.trim();
            if normalized.is_empty()
                || normalized.contains('@')
                || normalized.contains('*')
                || normalized.chars().any(char::is_whitespace)
                || normalized.contains('\0')
            {
                return Err(RuntimeError::InvalidInput(
                    "suppression_add: `value` for kind `domain` must be one exact domain without a wildcard".into(),
                ));
            }
            Ok(normalized.to_ascii_lowercase())
        }
        "recipient_key" => {
            if value.len() != 64 || !khive_types::is_lowercase_hex(value) {
                return Err(RuntimeError::InvalidInput(
                    "suppression_add: `value` for kind `recipient_key` must be exactly 64 lowercase hex characters".into(),
                ));
            }
            Ok(value.to_string())
        }
        _ => Err(RuntimeError::Internal(format!(
            "suppression_add: unsupported kind reached value normalizer: {kind:?}"
        ))),
    }
}

fn parse_uuid(verb: &str, field: &str, value: &str) -> Result<Uuid, RuntimeError> {
    Uuid::parse_str(value.trim())
        .map_err(|_| RuntimeError::InvalidInput(format!("{verb}: `{field}` must be a full UUID")))
}

async fn ensure_suppression_schema(
    writer: &mut dyn khive_storage::SqlWriter,
) -> Result<(), RuntimeError> {
    for statement in [
        crate::vocab::COMM_SUPPRESSION_SCHEMA_STMT,
        crate::vocab::COMM_SUPPRESSION_MIRROR_SCHEMA_STMT,
    ] {
        writer
            .execute_script(statement.to_string())
            .await
            .map_err(RuntimeError::Storage)?;
    }
    Ok(())
}

fn parse_record(row: &SqlRow, verb: &str) -> Result<SuppressionRecord, RuntimeError> {
    let id = text_column(row, "id", verb)?;
    let id = Uuid::parse_str(&id).map_err(|_| {
        RuntimeError::Internal(format!("{verb}: malformed suppression id from storage"))
    })?;
    Ok(SuppressionRecord {
        id,
        scope: text_column(row, "scope", verb)?,
        kind: text_column(row, "kind", verb)?,
        value: text_column(row, "value", verb)?,
        source: text_column(row, "source", verb)?,
        created_by: text_column(row, "created_by", verb)?,
        created_at: integer_column(row, "created_at", verb)?,
        reason: text_column(row, "reason", verb)?,
    })
}

fn record_json(record: &SuppressionRecord) -> Result<Value, RuntimeError> {
    let created_at =
        DateTime::<Utc>::from_timestamp_micros(record.created_at).ok_or_else(|| {
            RuntimeError::Internal("suppression entry has an invalid creation timestamp".into())
        })?;
    Ok(json!({
        "id": record.id,
        "scope": record.scope,
        "kind": record.kind,
        "value": record.value,
        "source": record.source,
        "created_by": record.created_by,
        "created_at": created_at.to_rfc3339(),
        "reason": record.reason,
    }))
}

fn text_column(row: &SqlRow, name: &str, verb: &str) -> Result<String, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        other => Err(RuntimeError::Internal(format!(
            "{verb}: malformed `{name}` column in suppression row: {other:?}"
        ))),
    }
}

fn integer_column(row: &SqlRow, name: &str, verb: &str) -> Result<i64, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        other => Err(RuntimeError::Internal(format!(
            "{verb}: malformed `{name}` column in suppression row: {other:?}"
        ))),
    }
}

fn optional_text_column(row: &SqlRow, name: &str) -> Option<String> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Some(value.clone()),
        Some(SqlValue::Integer(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn optional_integer_column(row: &SqlRow, name: &str) -> Option<i64> {
    match row.get(name) {
        Some(SqlValue::Integer(value)) => Some(*value),
        _ => None,
    }
}

fn opt_out_evidence_refusal(condition: &str) -> RuntimeError {
    RuntimeError::Khive(KhiveError::unauthorized(format!(
        "suppression_remove: {condition}"
    )))
}

fn validate_opt_out_evidence(
    record: &SuppressionRecord,
    row: &SqlRow,
    evidence_id: Option<Uuid>,
) -> Result<(), RuntimeError> {
    let Some(evidence_id) = evidence_id else {
        return Err(opt_out_evidence_refusal(
            "evidence_inbound_message_id is required for opt-out sources",
        ));
    };
    let evidence_id_text = evidence_id.to_string();
    if optional_text_column(row, "evidence_id").as_deref() != Some(evidence_id_text.as_str())
        || optional_text_column(row, "evidence_direction").as_deref() != Some("inbound")
    {
        return Err(opt_out_evidence_refusal(
            "evidence_inbound_message_id must reference a live inbound message",
        ));
    }

    let channel_kind = optional_text_column(row, "evidence_channel_kind");
    let quarantined = optional_integer_column(row, "evidence_quarantined");
    // The email channel only ingests non-quarantined messages after its
    // domain-authentication gate passes.
    let authenticated = match optional_integer_column(row, "evidence_authenticated") {
        Some(1) => true,
        Some(_) => false,
        None => channel_kind.as_deref() == Some("email") && quarantined != Some(1),
    };
    if channel_kind.as_deref() != Some("email") || !authenticated || quarantined == Some(1) {
        return Err(opt_out_evidence_refusal(
            "evidence message must be an authenticated email",
        ));
    }

    let evidence_created_at = optional_integer_column(row, "evidence_created_at");
    if evidence_created_at.is_none_or(|created_at| created_at <= record.created_at) {
        return Err(opt_out_evidence_refusal(
            "evidence message must be created after the suppression entry",
        ));
    }

    let raw_from = optional_text_column(row, "evidence_from").unwrap_or_default();
    let sender = raw_from.strip_prefix("email:").unwrap_or(&raw_from);
    let normalized_sender = khive_types::email_address::normalize_email_recipient(sender);
    let same_address = match record.kind.as_str() {
        "address" => normalized_sender.as_deref() == Some(record.value.as_str()),
        // The trusted email ingest path must attach the derived key; comm cannot
        // reproduce the secret HMAC, so missing key evidence fails closed.
        "recipient_key" => {
            optional_text_column(row, "evidence_recipient_key").as_deref()
                == Some(record.value.as_str())
        }
        "domain" => false,
        _ => false,
    };
    if !same_address {
        return Err(opt_out_evidence_refusal(
            "evidence message must be from the same address as the suppression entry",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "suppression_tests.rs"]
mod tests;
