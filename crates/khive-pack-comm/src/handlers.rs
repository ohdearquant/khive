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

/// `send` — create a message note in the caller's namespace (outbound) AND deliver
/// an inbound copy addressed to the actor label supplied in `to` (ADR-057).
///
/// Both copies land in the caller's namespace; no cross-namespace write occurs.
/// `from_actor` is set to `token.namespace().as_str()`. `to_actor` is set to the
/// `to` argument. When the caller's actor label is `"local"` (single-actor fallback),
/// `comm.inbox` does not apply an actor filter, preserving backward compatibility.
///
/// The routing `from` and `to` passed to `dual_write_message` are both set to the
/// caller's namespace string so that `from == recipient_ns_str` is always true: this
/// naturally bypasses the cross-namespace allowlist gate in `dual_write_message`
/// (ADR-057 §"Interaction with ADR-040"). The actor labels are propagated via the
/// `from_actor`/`to_actor` arguments and stored in message properties.
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

    // #200: addressed sends from an unattributed caller will stamp from_actor="local",
    // which causes reply-threading collapse when multiple unconfigured actors interact.
    // This is a known limitation pending issue #75 (actor identity per request).
    // We surface a visible warning so operators can diagnose mis-attribution; the send
    // proceeds rather than hard-erroring to preserve backward compatibility with
    // sessions that set default_namespace but not actor_id.
    //
    // Uses the shared actor-identity policy (#567) so this warning fires under
    // exactly the same "unattributed" definition the gate and token minter use.
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
///
/// When the caller's actor label is `"local"` (single-actor fallback), no `to_actor`
/// filter is applied and the inbox behaves as before (party-line). When the caller has
/// a non-`"local"` actor label, only messages addressed to that actor are returned.
/// Legacy messages without a `to_actor` field are visible regardless (Q3: OR IS NULL).
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

    // ADR-057 Q3: filter inbox by to_actor.
    //
    // When the caller has a configured actor label (non-"local"), apply an exact
    // to_actor filter so each actor sees only their own messages. Legacy messages
    // without a to_actor field (EqOrMissing) remain visible for the configured actor.
    //
    // When the caller is anonymous ("local") — the OSS single-tenant case — apply
    // EqOrMissing("local") so the caller sees only party-line messages (to_actor=
    // "local" or absent). This closes the #199 multi-actor read leak while preserving
    // backward-compatible behavior for deployments where everyone is 'local'.
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

    // #493: when a sender filter is supplied, apply it in Rust after the standard
    // direction/status/to_actor filters (which stay pushed into SQL for index usage) —
    // `FilterOp` has no prefix-match operator, so from_prefix cannot be pushed down.
    // Pages beyond the first are scanned (same unbounded-page-loop shape `handle_thread`
    // uses) until `limit` matches are collected or the store is exhausted.
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

    // Merge `read: true` into properties and patch in place via a real
    // `UPDATE` (not `upsert_note`'s `INSERT OR REPLACE`): the latter
    // silently reassigns the row's `rowid` on a primary-key conflict, which
    // would let a mark-as-read bump a message's `rowid` past the current
    // `comm.probe` cursor and resurrect it as "new" on the next poll (#780).
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

    // Issue #403: capture the parent's wire Message-ID so native mail clients
    // (not khive's own X-Khive-Thread-ID/external_id correlation) can group this
    // reply into the same conversation via In-Reply-To/References. `None` when
    // the parent has no wire Message-ID -- the reply then sends without those
    // headers, exactly as before this feature.
    let in_reply_to_message_id = parent_wire_message_id(&orig_props);

    // References must carry the FULL ancestor chain per RFC 5322, not just the
    // immediate parent: the parent's existing chain (if any) followed by the
    // parent's own Message-ID. `None` when the parent has no wire Message-ID at
    // all (mirrors `in_reply_to_message_id`); malformed tokens in the parent's
    // stored chain are individually skipped rather than corrupting the header.
    let references_chain = in_reply_to_message_id.as_deref().map(|parent_mid| {
        build_references_header(parent_references_chain(&orig_props), parent_mid)
    });

    // UE6-H2: thread_id must always be a full 36-char hyphenated UUID.
    // If the stored thread_id is a valid full UUID, use it; otherwise fall
    // back to the original message's own full UUID as the thread root.
    let thread_id = orig_props
        .get("thread_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .map(|u| u.as_hyphenated().to_string())
        .unwrap_or_else(|| original.id.as_hyphenated().to_string());

    // ADR-057: prefer from_actor/to_actor fields when present (actor-addressed messages).
    // Fall back to from/to namespace strings for legacy messages.
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

    // UE6-H1: route reply to the "other party" — not always to the original sender.
    // If the reply caller is the original sender (from_actor or from), route to the
    // original recipient. If the reply caller is the original recipient, route back
    // to the original sender.
    let reply_to = if from_actor_label == original_from {
        original_to.clone()
    } else {
        original_from.clone()
    };

    // ADR-057: always set from_actor/to_actor on replies (fail-closed on cross-namespace
    // write). Both copies land in the caller's namespace regardless of whether the
    // original message carried actor labels. The reply_to label is derived from the
    // original's actor fields when present, else from the legacy from/to strings treated
    // as labels. No legacy code path can cause dual_write_message to mint a token in a
    // foreign namespace.
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

        // Cross-namespace root resolution: if the note's properties.thread_id is a
        // valid full UUID that differs from the note's own UUID, use that as the
        // canonical thread_id.  This handles the case where the caller holds an
        // inbound copy UUID (id_B) but the canonical root is the outbound UUID (id_A).
        // Both copies were written with the same canonical thread_id by dual_write_message.
        //
        // Missing/invalid `thread_id` (issue #479b -- e.g. a legacy/imported root
        // written before the canonical field existed) falls back to the passed
        // note's own UUID, matching ADR-040: a target with no `thread_id` becomes
        // the root for its chain.
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

    // Practical equivalent of `id == root OR properties.thread_id == root`
    // (issue #479b): the SQL filter above only matches `properties.thread_id ==
    // canonical_thread_id`, which misses a root note that lacks a `thread_id`
    // property at all (e.g. legacy/imported data). Explicitly include the
    // already-validated root note when the query didn't already return it, so
    // `comm.thread(id=root)` never reports an empty/incomplete thread for a
    // root that exists but predates the canonical `thread_id` field.
    let root_already_present = rows.iter().any(|r| r.full_id == root_note.id);
    if !root_already_present {
        rows.push(ThreadRow {
            created_at: root_note.created_at,
            full_id: root_note.id,
            json: note_to_message_json(&root_note),
        });
    }

    // #494 / codex r1: `after` cursor — either a message id (short prefix or full
    // UUID, resolved the same way `id` is) or an RFC 3339 timestamp. An id cursor
    // resolves to the full `(created_at, full_id)` tuple of the referenced note so
    // ties on equal microsecond timestamps are broken deterministically instead of
    // being skipped or duplicated. A timestamp cursor is parsed to microseconds via
    // chrono (matching the pattern in khive-pack-brain/src/handlers.rs and
    // khive-vcs/src/sync.rs) rather than compared as a raw string, so non-canonical
    // but valid RFC 3339 forms (whole-second `Z`, `+00:00` offsets, ...) compare
    // correctly against khive's canonical microsecond timestamps. An `after` value
    // that is neither a resolvable id nor a parseable RFC 3339 timestamp is a hard
    // error — never silently coerced or treated as "no cursor".
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
            // Tuple compare, not timestamp-only: two rows sharing a microsecond
            // `created_at` (e.g. ADR-057 dual-write self-send copies) are ordered
            // deterministically by `full_id`, so an id cursor sitting on one of them
            // never skips or re-includes its tie.
            AfterCursor::Id {
                created_at,
                full_id,
            } => {
                let row_key = (r.created_at, r.full_id);
                let cursor_key = (*created_at, *full_id);
                match order {
                    // "after" in desc order means further along the desc sequence,
                    // i.e. strictly older (smaller key) than the cursor.
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

    // Total order: sort by `(created_at, full_id)` — the same tuple the cursor
    // filter above compares against — ascending for order="asc", reversed for
    // "desc". Sorting on timestamp alone (prior behavior) left ties among
    // same-microsecond rows in query-return order, which is not stable across
    // pages/backends.
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

/// A thread row carries the sort/cursor key (`created_at`, `full_id`) alongside
/// the already-rendered message JSON, so the total-order sort and cursor filter
/// in `handle_thread` compare exact `(i64, Uuid)` tuples instead of re-parsing
/// the ISO string embedded in the JSON.
struct ThreadRow {
    created_at: i64,
    full_id: Uuid,
    json: Value,
}

/// `after` cursor resolved to a comparable key. An id cursor carries the full
/// `(created_at, full_id)` tuple of the referenced message for tie-breaking; a
/// timestamp cursor carries only the parsed microsecond value (there is no
/// specific row to break ties against).
enum AfterCursor {
    Id { created_at: i64, full_id: Uuid },
    Timestamp { micros: i64 },
}

/// `ingest` — write a single inbound message note from a channel adapter.
///
/// This is a `Visibility::Subhandler` verb: it is not accessible via the MCP
/// wire and is only callable from within the process (e.g. the polling loop in
/// `khive-mcp`). It is the authoritative write path for all channel-delivered
/// messages; the polling loop must not bypass it.
///
/// Thread resolution: when `correlation_external_id` is supplied, the handler
/// queries for an existing message note whose `external_id` matches that value,
/// reads its `thread_id`, and attaches the new note to the same thread.
///
/// Deduplication: when `external_id` is supplied, `try_create_note` uses
/// a verify-after-insert check on the durable unique index on `external_id`.
/// A confirmed duplicate returns `Ok(None)` without error; only an
/// external_id collision is treated as dedup; other constraint violations
/// surface as errors.
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
    // Reject a malformed caller-supplied thread_id at the boundary (issue #479a):
    // a present, non-empty `thread_id` that is not a valid UUID must fail closed
    // rather than being silently dropped and replaced with a fresh UUID, which
    // would split the message into the wrong conversation. A blank/absent value
    // is not an error -- it just means "no caller-supplied thread_id".
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

    // Thread resolution: if correlation_external_id is present, find the message it refers to
    // and extract both its internal thread_id and the from_actor of the original sender so that
    // replies route back to the actor who sent the original, not to the raw email address.
    //
    // Two-query fallback: `corr` may be either a Message-ID (matched via $.external_id) from
    // a human webmail In-Reply-To header, OR a thread UUID (matched via $.thread_id) from
    // a preserved X-Khive-Thread-ID header on our own outbound emails.  We try external_id
    // first (preserves the In-Reply-To path); if that misses we fall back to thread_id.
    let resolved: Option<(String, String)> = if let Some(ref corr) = p.correlation_external_id {
        if !corr.is_empty() {
            // Pass 1: match by $.external_id (RFC 822 Message-ID, standard In-Reply-To path).
            // Our own outbound mail stores its Message-ID in wire form `<id@domain>`
            // (angle brackets included), while `mail_parser` strips the brackets from an
            // inbound `In-Reply-To`, yielding `id@domain`. Match the correlation key as
            // received and in its bracket-toggled form so `<id>` and `id` correlate either
            // way; the exact form is tried first. Restricted to outbound notes (mirrors
            // Pass 2) so an inbound note's own external_id can never be matched as a
            // threading parent.
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
                    // Fall back to the matched note's own UUID as the canonical
                    // root (issue #479b) when it carries no valid `thread_id` --
                    // e.g. a legacy/imported outbound row written before the
                    // canonical `thread_id` field existed. Per ADR-040, a
                    // target message with no `thread_id` becomes the root for
                    // the new chain, so replies stay attached to the right
                    // conversation/actor instead of splitting into a fresh
                    // thread routed to the default inbound actor.
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
    // Generic transport-layer metadata passthrough (issue #448 Finding 2): merged
    // additively so it can never clobber the identity/routing fields set above --
    // a key already present (from, to, from_actor, to_actor, direction, read,
    // thread_id, sent_at, subject, external_id, wire_message_id, wire_references,
    // channel_kind) always wins. The comm pack does not interpret any metadata
    // key; the email channel happens to use it for quarantine markers.
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
/// `(namespace, channel_kind, channel_slug)` triple (khive #606).
///
/// Deterministic (not `Uuid::new_v4`) so `handle_heartbeat` can compute the
/// same id on every poll tick and `upsert_note`'s `INSERT OR REPLACE` updates
/// the same row instead of accumulating a new one per tick. Keying by slug in
/// addition to kind is the point of #606's amendment 2: two accounts of the
/// same kind (e.g. two mailboxes, both `kind() == "email"`) must not collapse
/// into a single row.
///
/// The three components are hashed as a JSON array of strings, NOT joined
/// with a `:` delimiter (round-1 internal review, Medium finding). Namespaces
/// may themselves contain `:` (hierarchical namespace strings are explicitly
/// allowed), so a delimiter-joined `format!("...:{a}:{b}:{c}")` is not an
/// injective encoding: `(namespace="a:b", channel_kind="c", channel_slug="d")`
/// and `(namespace="a", channel_kind="b:c", channel_slug="d")` both produced
/// the identical string `"khive:channel_health:a:b:c:d"` under the old
/// scheme. `serde_json::to_vec` of an array of strings is unambiguous —
/// each element is quoted and internal quotes/backslashes are escaped — so
/// distinct triples always serialize to distinct byte sequences.
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
/// loop (`crates/khive-mcp/src/serve.rs::channel_poll_loop`) calls this.
///
/// Read-modify-write against the existing row (if any) so that:
/// - `created_at` is preserved across updates (first-seen time), not reset
///   every tick.
/// - `last_error` is RETAINED across a subsequent success (design review
///   amendment 3): callers compare `last_error.at` against
///   `last_success_at`/`last_failure_at` to tell a resolved issue from a live
///   one, so a success must never clear it.
/// - `consecutive_failures` resets to 0 on success and increments on failure,
///   read from the prior row rather than any in-process counter, so it is
///   correct even across a daemon restart.
pub(crate) async fn handle_heartbeat(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    // Note: HeartbeatParams does not use deny_unknown_fields — mirrors
    // IngestParams, since the poll loop passes `namespace` alongside these
    // fields for VerbRegistry::dispatch to consume before the handler runs.
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

    // #606 design review Blocker fix (review fix): heartbeat rows are an
    // OPERATIONAL surface, not message data. Persist to
    // `crate::CHANNEL_HEALTH_NAMESPACE` ALWAYS — never `token.namespace()` —
    // so a poll loop configured with a non-local `KHIVE_EMAIL_INGEST_NAMESPACE`
    // cannot cause heartbeat rows to land anywhere but where `handle_health`
    // reads from. This is enforced here (not just at the serve.rs call site)
    // so the guarantee holds even if a future caller passes a different
    // `namespace` dispatch param.
    let ns = crate::CHANNEL_HEALTH_NAMESPACE;
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

/// `health` — read-only per-channel health snapshot (khive #606).
///
/// Reads the daemon-persisted `channel_health` rows from
/// `crate::CHANNEL_HEALTH_NAMESPACE` UNCONDITIONALLY — never
/// `token.namespace()` (design review Blocker fix, example actor 2026-07-04). Heartbeat
/// rows are an operational surface, not message data, so a client-role
/// no-arg call must see them regardless of what namespace the caller's own
/// messages happen to be ingested under (e.g. `KHIVE_EMAIL_INGEST_NAMESPACE`
/// set to something other than `"local"`). Cross-process read is the point
/// of this verb (design review amendment 1): a client-role process (stdio MCP
/// without `--daemon`) has no in-memory poll-loop state of its own, so it
/// must read what the daemon already wrote. `role` answers "who owns the
/// loops", not "whose memory answered": any persisted row means some daemon
/// owns the channel loops, so `role` is reported as `"daemon"` with
/// `source: "daemon-heartbeat"` regardless of whether THIS process is that
/// daemon. `role: "client"` with an empty `channels` array is correct only
/// when no daemon heartbeat state exists at all (fresh install, or a daemon
/// that has never completed a poll tick) — the comm pack has no visibility
/// into which channels are configured (that lives in
/// `khive-mcp`/`khive-channel-email`), so an empty result is the only
/// fact-based response available at this layer.
///
/// Never returns a computed `healthy: bool` (design review amendment: "report
/// timestamps only") — staleness/alerting judgment belongs to the caller.
///
/// `resource` (ADR-103 Stage 1, issue #723 ask 2): a process-level self-report
/// of this process's own cumulative CPU time and RSS (via `getrusage`,
/// `khive_runtime::process_resource_usage`) plus the names of any background
/// phases (e.g. `ann_warm`) currently in flight in this process
/// (`khive_runtime::active_phase_names`). "This process" is, in the common
/// case, the daemon itself: a client-role stdio session without an in-memory
/// poll loop of its own still forwards `dispatch` calls to the daemon over
/// its socket, so this handler body executes inside the daemon process, not
/// the thin client. `cpu_us`/`rss_bytes` are `null` only if the underlying
/// `getrusage` read is unavailable on this platform; `active_phases` is
/// always present and empty when nothing is in flight. Raw observations
/// only, per the same "no computed healthy bool" rule as the rest of this
/// verb — attributing severity to a given CPU/RSS number is the caller's
/// judgment, not this verb's.
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
            crate::CHANNEL_HEALTH_NAMESPACE,
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
/// idx_comm_message_to_actor` is a regression fence: if a custom bootstrap
/// skips comm schema-plan application, this query fails loudly instead of
/// silently degrading to a table scan.
///
/// `cursor_us`/`since_us` are keyed on SQLite `rowid`, not `created_at`
/// (#780): `created_at` is an application-clock read taken before a note's
/// write acquires the writer critical section, so two concurrent writers can
/// commit out of stamp order, so a `created_at`-keyed cursor can then advance
/// past a row that committed *after* it, permanently hiding that row from
/// every later probe. `rowid` is assigned by SQLite exactly once, inside the
/// one active writer transaction, so it is monotonic with commit order by
/// construction. The wire field names keep the `_us` suffix (frozen contract,
/// ADR-D5) but the value is an opaque monotonic token, not a microsecond
/// timestamp; do not revert this to `created_at`. `created_at_us` on each
/// `new_messages` entry is unaffected: it stays a real display timestamp,
/// still ordered ascending by `created_at` for readability, and carries no
/// cursor guarantee of its own.
const PROBE_SQL: &str = "WITH \
stats AS ( \
    SELECT \
        COALESCE(MAX(rowid), 0) AS cursor_us, \
        COALESCE(SUM( \
            CASE \
                WHEN (json_type(properties, '$.read') IS NULL \
                      OR json_type(properties, '$.read') != 'true') \
                     AND created_at < ?4 \
                THEN 1 ELSE 0 \
            END \
        ), 0) AS stale_unread_count \
    FROM notes INDEXED BY idx_comm_message_to_actor \
    WHERE namespace = ?1 \
      AND kind = 'message' \
      AND deleted_at IS NULL \
      AND json_extract(properties, '$.to_actor') = ?2 \
      AND json_extract(properties, '$.direction') = 'inbound' \
), \
new_rows AS ( \
    SELECT \
        id, \
        created_at AS created_at_us, \
        COALESCE(json_extract(properties, '$.from_actor'), namespace) AS from_actor, \
        json_extract(properties, '$.subject') AS subject \
    FROM notes INDEXED BY idx_comm_message_to_actor \
    WHERE namespace = ?1 \
      AND kind = 'message' \
      AND deleted_at IS NULL \
      AND json_extract(properties, '$.to_actor') = ?2 \
      AND json_extract(properties, '$.direction') = 'inbound' \
      AND (?3 IS NULL OR rowid > ?3) \
    ORDER BY created_at DESC \
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

async fn query_probe(
    runtime: &KhiveRuntime,
    namespace: &str,
    actor: &str,
    since_us: Option<i64>,
    stale_cutoff_us: i64,
) -> Result<ProbeResponse, RuntimeError> {
    let since_param = match since_us {
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

    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
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

    Ok(ProbeResponse {
        cursor_us,
        new_messages,
        stale_unread_count,
    })
}

/// `cursor_get` — read the persisted channel poll checkpoint for
/// `(channel_kind, channel_slug)` (issue #449). Subhandler — only the
/// daemon's channel poll loop calls this. Returns JSON `null` when no row
/// exists yet (first-run compatibility mode).
///
/// Runs the pack-owned `comm_channel_cursor` schema statement before the
/// query so an in-memory/test runtime that never applied the boot-time
/// schema plan still works (matches the repository's lazy pack-schema
/// bootstrap convention).
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

/// Candidate `$.external_id` values to match an inbound correlation key against.
///
/// Outbound mail stores its Message-ID in wire form `<id@domain>` (angle brackets
/// included); `mail_parser` strips those brackets from an inbound `In-Reply-To`,
/// yielding `id@domain`. To correlate a reply back to the sending actor we must
/// match either representation, so this returns the key as received plus its
/// bracket-toggled variant, exact form first.
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

/// Normalize a stored Message-ID into RFC 5322 wire form (angle-bracketed).
///
/// Stored values may already be bracketed (an outbound note's self-minted
/// `external_id`, e.g. `<uuid@domain>`) or bracket-free (an inbound note's
/// `wire_message_id`, since `mail_parser` strips brackets when parsing). This is
/// the single place that normalizes to the wire form the `In-Reply-To` /
/// `References` headers require.
fn wrap_message_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        trimmed.to_string()
    } else {
        format!("<{trimmed}>")
    }
}

/// Resolve the parent message's wire Message-ID for an outbound reply's
/// `In-Reply-To`/`References` headers (issue #403).
///
/// Direction-aware: an outbound parent's own Message-ID is self-minted into
/// `external_id` at send time (e.g. `<uuid@domain>`). An inbound parent's
/// Message-ID lives in `wire_message_id` instead -- an inbound note's
/// `external_id` is the IMAP UIDVALIDITY/UID dedup key, never a Message-ID, and
/// must not be read here. Returns `None` when the parent carries no wire
/// Message-ID at all (e.g. a khive-internal parent, or an email parent the
/// channel never captured one for).
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
/// (issue #403 finding: References must carry the full ancestor chain, not
/// just the immediate parent).
///
/// An inbound parent's chain (as received over the wire) lives in
/// `wire_references`. An outbound parent's chain is whatever we persisted on
/// it as `references_chain` when *it* was sent (i.e. the chain that reply
/// itself extended) -- an outbound note that was a fresh send, not a reply,
/// carries no `references_chain`. Returns `None` when the parent has no chain
/// to extend; the caller then falls back to the parent's Message-ID alone,
/// matching RFC 5322 (References = prior chain, if any, + parent Message-ID).
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

/// Build the full `References` header value for a reply: the parent's existing
/// chain (each token individually sanitized; malformed tokens skipped per
/// issue #403 finding), followed by the parent's own Message-ID.
///
/// `parent_chain` tokens are whitespace-separated per RFC 5322. `parent_message_id`
/// is expected already wire-wrapped (as returned by [`parent_wire_message_id`]).
/// A stored chain can already contain an equivalent of the parent's own id (e.g.
/// tainted or legacy data); tokens are de-duplicated by their bracket-stripped
/// form, keeping first-seen order, so the parent id is skipped rather than
/// appended a second time when an equivalent token is already present.
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

    // #606 round-1 internal review, Medium finding: a delimiter-joined
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
        // Finding 1 core spec: a reply whose parent has an existing References
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
