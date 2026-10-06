use std::collections::{HashMap, HashSet};

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
use khive_storage::types::SqlValue;

#[cfg(test)]
use super::read_cluster_tests;
use super::validation::{
    addressed_recipient, caller_inherits_legacy_pool, caller_is_addressee, legacy_recipient,
};
use crate::message::{note_to_message_json, resolve_id, short_id};

const MAX_BULK_READ_IDS: usize = 500;

const BULK_READ_WINDOW: usize = 128;

pub(super) async fn validate_bulk_read_targets(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw_ids: Vec<String>,
    verb: &str,
) -> Result<(usize, Vec<(Uuid, Note)>), RuntimeError> {
    if raw_ids.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `ids` must contain at least one message id"
        )));
    }
    if raw_ids.len() > MAX_BULK_READ_IDS {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `ids` accepts at most {MAX_BULK_READ_IDS} message ids, got {}",
            raw_ids.len()
        )));
    }

    let requested_count = raw_ids.len();
    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(requested_count);
    let mut index = 0;
    while index < requested_count {
        // Lookahead parses only complete UUIDs. A prefix or invalid spelling
        // stops the window, and keeps its resolution at the original phase.
        let ids: Vec<Uuid> = raw_ids[index..]
            .iter()
            .take(BULK_READ_WINDOW)
            .map(|raw| raw.parse::<Uuid>())
            .take_while(Result::is_ok)
            .map(Result::unwrap)
            .collect();
        if ids.is_empty() {
            let (id, note) = validate_read_target(runtime, token, &raw_ids[index]).await?;
            if seen.insert(id) {
                targets.push((id, note));
            }
            index += 1;
            continue;
        }

        let store = runtime.notes(token)?;
        match store.get_notes_batch(&ids).await {
            Ok(notes) => {
                let notes: HashMap<Uuid, Note> =
                    notes.into_iter().map(|note| (note.id, note)).collect();
                #[cfg(test)]
                read_cluster_tests::observe_phase(read_cluster_tests::Phase::BatchRead(
                    ids.clone(),
                ))
                .await;
                for id in &ids {
                    let note = notes.get(id).cloned().ok_or_else(|| {
                        RuntimeError::NotFound(format!("read: message {id} not found"))
                    })?;
                    let (id, note) = validate_read_note(token, *id, note)?;
                    if seen.insert(id) {
                        targets.push((id, note));
                    }
                }
            }
            Err(_) => {
                // A batch can decode a later bad row before an earlier
                // missing/ineligible target. Replay this exact window with
                // the original point reads to retain its first error/text.
                for raw in &raw_ids[index..index + ids.len()] {
                    let (id, note) = validate_read_target(runtime, token, raw).await?;
                    if seen.insert(id) {
                        targets.push((id, note));
                    }
                }
            }
        }
        index += ids.len();
        #[cfg(test)]
        read_cluster_tests::observe_phase(read_cluster_tests::Phase::ValidatedWindow(ids)).await;
    }
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ValidatedAll).await;
    Ok((requested_count, targets))
}

pub(super) async fn mark_read_targets_best_effort(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    requested_count: usize,
    targets: Vec<(Uuid, Note)>,
    include_body: bool,
) -> Result<Value, RuntimeError> {
    // Read every target's fields before marking any target, so a failed field
    // lookup cannot leave earlier messages marked as read.
    let attachment_fields: Vec<Option<Value>> = if include_body {
        let ids: Vec<_> = targets.iter().map(|(id, _)| *id).collect();
        crate::file_attachments::metadata_many(runtime, &ids)
            .await?
            .into_iter()
            .map(Some)
            .collect()
    } else {
        vec![None; targets.len()]
    };
    let mut prepared_targets = Vec::with_capacity(targets.len());
    for ((id, note), fields) in targets.into_iter().zip(attachment_fields) {
        let message = fields.map(|fields| read_message_fields_prepared(&note, fields));
        prepared_targets.push((id, note, message));
    }

    let mut results = Vec::with_capacity(prepared_targets.len());
    for (id, note, message) in prepared_targets {
        let original_properties = note.properties.clone();
        match mark_read_target(runtime, token, id, note).await {
            Ok(result) => results.push(read_result_with_body(result, message)),
            Err(error) => {
                let (status, read) = match &error {
                    RuntimeError::Storage(storage_error) => {
                        match classify_mark_read_error(storage_error) {
                            MarkReadFailure::Unknown => ("unknown", Value::Null),
                            MarkReadFailure::Failed => ("failed", json!(false)),
                        }
                    }
                    _ => ("failed", json!(false)),
                };
                results.push(json!({
                    "id": short_id(id),
                    "full_id": id.as_hyphenated().to_string(),
                    "status": status,
                    "read": read,
                    "mark_error": error.to_string(),
                    "properties": original_properties,
                }));
            }
        }
    }
    Ok(bulk_read_response(requested_count, results))
}

pub(super) async fn read_message_fields(
    runtime: &KhiveRuntime,
    note: &Note,
) -> Result<Value, RuntimeError> {
    let attachment_fields = crate::file_attachments::metadata(runtime, note.id).await?;
    Ok(read_message_fields_prepared(note, attachment_fields))
}

fn read_message_fields_prepared(note: &Note, attachment_fields: Value) -> Value {
    let message = note_to_message_json(note);
    let mut fields = json!({
        "subject": message["subject"],
        "content": message["content"],
        "from": message["from"],
        "to": message["to"],
        "direction": message["direction"],
        "created_at": message["created_at"],
    });
    if let (Some(fields), Value::Object(attachment_fields)) =
        (fields.as_object_mut(), attachment_fields)
    {
        fields.extend(attachment_fields);
    }
    fields
}

pub(super) fn read_result_with_body(mut result: Value, message: Option<Value>) -> Value {
    if result["status"] == "success" {
        if let (Some(response), Some(Value::Object(message))) = (result.as_object_mut(), message) {
            response.extend(message);
        }
    }
    result
}

pub(super) async fn mark_read_targets_atomic(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    requested_count: usize,
    targets: Vec<(Uuid, Note)>,
) -> Result<Value, RuntimeError> {
    // Fixed dotted-path patch (`$.read`) — no caller input reaches the
    // top-level properties object, so the reserved key is unreachable here.
    let store = runtime.notes(token)?;
    let ids = targets.iter().map(|(id, _)| *id).collect();
    store
        .patch_note_property_atomic(
            ids,
            token.namespace().as_str(),
            &read_recheck_filter(token),
            "$.read",
            json!(true),
            Utc::now().timestamp_micros(),
        )
        .await?;

    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::AtomicCommitted).await;
    let mut results = Vec::with_capacity(targets.len());
    for window in targets.chunks(BULK_READ_WINDOW) {
        let ids: Vec<Uuid> = window.iter().map(|(id, _)| *id).collect();
        let fresh = store.get_notes_batch(&ids).await;
        let fresh = fresh.map(|notes| {
            notes
                .into_iter()
                .map(|note| (note.id, note))
                .collect::<HashMap<Uuid, Note>>()
        });
        for (id, note) in window {
            let latest = match &fresh {
                Ok(notes) => notes.get(id).cloned(),
                // The mutation is already committed: an unreadable batch is
                // retried only as fresh scalar reads, never as a mutation.
                Err(_) => store.get_note(*id).await.ok().flatten(),
            };
            let properties = match latest {
                Some(fresh) => fresh.properties.unwrap_or_else(|| json!({})),
                None => {
                    let mut fallback = note.properties.clone().unwrap_or_else(|| json!({}));
                    fallback["read"] = json!(true);
                    fallback
                }
            };
            results.push(json!({
                "id": short_id(*id),
                "full_id": id.as_hyphenated().to_string(),
                "status": "success",
                "read": true,
                "properties": properties,
            }));
        }
    }
    Ok(bulk_read_response(requested_count, results))
}

pub(super) fn bulk_read_response(requested_count: usize, results: Vec<Value>) -> Value {
    let marked_count = results
        .iter()
        .filter(|result| result["read"].as_bool() == Some(true))
        .count();
    let unknown_count = results
        .iter()
        .filter(|result| result["status"].as_str() == Some("unknown"))
        .count();
    let unique_count = results.len();
    let failed_count = unique_count - marked_count - unknown_count;
    let status = if failed_count == 0 && unknown_count == 0 {
        "success"
    } else if marked_count == 0 && unknown_count == 0 {
        "failed"
    } else if marked_count == 0 && failed_count == 0 {
        "unknown"
    } else {
        "partial"
    };
    json!({
        "results": results,
        "status": status,
        "requested_count": requested_count,
        "unique_count": unique_count,
        "marked_count": marked_count,
        "unknown_count": unknown_count,
        "failed_count": failed_count,
    })
}

pub(super) async fn validate_read_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
) -> Result<(Uuid, Note), RuntimeError> {
    let id = resolve_id(runtime, token, raw, "read").await?;

    let store = runtime.notes(token)?;
    let note = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("read: get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("read: message {id} not found")))?;

    validate_read_note(token, id, note)
}

fn validate_read_note(
    token: &NamespaceToken,
    id: Uuid,
    note: Note,
) -> Result<(Uuid, Note), RuntimeError> {
    if note.namespace != token.namespace().as_str() {
        return Err(RuntimeError::NotFound(format!(
            "read: message {id} not found"
        )));
    }
    if note.kind != "message" {
        return Err(RuntimeError::InvalidInput(format!(
            "read: note {id} is kind {:?}, expected \"message\"",
            note.kind
        )));
    }

    // Reject read() on outbound messages — "read" is a recipient action.
    // Marking an outbound (sent) message as read corrupts the read/unread
    // invariant and has no semantic meaning to the sender.
    let direction = note
        .properties
        .as_ref()
        .and_then(|p| p.get("direction"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if direction == "outbound" {
        let caller_actor = token.actor().id.as_str();
        let properties = note.properties.as_ref();
        let from_actor = properties
            .and_then(|p| p.get("from_actor"))
            .and_then(Value::as_str);
        let is_participant = addressed_recipient(properties)
            .is_some_and(|recipient| recipient == caller_actor || from_actor == Some(caller_actor))
            || (caller_inherits_legacy_pool(token) && legacy_recipient(properties));
        if !is_participant {
            return Err(RuntimeError::InvalidInput(format!(
                "read: that message is not addressed to caller actor {caller_actor:?}"
            )));
        }
        return Err(RuntimeError::InvalidInput(format!(
            "read: message {id} is outbound; only received (inbound) messages can be marked as read"
        )));
    }

    let caller_actor = token.actor().id.as_str();
    if !caller_is_addressee(token, note.properties.as_ref()) {
        return Err(RuntimeError::InvalidInput(format!(
            "read: that message is not addressed to caller actor {caller_actor:?}"
        )));
    }

    Ok((id, note))
}

pub(super) fn read_recheck_filter(token: &NamespaceToken) -> NoteFilter {
    let include_legacy = caller_inherits_legacy_pool(token);
    let mut property_filters = vec![
        PropertyFilter {
            json_path: "$.direction".to_string(),
            op: FilterOp::NotInOrMissing(vec![SqlValue::Text("outbound".to_string())]),
            value: SqlValue::Null,
        },
        PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op: if include_legacy {
                FilterOp::EqOrMissing
            } else {
                FilterOp::Eq
            },
            value: SqlValue::Text(token.actor().id.clone()),
        },
    ];
    if !include_legacy {
        property_filters.push(PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op: FilterOp::JsonTypeEq,
            value: SqlValue::Text("text".to_string()),
        });
    }
    NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        ..Default::default()
    }
}

/// Whether a mark-read write error means the patch definitely did not apply,
/// or whether the writer seam terminated after accepting the request with no
/// way to tell whether it landed.
enum MarkReadFailure {
    Failed,
    Unknown,
}

/// Classify a mark-read storage error by its real variant, never by its
/// display text. `SideEffectsUnknown` means the write was accepted before its
/// execution seam terminated — the patch may already be committed, so it must
/// not be reported as a definite failure. Same match shape as
/// `message::attach_outbound_id_to_ambiguous_write`'s dual-write classifier.
fn classify_mark_read_error(error: &khive_storage::StorageError) -> MarkReadFailure {
    match error {
        khive_storage::StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        } => MarkReadFailure::Unknown,
        _ => MarkReadFailure::Failed,
    }
}

pub(super) async fn mark_read_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    note: Note,
) -> Result<Value, RuntimeError> {
    // Patch via one atomic JSON-property `UPDATE`, not a get/replace cycle or
    // `upsert_note` (#1483, #780). See docs/api/message-lifecycle.md#handlersrshandle_read
    let store = runtime.notes(token)?;

    // `orig_props` is kept as the stored `Option<Value>` (a SQL-NULL
    // properties column is a real, distinct state from `{}`) so a degraded
    // response can report exactly what is stored.
    let orig_props = note.properties.clone();
    let updated_at = Utc::now().timestamp_micros();

    // Storage-side compare-and-swap: patches only the `$.read` key via
    // `json_set` instead of overwriting the whole `properties` column with
    // this call's snapshot (which bulk read's up-to-500-target
    // validate-then-mark window can leave stale — a concurrent write to any
    // other property between validation and this call must survive), and
    // rechecks kind/direction/addressee against the row's *current* state in
    // the same `UPDATE` — the same eligibility predicate
    // `validate_read_target` already checked, re-evaluated at mutation time
    // rather than trusted from an earlier read.
    let recheck_filter = read_recheck_filter(token);

    // Best-effort: under multi-client writer contention the pool checkout can
    // time out. Keep the failed or indeterminate mark in the response so the
    // caller can retry or re-check state. Mirrors handle_reply's
    // fold-in mark-read: `Ok(false)` (no live row currently matches, e.g.
    // soft-deleted or an eligibility property changed mid-flight) and `Err`
    // both degrade to `read: false` + `mark_error` instead of failing the
    // response. A caller polling unread counts simply sees the message still
    // unread and can re-issue `read` — self-healing, no retry loop needed here.
    // Fixed dotted-path patch (`$.read`) — no caller input reaches the
    // top-level properties object, so the reserved key is unreachable here.
    let patch_result = store
        .try_patch_note_property(
            id,
            token.namespace().as_str(),
            &recheck_filter,
            "$.read",
            json!(true),
            updated_at,
        )
        .await;

    // Only a successful patch needs the fresh row: `read_response`'s
    // `Ok(false)`/`Err` arms report `orig_props` (what is still stored), not
    // this value.
    let patched_properties = if matches!(patch_result, Ok(true)) {
        match store.get_note(id).await {
            Ok(Some(fresh)) => fresh.properties.unwrap_or_else(|| json!({})),
            _ => {
                let mut fallback = orig_props.clone().unwrap_or_else(|| json!({}));
                fallback["read"] = json!(true);
                fallback
            }
        }
    } else {
        Value::Null
    };

    Ok(read_response(
        short_id(id),
        id.as_hyphenated().to_string(),
        patch_result,
        orig_props,
        patched_properties,
    ))
}

/// Assemble `comm.read`'s response from the mark-read patch outcome.
///
/// Factored out so the three degrade arms (`Ok(true)`, `Ok(false)`, `Err`)
/// are unit-testable directly: the `Ok(false)`/soft-delete-mid-flight race
/// cannot be arranged honestly through the public dispatch path (`handle_read`
/// fetches and patches within a single sequential call, with no seam to
/// inject a concurrent delete between the two), so the response shape is
/// verified against this pure function instead of a racing integration test.
pub(super) fn read_response(
    short: String,
    full: String,
    patch_result: Result<bool, khive_storage::StorageError>,
    original_properties: Option<Value>,
    patched_properties: Value,
) -> Value {
    match patch_result {
        Ok(true) => json!({
            "id": short,
            "full_id": full,
            "status": "success",
            "read": true,
            "properties": patched_properties,
        }),
        Ok(false) => json!({
            "id": short,
            "full_id": full,
            "status": "failed",
            "read": false,
            "mark_error": "no live row updated",
            "properties": original_properties,
        }),
        Err(e) => {
            tracing::warn!(
                id = %full,
                error = %e,
                "comm mark-read: update failed under writer contention; \
                 degrading (best-effort)"
            );
            match classify_mark_read_error(&e) {
                MarkReadFailure::Unknown => json!({
                    "id": short,
                    "full_id": full,
                    "status": "unknown",
                    "read": Value::Null,
                    "mark_error": e.to_string(),
                    "properties": original_properties,
                }),
                MarkReadFailure::Failed => json!({
                    "id": short,
                    "full_id": full,
                    "status": "failed",
                    "read": false,
                    "mark_error": e.to_string(),
                    "properties": original_properties,
                }),
            }
        }
    }
}
