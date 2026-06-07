//! Verb handler implementations for the comm pack.
//!
//! All five verbs (`send`, `inbox`, `read`, `reply`, `thread`) store and query
//! `message` notes in the standard notes table. Message-specific metadata lives
//! in the `properties` JSON column; `content` is the message body.

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

use crate::message::{dual_write_message, note_to_message_json, resolve_id, short_id};
use crate::params::{deser, InboxParams, ReadParams, ReplyParams, SendParams, ThreadParams};

/// `send` — create a message note in the caller's namespace (outbound) AND the
/// recipient's namespace (inbound).
///
/// Two writes are made atomically via `dual_write_message`: if the inbound write
/// fails the outbound note is deleted before returning the error. When sender and
/// recipient are the same namespace both copies are written to the caller's namespace
/// (one outbound, one inbound) so that `inbox()` surfaces self-sent messages.
pub(crate) async fn handle_send(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: SendParams = deser(params)?;
    if p.to.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "send: `to` must not be empty".into(),
        ));
    }
    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "send: `content` must not be empty".into(),
        ));
    }
    // Validate thread_id is a well-formed UUID when supplied (thread_id is a root UUID).
    if let Some(ref tid) = p.thread_id {
        if tid.parse::<Uuid>().is_err() {
            return Err(RuntimeError::InvalidInput(format!(
                "send: `thread_id` must be a valid UUID, got: {tid:?}"
            )));
        }
    }

    let from = token.namespace().as_str().to_string();
    let sent_at = Utc::now().to_rfc3339();

    let outbound_note = dual_write_message(
        runtime,
        token,
        &from,
        &p.to,
        p.subject.as_deref(),
        &p.content,
        p.thread_id.as_deref(),
        &sent_at,
    )
    .await?;

    Ok(json!({
        "id": short_id(outbound_note.id),
        "full_id": outbound_note.id.as_hyphenated().to_string(),
        "from": from,
        "to": p.to,
        "subject": p.subject,
        "sent_at": sent_at,
    }))
}

/// `inbox` — list inbound messages for the caller namespace.
///
/// Implements a paginated scan so that matching messages are never lost when
/// the newest unfiltered page contains no inbound rows. Each page fetches up
/// to PAGE_SIZE messages; scanning stops when `limit` filtered rows are
/// collected or the store is exhausted.
pub(crate) async fn handle_inbox(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: InboxParams = deser(params)?;
    let raw_limit = p.limit.unwrap_or(20);
    if raw_limit == 0 {
        return Ok(json!({ "messages": [], "count": 0 }));
    }
    let limit = raw_limit.clamp(1, 200) as usize;

    let status = match p.status.as_deref().unwrap_or("unread") {
        s @ ("unread" | "read" | "all") => s,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "inbox: invalid status {other:?}; expected one of: unread, read, all"
            )));
        }
    };

    // Push direction + read-status filters into SQL so idx_comm_message_direction is usable.
    // Read filter uses json_type to match the old as_bool().unwrap_or(false) semantics:
    // only JSON boolean `true` counts as read; missing/false/string/integer all count as unread.
    let mut property_filters = vec![PropertyFilter {
        json_path: "$.direction".to_string(),
        op: FilterOp::Eq,
        value: SqlValue::Text("inbound".to_string()),
    }];
    match status {
        "unread" => property_filters.push(PropertyFilter {
            json_path: "$.read".to_string(),
            op: FilterOp::JsonTypeNeMissing,
            value: SqlValue::Text("true".to_string()),
        }),
        "read" => property_filters.push(PropertyFilter {
            json_path: "$.read".to_string(),
            op: FilterOp::JsonTypeEq,
            value: SqlValue::Text("true".to_string()),
        }),
        _ => {} // "all" — no read-status filter
    }
    let filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        order_by: None, // preserves existing created_at DESC ordering
    };
    let page = runtime
        .notes(token)?
        .query_notes_filtered(
            token.namespace().as_str(),
            &filter,
            PageRequest {
                limit: limit as u32,
                offset: 0,
            },
        )
        .await?;
    let messages: Vec<Value> = page.items.iter().map(note_to_message_json).collect();
    let count = messages.len();
    Ok(json!({ "messages": messages, "count": count }))
}

/// `read` — mark a message as read.
pub(crate) async fn handle_read(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ReadParams = deser(params)?;
    let id = resolve_id(runtime, token, &p.id, "read").await?;

    let store = runtime.notes(token)?;
    let mut note = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("read: get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("read: message {id} not found")))?;

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
        return Err(RuntimeError::InvalidInput(format!(
            "read: cannot mark outbound message {id} as read (direction=outbound); \
             read() is a recipient action for inbound messages only"
        )));
    }

    // Merge `read: true` into properties.
    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    props["read"] = json!(true);
    note.properties = Some(props.clone());
    note.updated_at = Utc::now().timestamp_micros();

    store
        .upsert_note(note)
        .await
        .map_err(|e| RuntimeError::Internal(format!("read: upsert_note: {e}")))?;

    Ok(
        json!({ "id": short_id(id), "full_id": id.as_hyphenated().to_string(), "read": true, "properties": props }),
    )
}

/// `reply` — reply to a message, threading linkage.
pub(crate) async fn handle_reply(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ReplyParams = deser(params)?;
    let id = resolve_id(runtime, token, &p.id, "reply").await?;
    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "reply: `content` must not be empty".into(),
        ));
    }

    let store = runtime.notes(token)?;
    let original = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("reply: get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("reply: message {id} not found")))?;

    if original.namespace != token.namespace().as_str() {
        return Err(RuntimeError::NotFound(format!(
            "reply: message {id} not found"
        )));
    }
    if original.kind != "message" {
        return Err(RuntimeError::InvalidInput(format!(
            "reply: note {id} is kind {:?}, expected \"message\"",
            original.kind
        )));
    }

    let orig_props = original
        .properties
        .as_ref()
        .cloned()
        .unwrap_or_else(|| json!({}));

    // UE6-H2: thread_id must always be a full 36-char hyphenated UUID.
    // If the stored thread_id is a valid full UUID, use it; otherwise fall
    // back to the original message's own full UUID as the thread root.
    let thread_id = orig_props
        .get("thread_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .map(|u| u.as_hyphenated().to_string())
        .unwrap_or_else(|| original.id.as_hyphenated().to_string());

    let original_from = orig_props
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let original_to = orig_props
        .get("to")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let original_subject = orig_props
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let reply_subject = if original_subject.starts_with("Re: ") || original_subject.is_empty() {
        original_subject.clone()
    } else {
        format!("Re: {original_subject}")
    };

    let from = token.namespace().as_str().to_string();
    let sent_at = Utc::now().to_rfc3339();

    // UE6-H1: route reply to the "other party" — not always to the original sender.
    // If the reply caller is the original sender (from), route to the original
    // recipient (to). If the reply caller is the original recipient, route back
    // to the original sender. This ensures both A→B and B→A reply correctly.
    let reply_to = if from == original_from {
        // Caller was the sender of the original; reply goes to the original recipient.
        original_to.clone()
    } else {
        // Caller was the recipient (or a third party); reply goes to the original sender.
        original_from.clone()
    };

    let reply_subject_opt = if reply_subject.is_empty() {
        None
    } else {
        Some(reply_subject.as_str())
    };

    // dual_write_message writes outbound to caller namespace and inbound to
    // recipient namespace, matching the same delivery semantics as `send`.
    let reply_note = dual_write_message(
        runtime,
        token,
        &from,
        &reply_to,
        reply_subject_opt,
        &p.content,
        Some(&thread_id),
        &sent_at,
    )
    .await?;

    Ok(json!({
        "id": short_id(reply_note.id),
        "full_id": reply_note.id.as_hyphenated().to_string(),
        "thread_id": thread_id,
        "from": from,
        "to": reply_to,
        "subject": reply_subject,
        "sent_at": sent_at,
    }))
}

/// `thread` — retrieve all messages in a conversation thread, ordered chronologically.
///
/// Returns the originating message (the one whose `id` matches the `thread_id`
/// root) plus all messages whose `properties.thread_id` equals the root UUID,
/// ordered by `created_at` ascending (chronological).
///
/// Cross-namespace thread resolution: when the resolved note carries a `thread_id`
/// in its properties that differs from its own UUID, that stored `thread_id` IS the
/// canonical root (e.g., this is an inbound copy of the root, or a non-root message).
/// `comm.thread` resolves to that canonical root so that `thread(id=id_A)` and
/// `thread(id=id_B)` both return the full conversation regardless of which copy UUID
/// the caller holds.
///
/// The root ID is validated: it must exist in the caller namespace and its
/// `kind` must be `"message"`. A full UUID that does not resolve, belongs to a
/// different namespace, or has the wrong kind returns an error — the same
/// behaviour as `read()` and `reply()`.
pub(crate) async fn handle_thread(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ThreadParams = deser(params)?;
    let limit = p.limit.unwrap_or(100).clamp(1, 500) as usize;

    // Resolve and validate the passed ID.
    let passed_uuid = resolve_id(runtime, token, &p.id, "thread").await?;

    let canonical_thread_id: String = {
        let store = runtime.notes(token)?;
        let note = store
            .get_note(passed_uuid)
            .await
            .map_err(|e| RuntimeError::Internal(format!("thread: get_note: {e}")))?
            .ok_or_else(|| {
                RuntimeError::NotFound(format!("thread: message {passed_uuid} not found"))
            })?;

        if note.namespace != token.namespace().as_str() {
            return Err(RuntimeError::NotFound(format!(
                "thread: message {passed_uuid} not found"
            )));
        }
        if note.kind != "message" {
            return Err(RuntimeError::InvalidInput(format!(
                "thread: note {passed_uuid} is kind {:?}, expected \"message\"",
                note.kind
            )));
        }

        // Cross-namespace root resolution: if the note's properties.thread_id is a
        // valid full UUID that differs from the note's own UUID, use that as the
        // canonical thread_id.  This handles the case where the caller holds an
        // inbound copy UUID (id_B) but the canonical root is the outbound UUID (id_A).
        // Both copies were written with the same canonical thread_id by dual_write_message.
        match note
            .properties
            .as_ref()
            .and_then(|p| p.get("thread_id"))
            .and_then(Value::as_str)
            .filter(|s| s.len() == 36)
            .and_then(|s| s.parse::<Uuid>().ok())
        {
            Some(stored_root) if stored_root != passed_uuid => {
                stored_root.as_hyphenated().to_string()
            }
            _ => passed_uuid.as_hyphenated().to_string(),
        }
    };

    // Push thread_id predicate into SQL so idx_comm_message_thread can be used.
    // The root note always has properties.thread_id == own_uuid == canonical_thread_id
    // (patched by dual_write_message), so it is captured by the same SQL filter as replies.
    let thread_store = runtime.notes(token)?;
    let thread_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.thread_id".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(canonical_thread_id.clone()),
        }],
        order_by: None,
    };
    const PAGE_SIZE: u32 = 200;
    let mut messages: Vec<Value> = Vec::new();
    let mut db_offset: u32 = 0;

    loop {
        let page = thread_store
            .query_notes_filtered(
                token.namespace().as_str(),
                &thread_filter,
                PageRequest {
                    limit: PAGE_SIZE,
                    offset: db_offset.into(),
                },
            )
            .await?;
        let fetched = page.items.len() as u32;
        for n in &page.items {
            messages.push(note_to_message_json(n));
        }
        if fetched < PAGE_SIZE {
            break;
        }
        db_offset += PAGE_SIZE;
    }

    // Sort chronologically ascending (earliest first).
    // ISO 8601 timestamps (e.g. "2026-05-27T10:30:00.000000Z") are lexicographically
    // ordered, so string comparison is correct and cheaper than parsing.
    messages.sort_by(|a, b| {
        let a_ts = a.get("created_at").and_then(Value::as_str).unwrap_or("");
        let b_ts = b.get("created_at").and_then(Value::as_str).unwrap_or("");
        a_ts.cmp(b_ts)
    });
    messages.truncate(limit);
    let count = messages.len();

    Ok(json!({
        "thread_id": canonical_thread_id,
        "count": count,
        "messages": messages,
    }))
}
