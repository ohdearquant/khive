use chrono::{DateTime, Utc};
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

    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    ensure_suppression_schema(&mut *writer).await?;
    let inserted = writer
        .execute(SqlStatement {
            sql: khive_runtime::sql!("comm_suppression_insert").into(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(scope.to_string()),
                SqlValue::Text(kind.to_string()),
                SqlValue::Text(value.clone()),
                SqlValue::Text(source.to_string()),
                SqlValue::Text(token.actor().id.clone()),
                SqlValue::Integer(created_at),
                SqlValue::Text(reason.clone()),
            ],
            label: Some("comm_suppression_add".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    if inserted != 1 {
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

fn validate_closed_set<'a>(
    verb: &str,
    field: &str,
    value: &'a str,
    allowed: &[&str],
) -> Result<&'a str, RuntimeError> {
    if allowed.contains(&value) {
        Ok(value)
    } else {
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
            if value.trim().is_empty() || value.trim() != value || value.contains('\0') {
                return Err(RuntimeError::InvalidInput(
                    "suppression_add: `value` for kind `recipient_key` must be non-empty and have no surrounding whitespace".into(),
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
    writer
        .execute_script(crate::vocab::COMM_SUPPRESSION_SCHEMA_STMT.to_string())
        .await
        .map_err(RuntimeError::Storage)
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
mod tests {
    use chrono::{DateTime, Utc};
    use khive_runtime::{AllowAllGate, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
    use khive_storage::Note;
    use khive_types::{EventKind, SubstrateKind};
    use serde_json::{json, Value};

    use super::*;

    fn runtime() -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            brain_profile: None,
            packs: vec!["kg".into(), "comm".into()],
            actor_id: Some("actor:maintainer".into()),
            gate: std::sync::Arc::new(AllowAllGate),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("runtime")
    }

    fn token(runtime: &KhiveRuntime) -> khive_runtime::NamespaceToken {
        runtime.authorize(Namespace::local()).expect("token")
    }

    async fn add(
        runtime: &KhiveRuntime,
        token: &khive_runtime::NamespaceToken,
        scope: &str,
        kind: &str,
        value: &str,
        source: &str,
    ) -> Result<Value, RuntimeError> {
        handle_suppression_add(
            runtime,
            token,
            json!({
                "scope": scope,
                "kind": kind,
                "value": value,
                "source": source,
                "reason": "test fixture",
            }),
        )
        .await
    }

    async fn evidence(
        runtime: &KhiveRuntime,
        from: &str,
        created_at: i64,
        quarantined: bool,
        recipient_key: Option<&str>,
    ) -> uuid::Uuid {
        let mut properties = json!({
            "direction": "inbound",
            "from": format!("email:{from}"),
            "channel_kind": "email",
        });
        if quarantined {
            properties["quarantined"] = json!(true);
        }
        if let Some(key) = recipient_key {
            properties["recipient_key"] = json!(key);
        }
        let mut note =
            Note::new("local", "message", "authenticated reply").with_properties(properties);
        note.created_at = created_at;
        note.updated_at = created_at;
        let id = note.id;
        assert!(runtime
            .backend()
            .notes()
            .expect("notes")
            .try_insert_note(note)
            .await
            .expect("insert evidence"));
        id
    }

    fn timestamp(value: &Value) -> i64 {
        DateTime::parse_from_rfc3339(value["created_at"].as_str().expect("created_at"))
            .expect("valid timestamp")
            .with_timezone(&Utc)
            .timestamp_micros()
    }

    fn invalid(error: RuntimeError) {
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
    }

    fn condition(error: RuntimeError, expected: &str) {
        let RuntimeError::Khive(error) = error else {
            panic!("expected typed refusal, got {error:?}");
        };
        assert_eq!(error.kind(), khive_types::ErrorKind::Unauthorized);
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[tokio::test]
    async fn suppression_add_list_remove_round_trip_is_audited() {
        let runtime = runtime();
        let token = token(&runtime);
        let added = add(
            &runtime,
            &token,
            "outbound",
            "address",
            " Person@Example.COM ",
            "operator",
        )
        .await
        .expect("add");
        assert_eq!(added["value"], "person@example.com");
        assert_eq!(added["created_by"], "actor:maintainer");
        let duplicate = add(
            &runtime,
            &token,
            "outbound",
            "address",
            "PERSON@example.com",
            "operator",
        )
        .await
        .unwrap_err();
        let RuntimeError::Khive(duplicate) = duplicate else {
            panic!("expected duplicate suppression conflict, got {duplicate:?}");
        };
        assert_eq!(duplicate.kind(), khive_types::ErrorKind::Conflict);

        let listed = handle_suppression_list(&runtime, &token, json!({}))
            .await
            .expect("list");
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["id"], added["id"]);

        let removed = handle_suppression_remove(
            &runtime,
            &token,
            json!({"id": added["id"], "reason": "operator correction"}),
        )
        .await
        .expect("remove");
        assert_eq!(removed["removed"], true);
        let audit_id = removed["audit_event_id"]
            .as_str()
            .expect("audit event id")
            .parse()
            .expect("audit UUID");
        let audit = runtime
            .events(&token)
            .expect("events")
            .get_event(audit_id)
            .await
            .expect("audit read")
            .expect("audit record");
        assert_eq!(audit.kind, EventKind::Audit);
        assert_eq!(audit.substrate, SubstrateKind::Event);
        assert_eq!(audit.payload["reason"], "operator correction");
        assert_eq!(audit.payload["value"], "person@example.com");

        let listed = handle_suppression_list(&runtime, &token, json!({}))
            .await
            .expect("list after remove");
        assert!(listed["items"].as_array().unwrap().is_empty());
        add(
            &runtime,
            &token,
            "outbound",
            "address",
            "person@example.com",
            "operator",
        )
        .await
        .expect("removed value can be added again");
    }

    #[tokio::test]
    async fn opt_out_survives_removal_of_an_operator_entry_for_the_same_value() {
        let runtime = runtime();
        let token = token(&runtime);
        let operator = add(
            &runtime,
            &token,
            "outbound",
            "address",
            "person@example.com",
            "operator",
        )
        .await
        .expect("operator add");
        let opt_out = add(
            &runtime,
            &token,
            "outbound",
            "address",
            "Person@Example.com",
            "reply_stop",
        )
        .await
        .expect("an opt-out for a value the operator already listed is still recorded");
        assert_ne!(operator["id"], opt_out["id"]);

        handle_suppression_remove(
            &runtime,
            &token,
            json!({"id": operator["id"], "reason": "operator correction"}),
        )
        .await
        .expect("operator entry removal needs only a reason");

        let listed = handle_suppression_list(&runtime, &token, json!({}))
            .await
            .expect("list");
        let items = listed["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], opt_out["id"]);
        assert_eq!(items[0]["source"], "reply_stop");
    }

    #[tokio::test]
    async fn suppression_closed_values_accept_members_and_refuse_unknowns() {
        let runtime = runtime();
        let token = token(&runtime);

        for (index, scope) in ["outbound", "inbound"].into_iter().enumerate() {
            add(
                &runtime,
                &token,
                scope,
                "address",
                &format!("scope{index}@example.com"),
                "operator",
            )
            .await
            .expect("closed scope member");
        }
        for (index, kind) in ["address", "domain", "recipient_key"]
            .into_iter()
            .enumerate()
        {
            let value = match kind {
                "address" => format!("kind{index}@example.com"),
                "domain" => format!("kind{index}.example.com"),
                _ => format!("key-{index}"),
            };
            add(&runtime, &token, "outbound", kind, &value, "operator")
                .await
                .expect("closed kind member");
        }
        for (index, source) in ["operator", "mailto_token", "one_click", "reply_stop"]
            .into_iter()
            .enumerate()
        {
            add(
                &runtime,
                &token,
                "outbound",
                "recipient_key",
                &format!("source-{index}"),
                source,
            )
            .await
            .expect("closed source member");
        }

        for (field, value) in [
            ("scope", "sideways"),
            ("kind", "mailbox"),
            ("source", "imported"),
        ] {
            let mut params = json!({
                "scope": "outbound",
                "kind": "address",
                "value": format!("invalid-{field}@example.com"),
                "source": "operator",
            });
            params[field] = json!(value);
            invalid(
                handle_suppression_add(&runtime, &token, params)
                    .await
                    .unwrap_err(),
            );
        }
    }

    #[tokio::test]
    async fn opt_out_removal_requires_new_authenticated_same_address_evidence() {
        let runtime = runtime();
        let token = token(&runtime);
        let old = evidence(
            &runtime,
            "person@example.com",
            Utc::now().timestamp_micros() - 10_000_000,
            false,
            None,
        )
        .await;
        let added = add(
            &runtime,
            &token,
            "outbound",
            "address",
            "person@example.com",
            "reply_stop",
        )
        .await
        .expect("add opt-out");
        let entry_time = timestamp(&added);

        condition(
            handle_suppression_remove(
                &runtime,
                &token,
                json!({"id": added["id"], "reason": "person wrote back"}),
            )
            .await
            .unwrap_err(),
            "evidence_inbound_message_id is required",
        );

        let different_address =
            evidence(&runtime, "other@example.com", entry_time + 1, false, None).await;
        condition(
            handle_suppression_remove(
                &runtime,
                &token,
                json!({
                    "id": added["id"],
                    "reason": "person wrote back",
                    "evidence_inbound_message_id": different_address,
                }),
            )
            .await
            .unwrap_err(),
            "same address",
        );

        let unauthenticated =
            evidence(&runtime, "person@example.com", entry_time + 2, true, None).await;
        condition(
            handle_suppression_remove(
                &runtime,
                &token,
                json!({
                    "id": added["id"],
                    "reason": "person wrote back",
                    "evidence_inbound_message_id": unauthenticated,
                }),
            )
            .await
            .unwrap_err(),
            "authenticated email",
        );

        condition(
            handle_suppression_remove(
                &runtime,
                &token,
                json!({
                    "id": added["id"],
                    "reason": "person wrote back",
                    "evidence_inbound_message_id": old,
                }),
            )
            .await
            .unwrap_err(),
            "created after the suppression entry",
        );

        let valid = evidence(&runtime, "person@example.com", entry_time + 3, false, None).await;
        let removed = handle_suppression_remove(
            &runtime,
            &token,
            json!({
                "id": added["id"],
                "reason": "person wrote back",
                "evidence_inbound_message_id": valid,
            }),
        )
        .await
        .expect("valid evidence removes opt-out");
        assert_eq!(removed["removed"], true);
    }

    #[tokio::test]
    async fn opt_out_sources_all_require_evidence_and_operator_removal_requires_reason() {
        let runtime = runtime();
        let token = token(&runtime);
        for (index, source) in ["mailto_token", "one_click", "reply_stop"]
            .into_iter()
            .enumerate()
        {
            let added = add(
                &runtime,
                &token,
                "outbound",
                "recipient_key",
                &format!("opt-out-{index}"),
                source,
            )
            .await
            .expect("add opt-out source");
            condition(
                handle_suppression_remove(
                    &runtime,
                    &token,
                    json!({"id": added["id"], "reason": "undo"}),
                )
                .await
                .unwrap_err(),
                "evidence_inbound_message_id is required",
            );
        }

        let operator = add(
            &runtime,
            &token,
            "outbound",
            "address",
            "operator@example.com",
            "operator",
        )
        .await
        .expect("operator entry");
        for reason in ["", "  "] {
            invalid(
                handle_suppression_remove(
                    &runtime,
                    &token,
                    json!({"id": operator["id"], "reason": reason}),
                )
                .await
                .unwrap_err(),
            );
        }
        handle_suppression_remove(
            &runtime,
            &token,
            json!({"id": operator["id"], "reason": "verified correction"}),
        )
        .await
        .expect("operator entry removed with reason");

        let keyed_opt_out = add(
            &runtime,
            &token,
            "outbound",
            "recipient_key",
            "derived-key-1",
            "mailto_token",
        )
        .await
        .expect("keyed opt-out");
        let keyed_evidence = evidence(
            &runtime,
            "person@example.com",
            timestamp(&keyed_opt_out) + 1,
            false,
            Some("derived-key-1"),
        )
        .await;
        handle_suppression_remove(
            &runtime,
            &token,
            json!({
                "id": keyed_opt_out["id"],
                "reason": "recipient wrote back",
                "evidence_inbound_message_id": keyed_evidence,
            }),
        )
        .await
        .expect("trusted recipient-key evidence removes keyed opt-out");
    }

    #[test]
    fn outbound_suppression_matches_exact_domains_and_recipient_keys() {
        use khive_runtime::{
            outbound_email_is_suppressed, EmailSuppression, EmailSuppressionKind,
            EmailSuppressionScope,
        };

        let domain = [EmailSuppression {
            scope: EmailSuppressionScope::Outbound,
            kind: EmailSuppressionKind::Domain,
            value: "x.com".into(),
        }];
        assert!(outbound_email_is_suppressed("a@x.com", None, &domain));
        assert!(!outbound_email_is_suppressed("a@sub.x.com", None, &domain));

        let key = [EmailSuppression {
            scope: EmailSuppressionScope::Outbound,
            kind: EmailSuppressionKind::RecipientKey,
            value: "key-1".into(),
        }];
        assert!(outbound_email_is_suppressed(
            "a@example.com",
            Some("key-1"),
            &key
        ));
        assert!(!outbound_email_is_suppressed(
            "a@example.com",
            Some("key-2"),
            &key
        ));
        assert!(!outbound_email_is_suppressed("a@example.com", None, &key));
    }

    #[test]
    fn suppression_caller_scope_uses_the_runtime_actor_as_current_maintainer() {
        let runtime = runtime();
        assert!(ensure_suppression_caller_allowed(&runtime, "actor:maintainer").is_ok());
        condition(
            ensure_suppression_caller_allowed(&runtime, "actor:other").unwrap_err(),
            "not allowed for this caller actor",
        );
        let external = runtime.clone().with_outbound_email_policy(
            khive_runtime::OutboundEmailPolicy::configured_for_tests(
                vec!["maintainer@example.com".into()],
                vec!["actor:external".into()],
                5,
            )
            .unwrap(),
        );
        assert!(ensure_suppression_caller_allowed(&external, "actor:external").is_ok());
        assert!(ensure_suppression_caller_allowed(&external, "actor:other").is_err());
    }
}
