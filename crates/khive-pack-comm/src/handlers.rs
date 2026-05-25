//! Verb handler implementations for the comm pack (ADR-040).
//!
//! All four verbs (`send`, `inbox`, `read`, `reply`) store and query `message`
//! notes in the standard notes table. Message-specific metadata lives in the
//! `properties` JSON column; `content` is the message body.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::Note;

fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

fn note_to_message_json(note: &Note) -> Value {
    json!({
        "id": short_id(note.id),
        "full_id": note.id,
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

/// `send` — create a message note in the caller's namespace (ADR-040 §send).
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

    let properties = json!({
        "from": from,
        "to": p.to,
        "direction": "outbound",
        "subject": p.subject,
        "thread_id": p.thread_id,
        "read": false,
        "sent_at": sent_at,
    });

    let note = runtime
        .create_note(
            token,
            "message",
            p.subject.as_deref(),
            &p.content,
            None,
            Some(properties),
            Vec::new(),
        )
        .await?;

    Ok(json!({
        "id": short_id(note.id),
        "full_id": note.id,
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
    let id = Uuid::parse_str(&p.id)
        .map_err(|_| RuntimeError::InvalidInput(format!("read: invalid UUID {:?}", p.id)))?;

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

    Ok(json!({ "id": short_id(id), "full_id": id, "read": true, "properties": props }))
}

/// `reply` — reply to a message, threading linkage (ADR-040 §reply).
pub(crate) async fn handle_reply(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ReplyParams = deser(params)?;
    let id = Uuid::parse_str(&p.id)
        .map_err(|_| RuntimeError::InvalidInput(format!("reply: invalid UUID {:?}", p.id)))?;
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
        "full_id": reply_note.id,
        "thread_id": thread_id,
        "from": from,
        "to": original_sender,
        "subject": reply_subject,
        "sent_at": sent_at,
    }))
}
