//! Verb handler implementations for the comm pack (ADR-040).
//!
//! All five verbs (`send`, `inbox`, `read`, `reply`, `thread`) store and query
//! `message` notes in the standard notes table. Message-specific metadata lives
//! in the `properties` JSON column; `content` is the message body.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
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

fn note_to_message_json(note: &Note) -> Value {
    json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "kind": "message",
        "content": note.content,
        "namespace": note.namespace,
        "properties": note.properties,
        "created_at": micros_to_iso(note.created_at),
        "updated_at": micros_to_iso(note.updated_at),
    })
}

/// Write an outbound copy (caller namespace) and an inbound copy (recipient namespace),
/// rolling back the outbound note if the inbound write fails (atomicity per ADR-040).
///
/// `subject`, `thread_id` are optional. `sent_at` is the RFC3339 timestamp for both copies.
///
/// Cross-namespace thread root invariant (ADR-040 §108-109): when a root message is sent
/// (i.e., `thread_id` is `None`), both the outbound and inbound copies must share the
/// same canonical `thread_id` — the sender's outbound UUID.  This ensures that
/// `comm.thread(id=outbound_id)` can find replies written in any namespace, because all
/// replies carry the same canonical thread_id regardless of which copy they were replying
/// to.
///
/// When `thread_id` is already supplied (reply path), it is forwarded unchanged to both
/// copies.
///
/// Returns the outbound `Note` on success.
// REASON: dual_write_message mirrors the ADR-040 §send wire shape exactly (from, to, subject,
// content, thread_id, sent_at) plus the two context args (runtime, token). Grouping them into
// a struct would not reduce overall complexity and would require an extra allocation on the
// hot path; the current flat signature is intentional.
#[allow(clippy::too_many_arguments)]
async fn dual_write_message(
    runtime: &KhiveRuntime,
    caller_token: &NamespaceToken,
    from: &str,
    to: &str,
    subject: Option<&str>,
    content: &str,
    thread_id: Option<&str>,
    sent_at: &str,
) -> Result<Note, RuntimeError> {
    // ADR-040 §cross-namespace-messaging: cross-namespace delivery is DENIED
    // until ADR-018 ACL policy is specified. This prevents unauthorized writes
    // into arbitrary recipient namespaces (issue #481).
    //
    // The recipient namespace must equal the caller namespace. Sending to a
    // different namespace would bypass the recipient's authorization gate, which
    // is unspecified until ADR-018 is implemented.
    let recipient_ns_str = to.trim();
    if from != recipient_ns_str {
        // Validate the recipient namespace string format before returning the
        // denial — so callers get InvalidInput for malformed strings rather than
        // a misleading CrossNamespaceWrite.
        if let Err(e) = Namespace::parse(recipient_ns_str) {
            return Err(RuntimeError::InvalidInput(format!(
                "send: invalid recipient namespace {to:?}: {e}"
            )));
        }
        return Err(RuntimeError::CrossNamespaceWrite {
            namespace: recipient_ns_str.to_string(),
        });
    }

    let outbound_props = json!({
        "from": from,
        "to": to,
        "direction": "outbound",
        "subject": subject,
        "thread_id": thread_id,
        "read": false,
        "sent_at": sent_at,
    });

    let outbound_note = runtime
        .create_note(
            caller_token,
            "message",
            subject,
            content,
            None,
            Some(outbound_props),
            Vec::new(),
        )
        .await?;

    // Canonical thread_id for both copies:
    // - If the caller supplied a thread_id (reply path), propagate it as-is.
    // - If this is a new root message (thread_id is None), use the outbound note's
    //   UUID so that both copies share the same canonical root across namespaces.
    let canonical_thread_id: String = match thread_id {
        Some(tid) => tid.to_string(),
        None => outbound_note.id.as_hyphenated().to_string(),
    };

    // Patch the outbound note's thread_id to the canonical value (only needed when
    // this is a root send; reply path already has the correct thread_id stored).
    if thread_id.is_none() {
        let store = runtime
            .notes(caller_token)
            .map_err(|e| RuntimeError::Internal(format!("dual_write: get outbound store: {e}")))?;
        let mut patched = outbound_note.clone();
        let mut props = patched.properties.clone().unwrap_or_else(|| json!({}));
        props["thread_id"] = json!(canonical_thread_id);
        patched.properties = Some(props);
        patched.updated_at = chrono::Utc::now().timestamp_micros();
        if let Err(patch_err) = store.upsert_note(patched).await {
            let _ = runtime
                .delete_note(caller_token, outbound_note.id, true)
                .await;
            return Err(RuntimeError::Internal(format!(
                "dual_write: patch outbound thread_id: {patch_err}"
            )));
        }
    }

    {
        // Inbound note lands in the caller's own namespace: cross-namespace send is
        // denied earlier in this function, so sender and recipient are always equal.
        let inbound_tok: &NamespaceToken = caller_token;

        let inbound_props = json!({
            "from": from,
            "to": to,
            "direction": "inbound",
            "subject": subject,
            "thread_id": canonical_thread_id,
            "read": false,
            "sent_at": sent_at,
            "outbound_ref": outbound_note.id,
        });

        let inbound_result = runtime
            .create_note(
                inbound_tok,
                "message",
                subject,
                content,
                None,
                Some(inbound_props),
                Vec::new(),
            )
            .await;

        if let Err(inbound_err) = inbound_result {
            let _ = runtime
                .delete_note(caller_token, outbound_note.id, true)
                .await;
            return Err(inbound_err);
        }
    }

    Ok(outbound_note)
}

// ── param structs ────────────────────────────────────────────────────────────

// ue-errors C1 (cross-pack): deny_unknown_fields so typo kwargs are rejected
// at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendParams {
    pub to: String,
    pub content: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadParams {
    pub id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyParams {
    pub id: String,
    pub content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadParams {
    /// Thread root ID: accepts either an 8-char short prefix or a full UUID.
    /// Returns all messages whose `properties.thread_id` matches this value,
    /// plus the originating message itself, in chronological order.
    pub id: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// `send` — create a message note in the caller's namespace (outbound) AND the
/// recipient's namespace (inbound) per ADR-040 §send.
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
    // Validate thread_id is a well-formed UUID when supplied (ADR-040: thread_id is a root UUID).
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

/// `inbox` — list inbound messages for the caller namespace (ADR-040 §inbox).
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
    let limit = p.limit.unwrap_or(20).clamp(1, 200) as usize;

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

/// `read` — mark a message as read (ADR-040 §read).
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

/// `reply` — reply to a message, threading linkage (ADR-040 §reply).
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

/// `thread` — retrieve all messages in a conversation thread (ADR-040 §thread).
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
