//! Verb handler implementations for the comm pack (ADR-040).
//!
//! All four verbs (`send`, `inbox`, `read`, `reply`) store and query `message`
//! notes in the standard notes table. Message-specific metadata lives in the
//! `properties` JSON column; `content` is the message body.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::note::Note;

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
        "created_at": note.created_at,
        "updated_at": note.updated_at,
    })
}

// ── param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct SendParams {
    pub to: String,
    pub content: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct InboxParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ReadParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(crate) struct ReplyParams {
    pub id: String,
    pub content: String,
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// `send` — create a message note in the caller's namespace (outbound) AND the
/// recipient's namespace (inbound) per ADR-040 §send.
///
/// Two writes are made atomically: if the inbound write fails the outbound note
/// is deleted before returning the error. When sender and recipient are the same
/// namespace only one note is written (no duplicate).
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

    let from = token.namespace().as_str().to_string();
    let sent_at = Utc::now().to_rfc3339();

    // Validate the recipient namespace before any write so we never write an
    // outbound note that we'd immediately have to roll back on parse failure.
    // When from == to, skip cross-namespace checks (self-send path).
    let recipient_token: Option<(Namespace, _)> = if from == p.to.trim() {
        None // self-send: only one note, no inbound write needed
    } else {
        let recipient_ns = Namespace::parse(p.to.trim()).map_err(|e| {
            RuntimeError::InvalidInput(format!("send: invalid recipient namespace {:?}: {e}", p.to))
        })?;
        let tok = runtime.authorize(recipient_ns.clone());
        Some((recipient_ns, tok))
    };

    // ── Write 1: outbound copy in caller's namespace ──────────────────────────
    let outbound_props = json!({
        "from": from,
        "to": p.to,
        "direction": "outbound",
        "subject": p.subject,
        "thread_id": p.thread_id,
        "read": false,
        "sent_at": sent_at,
    });

    let outbound_note = runtime
        .create_note(
            token,
            "message",
            p.subject.as_deref(),
            &p.content,
            None,
            Some(outbound_props),
            Vec::new(),
        )
        .await?;

    // ── Write 2: inbound copy in recipient's namespace ────────────────────────
    // Skipped when from == to (self-send) — only one note for that case.
    if let Some((_recipient_ns, ref recipient_tok)) = recipient_token {
        let inbound_props = json!({
            "from": from,
            "to": p.to,
            "direction": "inbound",
            "subject": p.subject,
            "thread_id": p.thread_id,
            "read": false,
            "sent_at": sent_at,
            // Cross-reference back to the outbound copy for read-receipt tracking.
            "outbound_ref": outbound_note.id,
        });

        let inbound_result = runtime
            .create_note(
                recipient_tok,
                "message",
                p.subject.as_deref(),
                &p.content,
                None,
                Some(inbound_props),
                Vec::new(),
            )
            .await;

        // Atomicity: roll back outbound on inbound failure.
        if let Err(inbound_err) = inbound_result {
            let _ = runtime.delete_note(token, outbound_note.id, true).await;
            return Err(inbound_err);
        }
    }

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
pub(crate) async fn handle_inbox(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: InboxParams = deser(params)?;
    let limit = p.limit.unwrap_or(20).clamp(1, 200);
    let status = p.status.as_deref().unwrap_or("unread");

    // Pull a broad window and filter in-memory for direction + read status.
    let notes = runtime
        .list_notes(token, Some("message"), limit * 4, 0)
        .await?;

    let messages: Vec<Value> = notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .filter(|n| {
            let props = n.properties.as_ref();
            let direction = props
                .and_then(|p| p.get("direction"))
                .and_then(Value::as_str);
            if direction != Some("inbound") {
                return false;
            }
            let read = props
                .and_then(|p| p.get("read"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match status {
                "unread" => !read,
                "read" => read,
                _ => true, // "all"
            }
        })
        .take(limit as usize)
        .map(note_to_message_json)
        .collect();

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

    // Thread root: use the original's thread_id if set, else the original's own UUID.
    let thread_id = orig_props
        .get("thread_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string());

    let original_sender = orig_props
        .get("from")
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

    let properties = json!({
        "from": from,
        "to": original_sender,
        "direction": "outbound",
        "subject": reply_subject,
        "thread_id": thread_id,
        "read": false,
        "sent_at": sent_at,
    });

    let reply_note = runtime
        .create_note(
            token,
            "message",
            if reply_subject.is_empty() {
                None
            } else {
                Some(reply_subject.as_str())
            },
            &p.content,
            None,
            Some(properties),
            Vec::new(),
        )
        .await?;

    Ok(json!({
        "id": short_id(reply_note.id),
        "full_id": reply_note.id.as_hyphenated().to_string(),
        "thread_id": thread_id,
        "from": from,
        "to": original_sender,
        "subject": reply_subject,
        "sent_at": sent_at,
    }))
}
