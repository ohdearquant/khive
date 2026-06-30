//! Core message primitives: ID helpers and the dual-write delivery function.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::note::Note;

pub(crate) fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

/// Resolve a raw id string to a full UUID.
///
/// Accepts a 36-char hyphenated UUID or an 8+ hex-char short prefix.
/// The prefix is resolved via `runtime.resolve_prefix` (namespace-scoped).
pub(crate) async fn resolve_id(
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

pub(crate) fn note_to_message_json(note: &Note) -> Value {
    let props = note.properties.as_ref();

    let from = props
        .and_then(|p| p.get("from_actor"))
        .and_then(Value::as_str)
        .map(|s| Value::String(s.to_string()))
        .unwrap_or_else(|| Value::String(note.namespace.clone()));

    let to = props
        .and_then(|p| p.get("to_actor"))
        .cloned()
        .unwrap_or(Value::Null);

    let subject = props
        .and_then(|p| p.get("subject"))
        .cloned()
        .unwrap_or(Value::Null);

    let read = props
        .and_then(|p| p.get("read"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let direction = props
        .and_then(|p| p.get("direction"))
        .cloned()
        .unwrap_or(Value::Null);

    let preview = build_preview(&note.content);

    json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "kind": "message",
        "from": from,
        "to": to,
        "subject": subject,
        "read": read,
        "direction": direction,
        "preview": preview,
        "content": note.content,
        "namespace": note.namespace,
        "properties": note.properties,
        "created_at": micros_to_iso(note.created_at),
        "updated_at": micros_to_iso(note.updated_at),
    })
}

fn build_preview(content: &str) -> String {
    const MAX_CHARS: usize = 80;
    let collapsed: String = content.split_whitespace().collect::<Vec<&str>>().join(" ");
    if collapsed.chars().count() > MAX_CHARS {
        let truncated: String = collapsed.chars().take(MAX_CHARS).collect();
        format!("{truncated}\u{2026}")
    } else {
        collapsed
    }
}

/// Write an outbound copy (caller namespace) and an inbound copy (recipient namespace),
/// rolling back the outbound note if the inbound write fails (atomicity guarantee).
///
/// `subject`, `thread_id` are optional. `sent_at` is the RFC3339 timestamp for both copies.
/// `from_actor` and `to_actor` are optional actor labels (ADR-057) stored in properties.
///
/// Cross-namespace thread root invariant: when a root message is sent (i.e., `thread_id`
/// is `None`), both the outbound and inbound copies must share the same canonical
/// `thread_id` — the sender's outbound UUID.  This ensures that
/// `comm.thread(id=outbound_id)` can find replies written in any namespace, because all
/// replies carry the same canonical thread_id regardless of which copy they were replying to.
///
/// When `thread_id` is already supplied (reply path), it is forwarded unchanged to both copies.
///
/// Returns the outbound `Note` on success.
// REASON: dual_write_message mirrors the send wire shape exactly (from, to, subject,
// content, thread_id, sent_at) plus the two context args (runtime, token). Grouping them into
// a struct would not reduce overall complexity and would require an extra allocation on the
// hot path; the current flat signature is intentional.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dual_write_message(
    runtime: &KhiveRuntime,
    caller_token: &NamespaceToken,
    from: &str,
    to: &str,
    subject: Option<&str>,
    content: &str,
    thread_id: Option<&str>,
    sent_at: &str,
    from_actor: Option<&str>,
    to_actor: Option<&str>,
) -> Result<Note, RuntimeError> {
    let recipient_ns_str = to.trim();
    if from != recipient_ns_str {
        // When actor labels are provided this is an actor-addressed local send;
        // both copies land in the caller's namespace so no cross-namespace check applies.
        // Only run the cross-namespace gate when no actor routing is in use.
        if from_actor.is_none() {
            // 1. Validate recipient namespace string format first.
            let recipient_ns = match Namespace::parse(recipient_ns_str) {
                Ok(ns) => ns,
                Err(e) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "send: invalid recipient namespace {to:?}: {e}"
                    )));
                }
            };

            // 2. Check sender-side outbound allowlist from config.
            //    Cross-namespace delivery is permitted only for declared recipients.
            let allowed = runtime
                .config()
                .allowed_outbound_namespaces
                .iter()
                .any(|ns| ns == &recipient_ns);

            if !allowed {
                return Err(RuntimeError::PermissionDenied {
                    verb: "comm.send".to_string(),
                    reason: format!(
                        "cross-namespace delivery to {recipient_ns_str:?} is not permitted; \
                         add {recipient_ns_str:?} to actor.allowed_outbound_namespaces in \
                         the sender's .khive/config.toml to enable delivery"
                    ),
                });
            }
            // 3. Allowlist hit: fall through to outbound note creation.
        }
    }

    let mut outbound_props = json!({
        "from": from,
        "to": to,
        "direction": "outbound",
        "subject": subject,
        "thread_id": thread_id,
        "read": false,
        "sent_at": sent_at,
    });
    if let Some(fa) = from_actor {
        outbound_props["from_actor"] = json!(fa);
    }
    if let Some(ta) = to_actor {
        outbound_props["to_actor"] = json!(ta);
    }

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
        // When actor labels are provided (ADR-057 actor-addressed path), both copies
        // land in the caller's namespace — no cross-namespace write occurs.
        // When sender and recipient are in different namespaces (allowed cross-ns path),
        // mint a recipient-scoped read+write token used for exactly one inbound
        // `create_note` call after the allowlist check so the inbound note lands in the
        // correct inbox. For same-namespace sends (from == to), use caller_token
        // unchanged (preserves existing behavior).
        let cross_ns_token;
        let inbound_tok: &NamespaceToken = if from_actor.is_some() || from == recipient_ns_str {
            // Actor-addressed path or same-namespace send: inbound copy stays in caller ns.
            caller_token
        } else {
            cross_ns_token = caller_token.with_namespace(
                Namespace::parse(recipient_ns_str)
                    .expect("recipient_ns_str already validated above"),
            );
            &cross_ns_token
        };

        let mut inbound_props = json!({
            "from": from,
            "to": to,
            "direction": "inbound",
            "subject": subject,
            "thread_id": canonical_thread_id,
            "read": false,
            "sent_at": sent_at,
            "outbound_ref": outbound_note.id,
        });
        if let Some(fa) = from_actor {
            inbound_props["from_actor"] = json!(fa);
        }
        if let Some(ta) = to_actor {
            inbound_props["to_actor"] = json!(ta);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::note::Note;
    use serde_json::json;

    fn make_note(namespace: &str, content: &str, props: Option<Value>) -> Note {
        let mut n = Note::new(namespace, "message", content);
        n.properties = props;
        n
    }

    #[test]
    fn promotes_from_to_subject_when_present() {
        let note = make_note(
            "local",
            "hello",
            Some(json!({
                "from_actor": "lambda:khive",
                "to_actor": "lambda:leo",
                "subject": "Status update",
                "direction": "inbound",
                "read": false,
            })),
        );
        let v = note_to_message_json(&note);
        assert_eq!(v["from"], json!("lambda:khive"));
        assert_eq!(v["to"], json!("lambda:leo"));
        assert_eq!(v["subject"], json!("Status update"));
        assert_eq!(v["direction"], json!("inbound"));
        assert_eq!(v["read"], json!(false));
        assert!(v["content"].is_string());
        assert!(v["properties"].is_object());
    }

    #[test]
    fn from_falls_back_to_namespace_when_from_actor_absent() {
        let note = make_note(
            "legacy-ns",
            "old message",
            Some(json!({ "to_actor": "lambda:leo" })),
        );
        let v = note_to_message_json(&note);
        assert_eq!(v["from"], json!("legacy-ns"));
    }

    #[test]
    fn preview_is_single_line_and_truncated_for_long_content() {
        let long_body = "word ".repeat(40);
        let note = make_note("local", long_body.trim(), None);
        let v = note_to_message_json(&note);
        let preview = v["preview"].as_str().expect("preview is a string");
        assert!(!preview.contains('\n'), "preview must be single-line");
        assert!(
            preview.ends_with('\u{2026}'),
            "long preview must end with ellipsis"
        );
        let without_ellipsis: &str = &preview[..preview.len() - '\u{2026}'.len_utf8()];
        assert!(
            without_ellipsis.chars().count() <= 80,
            "preview body must not exceed 80 chars before ellipsis"
        );
    }

    #[test]
    fn preview_not_truncated_for_short_content() {
        let note = make_note("local", "short message", None);
        let v = note_to_message_json(&note);
        let preview = v["preview"].as_str().expect("preview is a string");
        assert_eq!(preview, "short message");
        assert!(!preview.ends_with('\u{2026}'));
    }

    #[test]
    fn preview_collapses_whitespace_and_newlines() {
        let note = make_note("local", "line one\n  line two\n\nline three", None);
        let v = note_to_message_json(&note);
        let preview = v["preview"].as_str().expect("preview is a string");
        assert_eq!(preview, "line one line two line three");
    }

    #[test]
    fn properties_and_content_still_present() {
        let note = make_note(
            "local",
            "body text",
            Some(json!({ "from_actor": "x", "custom": 42 })),
        );
        let v = note_to_message_json(&note);
        assert_eq!(v["content"], json!("body text"));
        assert_eq!(v["properties"]["custom"], json!(42));
    }

    #[test]
    fn null_defaults_when_no_properties() {
        let note = make_note("local", "no props", None);
        let v = note_to_message_json(&note);
        assert_eq!(v["to"], Value::Null);
        assert_eq!(v["subject"], Value::Null);
        assert_eq!(v["direction"], Value::Null);
        assert_eq!(v["read"], json!(false));
        assert_eq!(v["from"], json!("local"));
    }
}
