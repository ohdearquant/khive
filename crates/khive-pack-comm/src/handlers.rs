//! Verb handler implementations for the comm pack.
//!
//! All five verbs (`send`, `inbox`, `read`, `reply`, `thread`) store and query
//! `message` notes in the standard notes table. Message-specific metadata lives
//! in the `properties` JSON column; `content` is the message body.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

use crate::message::{dual_write_message, note_to_message_json, resolve_id, short_id};
use crate::params::{
    deser, CursorCommitParams, CursorGetParams, HeartbeatParams, InboxParams, IngestParams,
    ProbeParams, ReadParams, ReplyParams, SendParams, ThreadParams,
};

/// Validate an actor label: non-empty, no control characters, ≤255 bytes (ADR-057 Q1 loose).
fn validate_actor_label(verb: &str, label: &str, field: &str) -> Result<(), RuntimeError> {
    if label.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must not be empty"
        )));
    }
    if label.len() > 255 {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must not exceed 255 bytes"
        )));
    }
    if label.chars().any(|c| c.is_control()) {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must not contain control characters"
        )));
    }
    Ok(())
}

/// `send` — create a message note in the caller's namespace (outbound) AND
/// deliver an inbound copy addressed to the actor label in `to` (ADR-057).
/// Both copies land in the caller's namespace; no cross-namespace write occurs.
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_send
pub(crate) async fn handle_send(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: SendParams = deser(params)?;
    validate_actor_label("send", &p.to, "to")?;
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

    let caller_ns = token.namespace().as_str().to_string();
    let from_actor = token.actor().id.clone();
    let to_actor = p.to.trim().to_string();

    // #820: reject a target that collapses onto the sender's own actor identity
    // unless self_send=true — usually a sub-agent/parent mis-resolution, not intent.
    // "local" is exempt (anonymous single-tenant party-line default).
    // See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_send
    if to_actor == from_actor && to_actor != "local" && !p.self_send {
        return Err(RuntimeError::InvalidInput(format!(
            "send: `to` ({to_actor:?}) resolves to the sender's own actor identity \
             ({from_actor:?}); refusing to silently self-address (issue #820). If you intended \
             to reach a distinct actor (e.g. a sub-agent addressing its parent orchestrator), \
             the sender's actor identity collapsed onto the same value as the named target -- \
             sessions spawned in the same project scope resolve `[actor] id` from the same \
             worktree-scoped `.khive/config.toml`, so they are not addressable as distinct \
             principals until each is configured with its own actor identity. If this send is \
             genuinely a note to yourself, resend with `self_send=true`."
        )));
    }

    // #200: unattributed callers stamp from_actor="local", corrupting reply-thread
    // routing; warn (don't hard-error, for back-compat) rather than silently proceed.
    if khive_runtime::actor_is_unattributed(token.actor()) && to_actor != "local" {
        tracing::warn!(
            to_actor = %to_actor,
            "comm.send: unattributed caller (actor.id not configured) sending to a specific \
             actor label; from_actor will be stamped 'local', corrupting attribution and \
             reply-thread routing in multi-actor deployments. \
             Set [actor] id in khive.toml to fix (issue #200)."
        );
    }

    let sent_at = Utc::now().to_rfc3339();

    // Pass caller_ns as both `from` and `to` so `from == recipient_ns_str` in
    // dual_write_message, naturally bypassing the cross-namespace allowlist gate
    // (ADR-057 §"Interaction with ADR-040"). Actor labels are stored via from_actor/to_actor.
    let outbound_note = dual_write_message(
        runtime,
        token,
        &caller_ns,
        &caller_ns,
        p.subject.as_deref(),
        &p.content,
        p.thread_id.as_deref(),
        &sent_at,
        Some(&from_actor),
        Some(&to_actor),
        None,
        None,
        p.tags.as_deref(),
    )
    .await?;

    Ok(json!({
        "id": short_id(outbound_note.id),
        "full_id": outbound_note.id.as_hyphenated().to_string(),
        "from": from_actor,
        "to": p.to,
        "subject": p.subject,
        "sent_at": sent_at,
    }))
}

/// `inbox` — list inbound messages for the caller's actor label (ADR-057).
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_inbox
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

    // #493: from_actor / from_prefix sender filter — mutually exclusive.
    if p.from_actor.is_some() && p.from_prefix.is_some() {
        return Err(RuntimeError::InvalidInput(
            "inbox: `from_actor` and `from_prefix` are mutually exclusive".into(),
        ));
    }

    let status = match p.status.as_deref().unwrap_or("unread") {
        s @ ("unread" | "read" | "all") => s,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "inbox: invalid status {other:?}; expected one of: unread, read, all"
            )));
        }
    };

    let caller_actor = token.actor().id.clone();

    // Push direction + read-status into SQL for idx_comm_message_direction; json_type
    // read-check keeps only JSON boolean `true` as read (matches prior as_bool semantics).
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

    // ADR-057 Q3: to_actor filter, EqOrMissing so legacy to_actor-less messages stay
    // visible; closes the #199 multi-actor read leak for non-"local" callers.
    property_filters.push(PropertyFilter {
        json_path: "$.to_actor".to_string(),
        op: FilterOp::EqOrMissing,
        value: SqlValue::Text(caller_actor.clone()),
    });

    let filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        order_by: None, // preserves existing created_at DESC ordering
        ..Default::default()
    };
    let store = runtime.notes(token)?;

    // #493: `FilterOp` has no prefix-match op, so a sender filter is applied in Rust
    // over paged results (see docs/api/message-lifecycle.md#handlersrshandle_inbox) instead of SQL.
    let messages: Vec<Value> = if p.from_actor.is_some() || p.from_prefix.is_some() {
        const PAGE_SIZE: u32 = 200;
        let mut collected: Vec<Value> = Vec::new();
        let mut db_offset: u32 = 0;
        loop {
            let page = store
                .query_notes_filtered(
                    token.namespace().as_str(),
                    &filter,
                    PageRequest {
                        limit: PAGE_SIZE,
                        offset: db_offset.into(),
                    },
                )
                .await?;
            let fetched = page.items.len() as u32;
            for n in &page.items {
                let sender = n
                    .properties
                    .as_ref()
                    .and_then(|props| props.get("from_actor"))
                    .and_then(Value::as_str);
                let matches = match (p.from_actor.as_deref(), p.from_prefix.as_deref()) {
                    (Some(exact), None) => sender == Some(exact),
                    (None, Some(prefix)) => sender.map(|s| s.starts_with(prefix)).unwrap_or(false),
                    _ => unreachable!("mutual exclusion already validated above"),
                };
                if matches {
                    collected.push(note_to_message_json(n));
                    if collected.len() >= limit {
                        break;
                    }
                }
            }
            if collected.len() >= limit || fetched < PAGE_SIZE {
                break;
            }
            db_offset += PAGE_SIZE;
        }
        collected
    } else {
        let page = store
            .query_notes_filtered(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    limit: limit as u32,
                    offset: 0,
                },
            )
            .await?;
        page.items.iter().map(note_to_message_json).collect()
    };
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
    let note = store
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
            "read: message {id} is outbound; only received (inbound) messages can be marked as read"
        )));
    }

    // Patch via a real `UPDATE`, not `upsert_note`'s `INSERT OR REPLACE` (#780
    // silently re-inserts the row on conflict). See docs/api/message-lifecycle.md#handlersrshandle_read
    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    props["read"] = json!(true);
    let updated_at = Utc::now().timestamp_micros();

    store
        .update_note_properties(id, Some(props.clone()), updated_at)
        .await
        .map_err(|e| RuntimeError::Internal(format!("read: update_note_properties: {e}")))?;

    Ok(
        json!({ "id": short_id(id), "full_id": id.as_hyphenated().to_string(), "read": true, "properties": props }),
    )
}

/// `reply` — reply to a message, threading linkage. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_reply
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

    // Issue #403: parent's wire Message-ID drives In-Reply-To/References for native
    // mail clients. `None` when the parent has none — see docs/api/message-lifecycle.md.
    let in_reply_to_message_id = parent_wire_message_id(&orig_props);

    // References carries the FULL ancestor chain per RFC 5322, not just the parent.
    let references_chain = in_reply_to_message_id.as_deref().map(|parent_mid| {
        build_references_header(parent_references_chain(&orig_props), parent_mid)
    });

    // UE6-H2: thread_id must be a full 36-char hyphenated UUID; falls back to the
    // original message's own UUID as thread root when the stored value isn't one.
    let thread_id = orig_props
        .get("thread_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .map(|u| u.as_hyphenated().to_string())
        .unwrap_or_else(|| original.id.as_hyphenated().to_string());

    // ADR-057: prefer from_actor/to_actor; fall back to from/to for legacy messages.
    let original_from_actor = orig_props
        .get("from_actor")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let original_to_actor = orig_props
        .get("to_actor")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let original_from = original_from_actor
        .as_deref()
        .unwrap_or_else(|| orig_props.get("from").and_then(Value::as_str).unwrap_or(""))
        .to_string();

    let original_to = original_to_actor
        .as_deref()
        .unwrap_or_else(|| orig_props.get("to").and_then(Value::as_str).unwrap_or(""))
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

    let caller_ns = token.namespace().as_str().to_string();
    let from_actor_label = token.actor().id.clone();
    let sent_at = Utc::now().to_rfc3339();

    // UE6-H1: route to the "other party" — not always the original sender.
    let reply_to = if from_actor_label == original_from {
        original_to.clone()
    } else {
        original_from.clone()
    };

    // ADR-057: always set from_actor/to_actor on replies (fail-closed on cross-namespace
    // write) — both copies land in the caller's namespace regardless of legacy labels.
    let reply_from_actor = from_actor_label.clone();
    let reply_to_actor = reply_to.clone();

    let reply_subject_opt = if reply_subject.is_empty() {
        None
    } else {
        Some(reply_subject.as_str())
    };

    // Pass caller_ns as both `from` and `to` so `from == recipient_ns_str` in
    // dual_write_message, naturally bypassing the cross-namespace allowlist gate
    // (ADR-057 §"Interaction with ADR-040"). Actor labels are stored via from_actor/to_actor.
    let reply_note = dual_write_message(
        runtime,
        token,
        &caller_ns,
        &caller_ns,
        reply_subject_opt,
        &p.content,
        Some(&thread_id),
        &sent_at,
        Some(&reply_from_actor),
        Some(&reply_to_actor),
        in_reply_to_message_id.as_deref(),
        references_chain.as_deref(),
        p.tags.as_deref(),
    )
    .await?;

    Ok(json!({
        "id": short_id(reply_note.id),
        "full_id": reply_note.id.as_hyphenated().to_string(),
        "thread_id": thread_id,
        "from": from_actor_label,
        "to": reply_to,
        "subject": reply_subject,
        "sent_at": sent_at,
    }))
}

/// `thread` — retrieve all messages in a conversation thread, ordered
/// chronologically: the originating message plus all messages whose
/// `properties.thread_id` equals the root UUID. The root ID is validated: it
/// must exist in the caller namespace and its `kind` must be `"message"`.
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_thread
pub(crate) async fn handle_thread(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ThreadParams = deser(params)?;
    let limit = p.limit.unwrap_or(100).clamp(1, 500) as usize;

    // #494: order — "asc" (default, unchanged) | "desc". Closed set.
    let order = match p.order.as_deref().unwrap_or("asc") {
        o @ ("asc" | "desc") => o,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "thread: invalid order {other:?}; expected one of: asc, desc"
            )));
        }
    };

    // Resolve and validate the passed ID.
    let passed_uuid = resolve_id(runtime, token, &p.id, "thread").await?;

    let (canonical_thread_id, root_note): (String, Note) = {
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

        // Cross-namespace root resolution: use the stored thread_id as canonical root
        // when it differs from the note's own UUID (dual_write_message patches both
        // copies to match); falls back to the note's own UUID otherwise (issue #479b,
        // ADR-040). See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_thread
        let canonical = match note
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
        };
        (canonical, note)
    };

    // Push thread_id predicate into SQL so idx_comm_message_thread can be used.
    let thread_store = runtime.notes(token)?;
    let thread_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.thread_id".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(canonical_thread_id.clone()),
        }],
        order_by: None,
        ..Default::default()
    };
    const PAGE_SIZE: u32 = 200;
    let mut rows: Vec<ThreadRow> = Vec::new();
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
            rows.push(ThreadRow {
                created_at: n.created_at,
                full_id: n.id,
                json: note_to_message_json(n),
            });
        }
        if fetched < PAGE_SIZE {
            break;
        }
        db_offset += PAGE_SIZE;
    }

    // Explicitly include the already-validated root when the SQL filter missed it
    // (issue #479b: a root lacking a `thread_id` property, e.g. legacy/imported data).
    let root_already_present = rows.iter().any(|r| r.full_id == root_note.id);
    if !root_already_present {
        rows.push(ThreadRow {
            created_at: root_note.created_at,
            full_id: root_note.id,
            json: note_to_message_json(&root_note),
        });
    }

    // #494: `after` cursor — message id or RFC 3339 timestamp; a hard error if
    // neither. See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_thread
    let after_cursor: Option<AfterCursor> = match p.after.as_deref() {
        None => None,
        Some(raw) => {
            let looks_like_id = raw.parse::<Uuid>().is_ok()
                || (raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()));
            if looks_like_id {
                let cursor_uuid = resolve_id(runtime, token, raw, "thread").await?;
                let cursor_store = runtime.notes(token)?;
                let cursor_note = cursor_store
                    .get_note(cursor_uuid)
                    .await
                    .map_err(|e| RuntimeError::Internal(format!("thread: get_note (after): {e}")))?
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "thread: `after` cursor {raw:?} does not resolve to a message"
                        ))
                    })?;
                Some(AfterCursor::Id {
                    created_at: cursor_note.created_at,
                    full_id: cursor_note.id,
                })
            } else {
                let micros = chrono::DateTime::parse_from_rfc3339(raw.trim())
                    .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
                    .map_err(|e| {
                        RuntimeError::InvalidInput(format!(
                            "thread: `after` cursor {raw:?} is neither a resolvable message id \
                             nor a valid RFC 3339 timestamp: {e}"
                        ))
                    })?;
                Some(AfterCursor::Timestamp { micros })
            }
        }
    };
    if let Some(cursor) = &after_cursor {
        rows.retain(|r| match cursor {
            // Tuple compare (not timestamp-only) breaks same-microsecond ties by `full_id`.
            AfterCursor::Id {
                created_at,
                full_id,
            } => {
                let row_key = (r.created_at, r.full_id);
                let cursor_key = (*created_at, *full_id);
                match order {
                    // desc "after" means further along the desc sequence (strictly older).
                    "desc" => row_key < cursor_key,
                    _ => row_key > cursor_key,
                }
            }
            AfterCursor::Timestamp { micros } => match order {
                "desc" => r.created_at < *micros,
                _ => r.created_at > *micros,
            },
        });
    }

    // Total order: sort by `(created_at, full_id)`, not timestamp alone, so ties
    // are stable across pages/backends (matches the cursor filter's key above).
    rows.sort_by(|a, b| {
        let a_key = (a.created_at, a.full_id);
        let b_key = (b.created_at, b.full_id);
        match order {
            "desc" => b_key.cmp(&a_key),
            _ => a_key.cmp(&b_key),
        }
    });
    rows.truncate(limit);
    let count = rows.len();
    let messages: Vec<Value> = rows.into_iter().map(|r| r.json).collect();

    Ok(json!({
        "thread_id": canonical_thread_id,
        "count": count,
        "messages": messages,
    }))
}

/// Sort/cursor key (`created_at`, `full_id`) plus rendered message JSON, so
/// `handle_thread` compares exact tuples instead of re-parsing the ISO string.
struct ThreadRow {
    created_at: i64,
    full_id: Uuid,
    json: Value,
}

/// `after` cursor resolved to a comparable key (id cursor: full tie-break tuple;
/// timestamp cursor: parsed microseconds only).
enum AfterCursor {
    Id { created_at: i64, full_id: Uuid },
    Timestamp { micros: i64 },
}

/// `ingest` — write a single inbound message note from a channel adapter.
/// `Visibility::Subhandler`: not accessible via the MCP wire, only callable
/// in-process (e.g. the polling loop in `khive-mcp`); the authoritative write
/// path for all channel-delivered messages. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_ingest
pub(crate) async fn handle_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    // Note: IngestParams does not use deny_unknown_fields.
    let p: IngestParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("ingest: bad params: {e}")))?;

    if p.from.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "ingest: `from` must not be empty".into(),
        ));
    }
    if p.to.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "ingest: `to` must not be empty".into(),
        ));
    }
    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "ingest: `content` must not be empty".into(),
        ));
    }
    // #479a: a non-empty malformed thread_id must fail closed, not silently get a
    // fresh UUID (which would split the message into the wrong conversation).
    if let Some(tid) = p
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if tid.parse::<Uuid>().is_err() {
            return Err(RuntimeError::InvalidInput(format!(
                "ingest: `thread_id` must be a valid UUID, got: {tid:?}"
            )));
        }
    }

    let ns = token.namespace().as_str();
    let store = runtime.notes(token)?;

    // Thread resolution: resolve correlation_external_id to the original message's
    // thread_id + from_actor. Two-query fallback (Message-ID pass, then thread-UUID
    // pass) — see crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_ingest
    let resolved: Option<(String, String)> = if let Some(ref corr) = p.correlation_external_id {
        if !corr.is_empty() {
            // Pass 1: match by $.external_id (RFC 822 Message-ID, standard In-Reply-To path).
            let mut pass1 = None;
            for candidate in message_id_match_candidates(corr) {
                let corr_filter = NoteFilter {
                    kind: Some("message".to_string()),
                    property_filters: vec![
                        PropertyFilter {
                            json_path: "$.external_id".to_string(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text(candidate),
                        },
                        PropertyFilter {
                            json_path: "$.direction".to_string(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text("outbound".to_string()),
                        },
                    ],
                    ..Default::default()
                };
                let corr_page = store
                    .query_notes_filtered(
                        ns,
                        &corr_filter,
                        PageRequest {
                            limit: 1,
                            offset: 0,
                        },
                    )
                    .await?;
                pass1 = corr_page.items.first().map(|n| {
                    // Falls back to the matched note's own UUID as root (#479b, ADR-040)
                    // when it carries no valid thread_id (e.g. legacy/imported row).
                    let thread_id = n
                        .properties
                        .as_ref()
                        .and_then(|props| props.get("thread_id"))
                        .and_then(Value::as_str)
                        .filter(|s| s.parse::<Uuid>().is_ok())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| n.id.as_hyphenated().to_string());
                    let from_actor = n
                        .properties
                        .as_ref()
                        .and_then(|props| props.get("from_actor"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (thread_id, from_actor)
                });
                if pass1.is_some() {
                    break;
                }
            }

            if pass1.is_some() {
                pass1
            } else if corr.parse::<Uuid>().is_ok() {
                // Pass 2: `corr` is a UUID — may be a thread UUID from X-Khive-Thread-ID.
                // Match against $.thread_id on an outbound note to recover from_actor.
                let thread_filter = NoteFilter {
                    kind: Some("message".to_string()),
                    property_filters: vec![
                        PropertyFilter {
                            json_path: "$.thread_id".to_string(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text(corr.clone()),
                        },
                        PropertyFilter {
                            json_path: "$.direction".to_string(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text("outbound".to_string()),
                        },
                    ],
                    ..Default::default()
                };
                let thread_page = store
                    .query_notes_filtered(
                        ns,
                        &thread_filter,
                        PageRequest {
                            limit: 1,
                            offset: 0,
                        },
                    )
                    .await?;
                thread_page.items.first().and_then(|n| {
                    let props = n.properties.as_ref()?;
                    let from_actor = props
                        .get("from_actor")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some((corr.clone(), from_actor))
                })
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Determine thread_id: caller-supplied > resolved from correlation > new root.
    // `p.thread_id` was already validated above (present+non-empty implies valid UUID).
    let thread_id: String = p
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| resolved.as_ref().map(|(tid, _)| tid.clone()))
        .unwrap_or_else(|| Uuid::new_v4().as_hyphenated().to_string());

    // Determine to_actor with 3-tier priority:
    // 1. from_actor of the correlated original (route reply back to the sending actor)
    // 2. caller-supplied default_inbound_actor (fresh email landing actor)
    // 3. p.to.trim() (back-compat: raw recipient address)
    let to_actor = resolved
        .as_ref()
        .map(|(_, fa)| fa.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            p.default_inbound_actor
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| p.to.trim().to_string());

    let sent_at = p.sent_at.as_deref().unwrap_or("").to_string();
    let sent_at_value = if sent_at.is_empty() {
        json!(Utc::now().to_rfc3339())
    } else {
        json!(sent_at)
    };

    let mut props = json!({
        "from": p.from.trim(),
        "to": p.to.trim(),
        "from_actor": p.from.trim(),
        "to_actor": to_actor,
        "direction": "inbound",
        "read": false,
        "thread_id": thread_id,
        "sent_at": sent_at_value,
    });
    if let Some(ref s) = p.subject {
        props["subject"] = json!(s);
    }
    if let Some(ref ext) = p.external_id {
        props["external_id"] = json!(ext);
    }
    if let Some(ref wmid) = p.wire_message_id {
        if !wmid.trim().is_empty() {
            props["wire_message_id"] = json!(wmid.trim());
        }
    }
    if let Some(ref wrefs) = p.wire_references {
        if !wrefs.trim().is_empty() {
            props["wire_references"] = json!(wrefs.trim());
        }
    }
    if let Some(ref kind) = p.channel_kind {
        props["channel_kind"] = json!(kind);
    }
    // Metadata passthrough (#448): merged additively so it never clobbers the
    // identity/routing fields set above — a key already present always wins.
    if let Some(metadata) = p.metadata {
        if let Some(obj) = props.as_object_mut() {
            for (k, v) in metadata {
                obj.entry(k).or_insert(v);
            }
        }
    }

    let note = match runtime
        .try_create_note(
            token,
            "message",
            p.subject.as_deref(),
            p.content.trim(),
            Some(props),
        )
        .await?
    {
        Some(n) => n,
        None => {
            tracing::debug!(
                external_id = ?p.external_id,
                "comm.ingest: duplicate message skipped"
            );
            return Ok(json!({
                "ok": true,
                "deduplicated": true,
                "external_id": p.external_id,
            }));
        }
    };

    Ok(json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "thread_id": thread_id,
        "external_id": p.external_id,
        "deduplicated": false,
    }))
}

/// Deterministic UUID identifying the `channel_health` row for one
/// `(namespace, channel_kind, channel_slug)` triple (khive #606). Hashes the
/// triple as a JSON array (not a `:`-joined string, which is not injective
/// when a component itself contains `:`). See
/// crates/khive-pack-comm/docs/api/channel-health.md#handlersrsheartbeat_note_id
fn heartbeat_note_id(namespace: &str, channel_kind: &str, channel_slug: &str) -> Uuid {
    let key = serde_json::to_vec(&(
        "khive:channel_health",
        namespace,
        channel_kind,
        channel_slug,
    ))
    .expect("a 4-tuple of &str always serializes to JSON");
    Uuid::new_v5(&Uuid::NAMESPACE_URL, &key)
}

/// `heartbeat` — persist one poll attempt's outcome into the channel's
/// heartbeat row (khive #606). Subhandler — only the daemon's channel poll
/// loop calls this. Read-modify-write: `created_at` is preserved across
/// updates, `last_error` is RETAINED across a subsequent success (design
/// review amendment 3), and `consecutive_failures` resets on success /
/// increments on failure, read from the prior row (correct across restarts).
pub(crate) async fn handle_heartbeat(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    // HeartbeatParams omits deny_unknown_fields — mirrors IngestParams (dispatch
    // consumes `namespace` before the handler runs).
    let p: HeartbeatParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("heartbeat: bad params: {e}")))?;

    if p.channel_kind.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `channel_kind` must not be empty".into(),
        ));
    }
    if p.channel_slug.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `channel_slug` must not be empty".into(),
        ));
    }
    let outcome = match p.outcome.as_str() {
        s @ ("success" | "failure") => s,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "heartbeat: invalid `outcome` {other:?}; expected \"success\" or \"failure\""
            )));
        }
    };
    if outcome == "failure"
        && p.error_class
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `error_class` is required when outcome is \"failure\"".into(),
        ));
    }

    // Issue #917: heartbeat rows persist under `token.namespace()` — the
    // dispatch-authorized namespace every other comm verb already uses —
    // rather than the fixed `crate::CHANNEL_HEALTH_NAMESPACE` constant #606
    // pinned this to. `comm.heartbeat` is `Visibility::Subhandler` (never
    // reachable from the MCP wire); the only callers able to dispatch it are
    // trusted internal Rust code holding a `&VerbRegistry` handle, so the
    // gate check `VerbRegistry::dispatch_with_identity` already runs for
    // every dispatch (subhandlers included) is the sole authorization
    // boundary here (ADR-018) — this handler must not layer a second,
    // handler-local namespace check on top of it.
    //
    // The local single-tenant poll loop (`khive-mcp`'s
    // `record_channel_heartbeat`) is unaffected: it always passes
    // `"namespace": crate::CHANNEL_HEALTH_NAMESPACE` explicitly in its own
    // dispatch params, so it keeps writing under `"local"` exactly as
    // before. An authorized per-tenant writer (#917) instead dispatches via
    // `VerbRegistry::dispatch_as` with a `VerifiedActor` (an out-of-band
    // authenticated tenant principal, never derived from a wire-supplied
    // field — this verb has no wire path at all) and passes that tenant's
    // own namespace as this same explicit `namespace` dispatch param. Those
    // heartbeat rows land under that tenant's namespace, so a tenant-scoped
    // `comm.health` (#877) now observes real writer state
    // instead of an empty set by construction.
    let ns = token.namespace().as_str();
    let store = runtime.notes(token)?;
    let id = heartbeat_note_id(ns, &p.channel_kind, &p.channel_slug);

    let existing = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("heartbeat: get_note: {e}")))?;

    let now = Utc::now();
    let at = p.at.clone().unwrap_or_else(|| now.to_rfc3339());

    let mut props = existing
        .as_ref()
        .and_then(|n| n.properties.clone())
        .unwrap_or_else(|| json!({}));

    props["channel_kind"] = json!(p.channel_kind);
    props["channel_slug"] = json!(p.channel_slug);
    props["last_poll_attempt_at"] = json!(at);

    match outcome {
        "success" => {
            props["last_success_at"] = json!(at);
            props["consecutive_failures"] = json!(0);
            // last_error is intentionally left untouched — design review amendment 3.
        }
        "failure" => {
            props["last_failure_at"] = json!(at);
            let prev_failures = props
                .get("consecutive_failures")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            props["consecutive_failures"] = json!(prev_failures + 1);
            props["last_error"] = json!({
                "class": p.error_class.clone().unwrap_or_default(),
                "message": p.error_message.clone().unwrap_or_default(),
                "at": at,
            });
        }
        _ => unreachable!("outcome already validated above"),
    }

    khive_runtime::secret_gate::check_json(&props)?;

    let content = format!("channel heartbeat: {}:{}", p.channel_kind, p.channel_slug);
    khive_runtime::secret_gate::check(&content)?;

    let created_at = existing
        .as_ref()
        .map(|n| n.created_at)
        .unwrap_or_else(|| now.timestamp_micros());

    let note = Note {
        id,
        namespace: ns.to_string(),
        kind: "channel_health".to_string(),
        status: "active".to_string(),
        name: Some(format!("{}:{}", p.channel_kind, p.channel_slug)),
        content,
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(props),
        created_at,
        updated_at: now.timestamp_micros(),
        deleted_at: None,
    };

    store
        .upsert_note(note)
        .await
        .map_err(|e| RuntimeError::Internal(format!("heartbeat: upsert_note: {e}")))?;

    Ok(json!({
        "ok": true,
        "channel_kind": p.channel_kind,
        "channel_slug": p.channel_slug,
        "outcome": outcome,
    }))
}

/// Project a persisted `channel_health` note into the `comm.health()` channel
/// entry shape. Missing fields (a row written before a given property existed)
/// default to `null`/`0` rather than panicking — forward-compatible with rows
/// written by an older heartbeat writer.
fn channel_health_to_json(note: &Note) -> Value {
    let props = note.properties.clone().unwrap_or_else(|| json!({}));
    json!({
        "channel_kind": props.get("channel_kind").cloned().unwrap_or(Value::Null),
        "channel_slug": props.get("channel_slug").cloned().unwrap_or(Value::Null),
        "last_success_at": props.get("last_success_at").cloned().unwrap_or(Value::Null),
        "last_poll_attempt_at": props.get("last_poll_attempt_at").cloned().unwrap_or(Value::Null),
        "last_failure_at": props.get("last_failure_at").cloned().unwrap_or(Value::Null),
        "last_error": props.get("last_error").cloned().unwrap_or(Value::Null),
        "consecutive_failures": props.get("consecutive_failures").cloned().unwrap_or(json!(0)),
    })
}

/// `health` — read-only per-channel health snapshot (khive #606). Reads
/// `channel_health` rows from `token.namespace()` (khive #877 namespace
/// scoping); never returns a computed `healthy: bool` — that judgment belongs
/// to the caller. See crates/khive-pack-comm/docs/api/channel-health.md#handlersrshandle_health
/// for the `role`/`namespace`/`resource` field semantics (ADR-103 Stage 1).
///
/// `resource` is a process-level self-report of this process's own CPU/RSS
/// (via `getrusage`) plus in-flight background phase names. `cpu_us`/
/// `rss_bytes` are `null` only if `getrusage` is unavailable; `active_phases`
/// is always present and empty when nothing is in flight — raw observations
/// only, same "no computed healthy bool" rule as the rest of this verb.
pub(crate) async fn handle_health(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let has_args = match params.as_object() {
        Some(obj) => !obj.is_empty(),
        None => !params.is_null(),
    };
    if has_args {
        return Err(RuntimeError::InvalidInput(
            "health: takes no arguments".into(),
        ));
    }

    let store = runtime.notes(token)?;
    const MAX_CHANNELS: u32 = 200;
    let filter = NoteFilter {
        kind: Some("channel_health".to_string()),
        ..Default::default()
    };
    let page = store
        .query_notes_filtered(
            token.namespace().as_str(),
            &filter,
            PageRequest {
                limit: MAX_CHANNELS,
                offset: 0,
            },
        )
        .await?;

    if page.items.len() == MAX_CHANNELS as usize {
        tracing::debug!(
            max_channels = MAX_CHANNELS,
            "comm.health: channel_health row count hit the page limit; \
             results may be silently truncated"
        );
    }

    let channels: Vec<Value> = page.items.iter().map(channel_health_to_json).collect();
    let as_of = Utc::now().to_rfc3339();

    let (role, source) = if channels.is_empty() {
        ("client", None::<&str>)
    } else {
        ("daemon", Some("daemon-heartbeat"))
    };

    let usage = khive_runtime::process_resource_usage();
    let resource = json!({
        "cpu_us": usage.map(|u| u.cpu_us),
        "rss_bytes": usage.map(|u| u.rss_bytes),
        "active_phases": khive_runtime::active_phase_names(),
    });

    Ok(json!({
        "role": role,
        "source": source,
        "as_of": as_of,
        "namespace": token.namespace().as_str(),
        "channels": channels,
        "resource": resource,
    }))
}

/// `comm.probe` response — a stable, minimal polling contract (khive daemon
/// hardening slice, ADR-D5). Field shape is frozen: do not add fields without
/// updating the frozen contract in the comm pack README.
#[derive(serde::Serialize)]
pub(crate) struct ProbeResponse {
    pub cursor_us: i64,
    pub new_messages: Vec<ProbeMessage>,
    pub stale_unread_count: i64,
}

#[derive(serde::Serialize)]
pub(crate) struct ProbeMessage {
    /// Full note UUID, hyphenated. `comm.read` accepts it directly.
    pub id: String,
    pub created_at_us: i64,
    pub from_actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// The single indexed read powering `comm.probe` (ADR-D5). `INDEXED BY
/// idx_comm_message_to_actor` is a regression fence against silent table scans.
/// `cursor_us`/`since_us` are keyed on `notes_seq.seq`, NOT `created_at` or
/// SQLite `rowid` — both can regress/collide across concurrent writers, VACUUM,
/// or hard-delete. Do not revert to either. See
/// crates/khive-pack-comm/docs/api/probe-cursor.md#handlersrsprobe_sql for the full
/// #780/#827 incident history.
const PROBE_SQL: &str = "WITH \
stats AS ( \
    SELECT \
        COALESCE(MAX(notes_seq.seq), 0) AS cursor_us, \
        COALESCE(SUM( \
            CASE \
                WHEN (json_type(notes.properties, '$.read') IS NULL \
                      OR json_type(notes.properties, '$.read') != 'true') \
                     AND notes.created_at < ?4 \
                THEN 1 ELSE 0 \
            END \
        ), 0) AS stale_unread_count \
    FROM notes INDEXED BY idx_comm_message_to_actor \
    JOIN notes_seq ON notes_seq.note_id = notes.id \
    WHERE notes.namespace = ?1 \
      AND notes.kind = 'message' \
      AND notes.deleted_at IS NULL \
      AND json_extract(notes.properties, '$.to_actor') = ?2 \
      AND json_extract(notes.properties, '$.direction') = 'inbound' \
), \
new_rows AS ( \
    SELECT \
        notes.id, \
        notes.created_at AS created_at_us, \
        COALESCE(json_extract(notes.properties, '$.from_actor'), notes.namespace) AS from_actor, \
        json_extract(notes.properties, '$.subject') AS subject \
    FROM notes INDEXED BY idx_comm_message_to_actor \
    JOIN notes_seq ON notes_seq.note_id = notes.id \
    WHERE notes.namespace = ?1 \
      AND notes.kind = 'message' \
      AND notes.deleted_at IS NULL \
      AND json_extract(notes.properties, '$.to_actor') = ?2 \
      AND json_extract(notes.properties, '$.direction') = 'inbound' \
      AND (?3 IS NULL OR notes_seq.seq > ?3) \
    ORDER BY notes.created_at DESC \
    LIMIT 100 \
) \
SELECT \
    stats.cursor_us, \
    stats.stale_unread_count, \
    new_rows.id, \
    new_rows.created_at_us, \
    new_rows.from_actor, \
    new_rows.subject \
FROM stats \
LEFT JOIN ( \
    SELECT * FROM new_rows ORDER BY created_at_us ASC \
) AS new_rows ON TRUE \
ORDER BY new_rows.created_at_us ASC";

/// `probe` — strictly read-only poll for new inbound message metadata and a
/// stale-unread count (ADR-D5). No read-flag mutation, no writes: this is
/// polled every ~30s by many monitors and must stay a single cheap indexed
/// query.
pub(crate) async fn handle_probe(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ProbeParams = deser(params)?;
    validate_actor_label("probe", &p.actor, "actor")?;
    if p.stale_minutes <= 0 {
        return Err(RuntimeError::InvalidInput(
            "probe: `stale_minutes` must be positive".into(),
        ));
    }

    let now_us = Utc::now().timestamp_micros();
    let stale_cutoff_us = now_us - p.stale_minutes * 60_000_000;

    let response = query_probe(
        runtime,
        token.namespace().as_str(),
        &p.actor,
        p.since_us,
        stale_cutoff_us,
    )
    .await?;

    serde_json::to_value(response).map_err(|e| {
        RuntimeError::InvalidInput(format!("probe: failed to serialize response: {e}"))
    })
}

/// A caller-supplied `since_us` above `notes_seq`'s durable high-water mark
/// cannot be a genuine cursor — it must be a pre-upgrade persisted-timestamp
/// cursor (#827). See crates/khive-pack-comm/docs/api/probe-cursor.md#handlersrsnotes_seq_high_water_mark
async fn notes_seq_high_water_mark(
    reader: &mut Box<dyn khive_storage::sql::SqlReader>,
) -> Result<i64, RuntimeError> {
    let row = reader
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT seq FROM sqlite_sequence WHERE name = 'notes_seq'".into(),
            params: vec![],
            label: Some("comm_probe_notes_seq_hwm".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    match row.and_then(|r| r.get("seq").cloned()) {
        Some(SqlValue::Integer(v)) => Ok(v),
        _ => Ok(0),
    }
}

async fn query_probe(
    runtime: &KhiveRuntime,
    namespace: &str,
    actor: &str,
    since_us: Option<i64>,
    stale_cutoff_us: i64,
) -> Result<ProbeResponse, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

    let high_water_mark = notes_seq_high_water_mark(&mut reader).await?;

    let effective_since = match since_us {
        Some(v) if v > high_water_mark => {
            tracing::warn!(
                actor,
                since_us = v,
                high_water_mark,
                "comm.probe: since_us exceeds the notes_seq high-water mark; treating it as a \
                 stale pre-upgrade timestamp cursor and resetting to baseline"
            );
            None
        }
        other => other,
    };

    let since_param = match effective_since {
        Some(v) => SqlValue::Integer(v),
        None => SqlValue::Null,
    };

    let statement = khive_storage::types::SqlStatement {
        sql: PROBE_SQL.to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(actor.to_string()),
            since_param,
            SqlValue::Integer(stale_cutoff_us),
        ],
        label: Some("comm_probe".into()),
    };

    let rows = reader
        .query_all(statement)
        .await
        .map_err(RuntimeError::Storage)?;

    let mut cursor_us = 0i64;
    let mut stale_unread_count = 0i64;
    let mut new_messages = Vec::new();

    for row in &rows {
        if let Some(SqlValue::Integer(v)) = row.get("cursor_us") {
            cursor_us = *v;
        }
        if let Some(SqlValue::Integer(v)) = row.get("stale_unread_count") {
            stale_unread_count = *v;
        }

        let id = match row.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => continue,
        };
        let created_at_us = match row.get("created_at_us") {
            Some(SqlValue::Integer(v)) => *v,
            _ => continue,
        };
        let from_actor = match row.get("from_actor") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => continue,
        };
        let subject = match row.get("subject") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        };

        new_messages.push(ProbeMessage {
            id,
            created_at_us,
            from_actor,
            subject,
        });
    }

    // #827: never let the returned cursor regress below what the caller already
    // holds (a hard-deleted high-seq row can lower MAX(seq) below a prior cursor).
    if let Some(floor) = effective_since {
        if cursor_us < floor {
            cursor_us = floor;
        }
    }

    Ok(ProbeResponse {
        cursor_us,
        new_messages,
        stale_unread_count,
    })
}

/// `cursor_get` — read the persisted channel poll checkpoint for
/// `(channel_kind, channel_slug)` (issue #449). Subhandler. Returns JSON
/// `null` when no row exists yet. Runs the pack-owned schema statement first
/// (lazy pack-schema bootstrap for in-memory/test runtimes).
pub(crate) async fn handle_cursor_get(
    runtime: &KhiveRuntime,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: CursorGetParams = deser(params)?;
    if p.channel_kind.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cursor_get: `channel_kind` must not be empty".into(),
        ));
    }
    if p.channel_slug.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cursor_get: `channel_slug` must not be empty".into(),
        ));
    }

    let sql = runtime.sql();
    let mut w = sql.writer().await.map_err(RuntimeError::Storage)?;
    w.execute_script(crate::vocab::COMM_CHANNEL_CURSOR_SCHEMA_STMT.to_string())
        .await
        .map_err(RuntimeError::Storage)?;

    let row = w
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT source, generation, high_water, updated_at FROM comm_channel_cursor \
                  WHERE channel_kind = ?1 AND channel_slug = ?2"
                .into(),
            params: vec![
                SqlValue::Text(p.channel_kind.clone()),
                SqlValue::Text(p.channel_slug.clone()),
            ],
            label: Some("comm_cursor_get".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    let Some(row) = row else {
        return Ok(Value::Null);
    };

    let source = match row.get("source") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => {
            return Err(RuntimeError::Internal(
                "cursor_get: malformed `source` column".into(),
            ));
        }
    };
    let generation = match row.get("generation") {
        Some(SqlValue::Integer(i)) if *i > 0 => *i as u64,
        _ => {
            return Err(RuntimeError::Internal(
                "cursor_get: malformed `generation` column".into(),
            ));
        }
    };
    let high_water = match row.get("high_water") {
        Some(SqlValue::Integer(i)) if *i > 0 => Some(*i as u64),
        None | Some(SqlValue::Null) => None,
        _ => {
            return Err(RuntimeError::Internal(
                "cursor_get: malformed `high_water` column".into(),
            ));
        }
    };
    let updated_at_us = match row.get("updated_at") {
        Some(SqlValue::Integer(i)) => *i,
        _ => {
            return Err(RuntimeError::Internal(
                "cursor_get: malformed `updated_at` column".into(),
            ));
        }
    };
    let committed_at = DateTime::<Utc>::from_timestamp_micros(updated_at_us).ok_or_else(|| {
        RuntimeError::Internal("cursor_get: invalid `updated_at` timestamp".into())
    })?;

    Ok(json!({
        "source": source,
        "generation": generation,
        "high_water": high_water,
        "committed_at": committed_at.to_rfc3339(),
    }))
}

/// `cursor_commit` — persist a channel poll checkpoint for `(channel_kind,
/// channel_slug)` (issue #449), replacing any prior row for that identity.
/// Subhandler — only the daemon's channel poll loop calls this, and only
/// after every envelope in the page has returned `Ok` from `comm.ingest`.
pub(crate) async fn handle_cursor_commit(
    runtime: &KhiveRuntime,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: CursorCommitParams = deser(params)?;
    if p.channel_kind.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cursor_commit: `channel_kind` must not be empty".into(),
        ));
    }
    if p.channel_slug.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cursor_commit: `channel_slug` must not be empty".into(),
        ));
    }
    if p.source.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cursor_commit: `source` must not be empty".into(),
        ));
    }
    if p.generation == 0 || p.generation > i64::MAX as u64 {
        return Err(RuntimeError::InvalidInput(
            "cursor_commit: `generation` must be in 1..=i64::MAX".into(),
        ));
    }
    if let Some(h) = p.high_water {
        if h == 0 || h > i64::MAX as u64 {
            return Err(RuntimeError::InvalidInput(
                "cursor_commit: `high_water` must be in 1..=i64::MAX when present".into(),
            ));
        }
    }

    let now_us = Utc::now().timestamp_micros();

    let sql = runtime.sql();
    let mut w = sql.writer().await.map_err(RuntimeError::Storage)?;
    w.execute_script(crate::vocab::COMM_CHANNEL_CURSOR_SCHEMA_STMT.to_string())
        .await
        .map_err(RuntimeError::Storage)?;

    w.execute(khive_storage::types::SqlStatement {
        sql: "INSERT INTO comm_channel_cursor(channel_kind, channel_slug, source, generation, high_water, updated_at) \
              VALUES(?1, ?2, ?3, ?4, ?5, ?6) \
              ON CONFLICT(channel_kind, channel_slug) DO UPDATE SET \
                source=excluded.source, \
                generation=excluded.generation, \
                high_water=excluded.high_water, \
                updated_at=excluded.updated_at"
            .into(),
        params: vec![
            SqlValue::Text(p.channel_kind.clone()),
            SqlValue::Text(p.channel_slug.clone()),
            SqlValue::Text(p.source.clone()),
            SqlValue::Integer(p.generation as i64),
            match p.high_water {
                Some(h) => SqlValue::Integer(h as i64),
                None => SqlValue::Null,
            },
            SqlValue::Integer(now_us),
        ],
        label: Some("comm_cursor_commit".into()),
    })
    .await
    .map_err(RuntimeError::Storage)?;

    let committed_at = DateTime::<Utc>::from_timestamp_micros(now_us)
        .expect("Utc::now().timestamp_micros() always round-trips");

    Ok(json!({
        "source": p.source,
        "generation": p.generation,
        "high_water": p.high_water,
        "committed_at": committed_at.to_rfc3339(),
    }))
}

/// Candidate `$.external_id` values (as received, plus bracket-toggled) to
/// match an inbound correlation key against. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#message-id--references-header-helpers-403
fn message_id_match_candidates(corr: &str) -> Vec<String> {
    let bare = corr
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(corr);
    if bare == corr {
        vec![corr.to_string(), format!("<{corr}>")]
    } else {
        vec![corr.to_string(), bare.to_string()]
    }
}

/// Normalize a stored Message-ID into RFC 5322 wire form (angle-bracketed);
/// the single place that does so for `In-Reply-To`/`References` headers.
fn wrap_message_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        trimmed.to_string()
    } else {
        format!("<{trimmed}>")
    }
}

/// Resolve the parent message's wire Message-ID (issue #403), direction-aware:
/// outbound parents read `external_id`, inbound parents read `wire_message_id`
/// (never the reverse — `external_id` on an inbound note is the IMAP dedup
/// key, not a Message-ID). `None` when the parent has no wire Message-ID.
fn parent_wire_message_id(orig_props: &Value) -> Option<String> {
    let direction = orig_props.get("direction").and_then(Value::as_str);
    let raw = if direction == Some("outbound") {
        orig_props.get("external_id").and_then(Value::as_str)
    } else {
        orig_props.get("wire_message_id").and_then(Value::as_str)
    }?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(wrap_message_id(trimmed))
    }
}

/// Resolve the parent message's own `References` chain, direction-aware
/// (inbound: `wire_references`; outbound: `references_chain`). `None` when
/// the parent has no chain to extend (RFC 5322: caller then falls back to the
/// parent's Message-ID alone). See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#message-id--references-header-helpers-403
fn parent_references_chain(orig_props: &Value) -> Option<&str> {
    let direction = orig_props.get("direction").and_then(Value::as_str);
    let raw = if direction == Some("outbound") {
        orig_props.get("references_chain").and_then(Value::as_str)
    } else {
        orig_props.get("wire_references").and_then(Value::as_str)
    }?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Sanitize a single References/In-Reply-To token: reject anything containing
/// CR or LF (header injection guard) or without an `@` (not a plausible
/// message id), then normalize to wire form via [`wrap_message_id`].
///
/// Returns `None` for a malformed token so the caller can skip it rather than
/// emit a corrupt header.
fn sanitize_reference_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
        return None;
    }
    let bare = trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed);
    if bare.is_empty() || !bare.contains('@') || bare.contains(['<', '>']) {
        return None;
    }
    Some(wrap_message_id(trimmed))
}

/// Strip angle brackets and surrounding whitespace from a wire-form message id,
/// for use as a de-duplication comparison key only -- callers keep pushing each
/// token's original serialization into the emitted header, never this bare form.
fn bare_reference_id(token: &str) -> String {
    let trimmed = token.trim();
    trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed)
        .to_string()
}

/// Build the full `References` header value for a reply: the parent's
/// existing chain (sanitized, malformed tokens skipped) followed by the
/// parent's own Message-ID, de-duplicated by bracket-stripped form
/// (first-seen order). `parent_message_id` is expected already wire-wrapped.
fn build_references_header(parent_chain: Option<&str>, parent_message_id: &str) -> String {
    let chain_tokens = parent_chain
        .map(|chain| {
            chain
                .split_whitespace()
                .filter_map(sanitize_reference_token)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut tokens: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for token in chain_tokens
        .into_iter()
        .chain(std::iter::once(parent_message_id.to_string()))
    {
        if seen.insert(bare_reference_id(&token)) {
            tokens.push(token);
        }
    }
    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::{
        build_references_header, heartbeat_note_id, message_id_match_candidates,
        parent_references_chain, parent_wire_message_id, sanitize_reference_token, wrap_message_id,
    };
    use serde_json::json;

    // #606: a delimiter-joined
    // `format!("...:{a}:{b}:{c}")` id encoding is not injective once
    // components may themselves contain `:` — these two distinct triples
    // both produced `"khive:channel_health:a:b:c:d"` under the pre-fix
    // scheme (`namespace:kind:slug` == `"a:b"` + `"c"` + `"d"` joins to the
    // same string as `"a"` + `"b:c"` + `"d"`). The JSON-array encoding must
    // keep them distinct.
    #[test]
    fn heartbeat_note_id_does_not_collide_on_delimiter_bearing_components() {
        let a = heartbeat_note_id("a:b", "c", "d");
        let b = heartbeat_note_id("a", "b:c", "d");
        assert_ne!(
            a, b,
            "distinct (namespace, channel_kind, channel_slug) triples with \
             colons inside a component must never hash to the same id"
        );
    }

    #[test]
    fn heartbeat_note_id_is_deterministic() {
        assert_eq!(
            heartbeat_note_id("local", "email", "recipient@example.com"),
            heartbeat_note_id("local", "email", "recipient@example.com"),
        );
    }

    #[test]
    fn candidates_bare_input_adds_bracketed_form() {
        // A bracket-free correlation key (as delivered by mail_parser) must also
        // try the wire form so it matches an outbound `<id@domain>` external_id.
        assert_eq!(
            message_id_match_candidates("sent-msg@khive.ai"),
            vec![
                "sent-msg@khive.ai".to_string(),
                "<sent-msg@khive.ai>".to_string(),
            ],
        );
    }

    #[test]
    fn candidates_bracketed_input_adds_bare_form() {
        // Reverse direction: a bracketed correlation key must also try the bare
        // form so it matches a stored bracket-free external_id. Guards the `else`
        // branch, which no ingest test exercises directly.
        assert_eq!(
            message_id_match_candidates("<sent-msg@khive.ai>"),
            vec![
                "<sent-msg@khive.ai>".to_string(),
                "sent-msg@khive.ai".to_string(),
            ],
        );
    }

    #[test]
    fn wrap_message_id_adds_brackets_when_absent() {
        assert_eq!(wrap_message_id("id@example.com"), "<id@example.com>");
    }

    #[test]
    fn wrap_message_id_leaves_already_bracketed_form_unchanged() {
        assert_eq!(
            wrap_message_id("<id@example.com>"),
            "<id@example.com>",
            "must not double-wrap an already-bracketed id"
        );
    }

    #[test]
    fn wrap_message_id_trims_whitespace() {
        assert_eq!(wrap_message_id("  id@example.com  "), "<id@example.com>");
    }

    #[test]
    fn parent_wire_message_id_reads_wire_message_id_for_inbound_parent() {
        let props = json!({
            "direction": "inbound",
            "wire_message_id": "inbound-msg@example.com",
            "external_id": "imap:host:1:42",
        });
        assert_eq!(
            parent_wire_message_id(&props).as_deref(),
            Some("<inbound-msg@example.com>"),
            "inbound parent must use wire_message_id, never the IMAP-key external_id"
        );
    }

    #[test]
    fn parent_wire_message_id_reads_external_id_for_outbound_parent() {
        let props = json!({
            "direction": "outbound",
            "external_id": "<outbound-msg@khive.ai>",
        });
        assert_eq!(
            parent_wire_message_id(&props).as_deref(),
            Some("<outbound-msg@khive.ai>"),
            "outbound parent must reuse its self-minted external_id verbatim"
        );
    }

    #[test]
    fn parent_wire_message_id_none_when_outbound_parent_has_no_external_id() {
        let props = json!({ "direction": "outbound" });
        assert_eq!(parent_wire_message_id(&props), None);
    }

    #[test]
    fn parent_wire_message_id_none_when_inbound_parent_has_no_wire_message_id() {
        let props = json!({ "direction": "inbound" });
        assert_eq!(parent_wire_message_id(&props), None);
    }

    #[test]
    fn parent_wire_message_id_none_for_empty_properties() {
        assert_eq!(parent_wire_message_id(&json!({})), None);
    }

    #[test]
    fn parent_references_chain_reads_wire_references_for_inbound_parent() {
        let props = json!({
            "direction": "inbound",
            "wire_references": "<grandparent1@example.com> <parent123@example.com>",
            "references_chain": "should-not-be-read@example.com",
        });
        assert_eq!(
            parent_references_chain(&props),
            Some("<grandparent1@example.com> <parent123@example.com>"),
            "inbound parent must use wire_references, never the outbound-only references_chain"
        );
    }

    #[test]
    fn parent_references_chain_reads_references_chain_for_outbound_parent() {
        let props = json!({
            "direction": "outbound",
            "references_chain": "<grandparent1@example.com> <parent123@example.com>",
            "wire_references": "should-not-be-read@example.com",
        });
        assert_eq!(
            parent_references_chain(&props),
            Some("<grandparent1@example.com> <parent123@example.com>"),
            "outbound parent must use references_chain, never the inbound-only wire_references"
        );
    }

    #[test]
    fn parent_references_chain_none_when_outbound_parent_has_no_chain() {
        let props = json!({ "direction": "outbound" });
        assert_eq!(parent_references_chain(&props), None);
    }

    #[test]
    fn parent_references_chain_none_when_inbound_parent_has_no_chain() {
        let props = json!({ "direction": "inbound" });
        assert_eq!(parent_references_chain(&props), None);
    }

    #[test]
    fn parent_references_chain_none_for_empty_properties() {
        assert_eq!(parent_references_chain(&json!({})), None);
    }

    #[test]
    fn parent_references_chain_none_for_blank_chain() {
        let props = json!({ "direction": "inbound", "wire_references": "   " });
        assert_eq!(
            parent_references_chain(&props),
            None,
            "a whitespace-only stored chain must resolve to None, not an empty References token"
        );
    }

    #[test]
    fn sanitize_reference_token_wraps_bare_id() {
        assert_eq!(
            sanitize_reference_token("id@example.com"),
            Some("<id@example.com>".to_string())
        );
    }

    #[test]
    fn sanitize_reference_token_leaves_bracketed_id_unchanged() {
        assert_eq!(
            sanitize_reference_token("<id@example.com>"),
            Some("<id@example.com>".to_string())
        );
    }

    #[test]
    fn sanitize_reference_token_rejects_crlf() {
        assert_eq!(
            sanitize_reference_token("id@example.com\r\nBcc: evil"),
            None
        );
        assert_eq!(sanitize_reference_token("id@example.com\nBcc: evil"), None);
    }

    #[test]
    fn sanitize_reference_token_rejects_missing_at_sign() {
        assert_eq!(sanitize_reference_token("not-a-message-id"), None);
    }

    #[test]
    fn sanitize_reference_token_rejects_empty() {
        assert_eq!(sanitize_reference_token(""), None);
        assert_eq!(sanitize_reference_token("   "), None);
    }

    #[test]
    fn sanitize_reference_token_rejects_embedded_angle_brackets() {
        assert_eq!(
            sanitize_reference_token("a@example.com<b@example.com>"),
            None
        );
    }

    #[test]
    fn build_references_header_extends_existing_chain_of_two_or_more() {
        // Core spec: a reply whose parent has an existing References
        // chain of 2+ ids must produce chain + parent Message-ID, not just the
        // immediate parent.
        let chain = Some("<grandparent1@example.com> <grandparent2@example.com>");
        assert_eq!(
            build_references_header(chain, "<parent123@example.com>"),
            "<grandparent1@example.com> <grandparent2@example.com> <parent123@example.com>"
        );
    }

    #[test]
    fn build_references_header_falls_back_to_parent_message_id_when_no_chain() {
        assert_eq!(
            build_references_header(None, "<parent123@example.com>"),
            "<parent123@example.com>"
        );
    }

    #[test]
    fn build_references_header_skips_malformed_token_in_chain() {
        // A malformed token embedded in a stored chain (e.g. corrupted data, or a
        // CRLF injection attempt) must be skipped, not propagated into the header.
        let chain = Some("<good1@example.com> not-a-message-id <good2@example.com>");
        assert_eq!(
            build_references_header(chain, "<parent123@example.com>"),
            "<good1@example.com> <good2@example.com> <parent123@example.com>"
        );
    }

    #[test]
    fn build_references_header_bare_chain_tokens_get_wrapped() {
        // Chain tokens stored bracket-free (e.g. from an inbound parent's
        // wire_references, since mail_parser strips brackets) must be
        // normalized to wire form, matching wrap_message_id's contract.
        let chain = Some("bare1@example.com bare2@example.com");
        assert_eq!(
            build_references_header(chain, "<parent123@example.com>"),
            "<bare1@example.com> <bare2@example.com> <parent123@example.com>"
        );
    }

    #[test]
    fn build_references_header_dedups_when_chain_already_contains_parent_id() {
        // A stored chain that already contains an equivalent of the parent's own
        // id (e.g. tainted/legacy data) must not yield a literal duplicate: the
        // parent id keeps its original position in the chain (first-seen order)
        // and is not appended a second time at the end.
        let chain = Some("<root1@example.com> <parent123@example.com> <root2@example.com>");
        assert_eq!(
            build_references_header(chain, "<parent123@example.com>"),
            "<root1@example.com> <parent123@example.com> <root2@example.com>"
        );
    }

    #[test]
    fn build_references_header_dedups_bare_and_bracketed_forms_as_equivalent() {
        // The de-dup comparison must strip brackets before comparing, not just
        // compare byte-identical strings -- otherwise a bracket-free chain token
        // and a bracketed parent_message_id (or vice versa) would both survive
        // into the header as two "different" entries for the same id.
        let chain = Some("<parent123@example.com>");
        assert_eq!(
            build_references_header(chain, "parent123@example.com"),
            "<parent123@example.com>"
        );
    }
}
