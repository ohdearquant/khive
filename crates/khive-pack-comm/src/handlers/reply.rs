//! Reply creation and subject normalization for the comm pack.

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{EmailMessageIdDomains, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

use crate::idempotency::MessageIdentity;
use crate::inbox_signal::InboxSignal;
use crate::message::{dual_write_message_with_identity, short_id, MessageWrite};
use crate::params::deser;

use super::validation::{
    addressed_recipient, caller_inherits_legacy_pool, caller_is_addressee, legacy_recipient,
    thread_id_query_spellings,
};
use super::{
    add_embedding_truncation_warning, build_references_header, external_id_unverifiable,
    parent_references_chain, parent_wire_message_id, read_recheck_filter,
    verified_outbound_email_external_id, ReplyParams, SortDir,
};

/// `Re: ` + the subject with every leading reply prefix removed and whitespace
/// runs collapsed; empty stays empty. Idempotent, so replying to a reply keeps
/// one `Re: ` and a subject that drifted by whitespace maps back to one form.
pub(crate) fn reply_subject_for(subject: &str) -> String {
    let mut base = subject.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let stripped = base
            .strip_prefix("Re:")
            .or_else(|| base.strip_prefix("RE:"))
            .or_else(|| base.strip_prefix("re:"))
            .map(|rest| rest.trim_start().to_string());
        match stripped {
            Some(rest) => base = rest,
            None => break,
        }
    }
    if base.is_empty() {
        String::new()
    } else {
        format!("Re: {base}")
    }
}

/// `reply` — reply to a message, threading linkage. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_reply
pub(crate) async fn handle_reply(
    runtime: &KhiveRuntime,
    inbox_signal: &InboxSignal,
    email_domains: &Result<Option<EmailMessageIdDomains>, String>,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ReplyParams = deser(params)?;
    let id = runtime
        .resolve_uuid_or_prefix_for_verb(token, &p.id, "reply")
        .await?;
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

    // Every nonempty outbound external_id would become a parent mail header
    // below, including a legacy row with no channel metadata at all.
    if orig_props.get("direction").and_then(Value::as_str) == Some("outbound") {
        if let Some(external_id) = orig_props.get("external_id").and_then(Value::as_str) {
            if !external_id.is_empty()
                && !verified_outbound_email_external_id(&original, email_domains)
            {
                return Err(external_id_unverifiable(
                    original.id,
                    "outbound parent has no own-ID-bound Message-ID in the configured sending domains",
                ));
            }
        }
    }

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

    let caller_actor = token.actor().id.as_str();
    let is_participant = addressed_recipient(Some(&orig_props)).is_some_and(|recipient| {
        recipient == caller_actor || original_from_actor.as_deref() == Some(caller_actor)
    }) || (caller_inherits_legacy_pool(token)
        && legacy_recipient(Some(&orig_props))
        && original_from_actor
            .as_deref()
            .is_none_or(|sender| sender == caller_actor));
    if !is_participant {
        return Err(RuntimeError::InvalidInput(format!(
            "reply: that message is not addressed to or from caller actor {caller_actor:?}"
        )));
    }

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

    // The reply subject derives from the thread ROOT's stored subject, not from
    // the message being replied to. An inbound subject is a decoded mail header
    // and can drift (whitespace at encoded-word boundaries, client re-encoding);
    // echoing it compounds the drift on every round trip until mail clients stop
    // threading the exchange. The root is the one subject this side authored or
    // first received.
    //
    // The root is resolved by thread MEMBERSHIP, the earliest message carrying
    // this `thread_id`, never by id equality: `comm.ingest` mints a thread id
    // that is not the root note's id, so a thread opened by an inbound mail has
    // no note AT the thread id, and an id lookup would silently fall back to
    // the drifted subject for exactly the exchanges this rule exists for.
    // Ordering is by the message's own `sent_at`; only rows that carry a text
    // `sent_at` are candidates, because SQL NULL sorts first under ASC and a
    // legacy member without one would otherwise be taken for the root.
    //
    // The root's subject is used only when the caller is a party to the root,
    // the same thread-participant predicate `reply` enforces on the replied-to
    // message above (issue #113): a caller who self-sends into a foreign
    // thread id must not learn that thread's subject through its own reply.
    // Falls back to the replied-to message's subject when the root is the
    // message itself, unreadable, not the caller's, or has no subject.
    let root_subject = match Uuid::parse_str(&thread_id) {
        Ok(root_uuid) if root_uuid != original.id => {
            let spellings = thread_id_query_spellings(root_uuid, None)
                .into_iter()
                .map(SqlValue::Text)
                .collect();
            let root_filter = NoteFilter {
                kind: Some("message".to_string()),
                property_filters: vec![
                    PropertyFilter {
                        json_path: "$.thread_id".to_string(),
                        op: FilterOp::In(spellings),
                        value: SqlValue::Null,
                    },
                    PropertyFilter {
                        json_path: "$.sent_at".to_string(),
                        op: FilterOp::JsonTypeEq,
                        value: SqlValue::Text("text".to_string()),
                    },
                ],
                order_by: Some(("$.sent_at".to_string(), SortDir::Asc)),
                ..Default::default()
            };
            let caller_actor = token.actor().id.as_str();
            store
                .query_notes_filtered_count_free(
                    token.namespace().as_str(),
                    &root_filter,
                    PageRequest {
                        limit: 1,
                        offset: 0,
                    },
                )
                .await
                .ok()
                .and_then(|page| page.items.into_iter().next())
                .and_then(|root| {
                    let props = root.properties?;
                    let is_party =
                        |key: &str| props.get(key).and_then(Value::as_str) == Some(caller_actor);
                    if !(is_party("from_actor") || is_party("to_actor")) {
                        return None;
                    }
                    props
                        .get("subject")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|subject| !subject.trim().is_empty())
        }
        _ => None,
    };
    let base_subject = root_subject.unwrap_or(original_subject);
    let reply_subject = reply_subject_for(&base_subject);

    let caller_ns = token.namespace().as_str().to_string();
    let from_actor_label = token.actor().id.clone();
    let sent_at = Utc::now().to_rfc3339();
    let sent_by_process = token.process_ref();

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
    let attachments =
        crate::file_attachments::prepare(runtime, "comm.reply", &reply_to, &p.attachments).await?;
    let identity = MessageIdentity::new(p.idempotency_key.as_deref(), || {
        crate::file_attachments::identify_request(
            json!({
                "version": 1, "op": "reply", "to": reply_to, "content": p.content,
                "subject": reply_subject_opt, "thread_id": thread_id,
                "tags": p.tags.as_deref().unwrap_or_default(), "reply_parent_id": id,
            }),
            &p.attachments,
        )
    })?;
    let MessageWrite {
        outbound: reply_note,
        embedding_truncation,
        replayed,
    } = dual_write_message_with_identity(
        runtime,
        token,
        "comm.reply",
        &caller_ns,
        &caller_ns,
        reply_subject_opt,
        &p.content,
        Some(&thread_id),
        &sent_at,
        sent_by_process,
        Some(&reply_from_actor),
        Some(&reply_to_actor),
        in_reply_to_message_id.as_deref(),
        references_chain.as_deref(),
        p.tags.as_deref(),
        &attachments,
        identity.as_ref(),
    )
    .await?;
    if !replayed {
        inbox_signal.publish();
    }

    // Replying is the strongest possible read signal, and callers universally
    // chained `reply | read` to say so — fold it in. Skips only an explicitly
    // outbound original, matching handle_read's rejection exactly rather than
    // requiring a literal "inbound" (legacy messages may carry no direction).
    // Best-effort: the reply is already committed above, so a failed or
    // no-op patch degrades to `marked_read: false` rather than failing a
    // delivered reply.
    //
    // Reply participation does not grant addressee-owned read state. The
    // mutation rechecks the same recipient policy as read/mark_read.
    let original_direction = orig_props
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("");
    let caller_is_addressee = caller_is_addressee(token, Some(&orig_props));
    let marked_read = if replayed || original_direction == "outbound" || !caller_is_addressee {
        None
    } else {
        let updated_at = Utc::now().timestamp_micros();
        // `Ok(false)` means no live row was updated (e.g. the original was
        // soft-deleted mid-flight, or its properties ceased to be an object)
        // — that is not a successful mark. The one-statement property set
        // preserves every unrelated key without a race window (#1483).
        Some(
            store
                .try_patch_note_property(
                    id,
                    &original.namespace,
                    &read_recheck_filter(token),
                    "$.read",
                    json!(true),
                    updated_at,
                )
                .await
                .unwrap_or(false),
        )
    };

    let mut response = json!({
        "id": short_id(reply_note.id),
        "full_id": reply_note.id.as_hyphenated().to_string(),
        "thread_id": thread_id,
        "from": from_actor_label,
        "to": reply_to,
        "subject": reply_subject,
        "sent_at": sent_at,
        "marked_read": marked_read,
    });
    if let Some(identity) = identity {
        identity.annotate_response(&mut response, &reply_note, replayed);
    }
    add_embedding_truncation_warning(&mut response, &embedding_truncation);
    Ok(response)
}
