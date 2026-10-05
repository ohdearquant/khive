use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{mark_read_targets_atomic, mark_read_targets_best_effort, validate_bulk_read_targets};
use crate::params::{deser, DeliveredParams, MarkReadParams};

/// `delivered` — confirm that the inbound half of an internal dual-write
/// exists, using the outbound UUID copied into `properties.outbound_ref`.
///
/// The outbound row is deliberately not resolved or fetched first. An
/// ambiguous atomic-write outcome may have committed both copies or neither,
/// and legacy/injected half-pairs can lack the outbound row. A full UUID is
/// therefore required instead of a display prefix.
/// This result says nothing about a later external transport attempt (SMTP,
/// Telegram, and so on); it only confirms the comm pack's inbound sibling.
pub(crate) async fn handle_delivered(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let listing_field = params.as_object().is_some_and(|map| {
        [
            "mailbox_actor",
            "tags",
            "kind",
            "thread_id",
            "limit",
            "box",
            "offset",
            "status",
            "wait_ms",
            "from_actor",
            "from_prefix",
            "exclude_from_actor",
            "to_actor",
            "since",
            "before",
            "subject_contains",
            "content_contains",
            "fields",
        ]
        .iter()
        .any(|field| map.contains_key(*field))
    });
    let p: DeliveredParams = deser(params).map_err(|error| match error {
        RuntimeError::InvalidInput(message) if listing_field => {
            RuntimeError::InvalidInput(format!(
                "{message}; comm.delivered confirms one outbound `id`; \
             use comm.inbox(box=\"sent\") to list sent messages"
            ))
        }
        other => other,
    })?;
    let outbound_id = Uuid::parse_str(p.id.trim()).map_err(|_| {
        RuntimeError::InvalidInput(
            "delivered: a short prefix would require scoped resolution and cannot prove an \
             exact delivery correlation; `id` must be the full outbound UUID returned as \
             `full_id` by comm.send or comm.reply, or surfaced as `outbound_id` in an \
             ambiguous atomic-write error"
                .into(),
        )
    })?;

    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) AS inbound_count \
                  FROM notes \
                  WHERE namespace = ?1 \
                    AND kind = 'message' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.direction') = 'inbound' \
                    AND json_extract(properties, '$.from_actor') = ?2 \
                    AND json_extract(properties, '$.outbound_ref') = ?3"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(token.actor().id.clone()),
                SqlValue::Text(outbound_id.to_string()),
            ],
            label: Some("comm_delivered".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    let inbound_count = match row.and_then(|row| row.get("inbound_count").cloned()) {
        Some(SqlValue::Integer(count)) if count >= 0 => count,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "delivered: storage returned an invalid inbound count: {other:?}"
            )))
        }
    };
    let delivered = inbound_count > 0;

    Ok(json!({
        "id": outbound_id,
        "status": if delivered { "delivered" } else { "undelivered" },
        "delivered": delivered,
        "inbound_count": inbound_count,
    }))
}

/// `mark_read` — canonical bulk mark-read surface with optional all-or-nothing mutation.
pub(crate) async fn handle_mark_read(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mut params: Value,
) -> Result<Value, RuntimeError> {
    if let Some(map) = params.as_object_mut() {
        if map.contains_key("ids") && map.contains_key("id") {
            return Err(RuntimeError::InvalidInput(
                "`id` is an alias for `ids`; supply only one of the two, \
                 even when the values agree"
                    .into(),
            ));
        }
        if let Some(id) = map.remove("id") {
            map.insert("ids".into(), Value::Array(vec![id]));
        }
    }
    let p: MarkReadParams = deser(params)?;
    let (requested_count, targets) =
        validate_bulk_read_targets(runtime, token, p.ids, "mark_read").await?;
    if p.atomic {
        mark_read_targets_atomic(runtime, token, requested_count, targets).await
    } else {
        mark_read_targets_best_effort(runtime, token, requested_count, targets, false).await
    }
}
