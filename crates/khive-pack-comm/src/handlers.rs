//! Verb handler implementations for the comm pack.
//!
//! All eleven public verbs (`send`, `delivered`, `transport_status`, `inbox`, `unread`,
//! `read`, `mark_read`, `reply`, `thread`, `health`, `probe`) store or query comm state.
//! Message-specific metadata lives
//! in the `properties` JSON column; `content` is the message body.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::future::Future;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    is_valid_mailbox_actor_label, EmailMessageIdDomains, KhiveRuntime, MailboxView, NamespaceToken,
    RuntimeError,
};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter, SortDir};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::{Attachment, AttachmentSubstrate, ContentRef, NewAttachment};

use crate::idempotency::MessageIdentity;
use crate::inbox_signal::InboxSignal;
use crate::message::{
    dual_write_message_with_identity, note_to_message_json, project_message_json, resolve_id,
    short_id, validate_message_projection_fields, MessageWrite, COMM_SCHEMA_VERSION,
    COMM_STABLE_PROPERTY_KEYS,
};
use crate::params::{
    deser, CursorCommitParams, CursorGetParams, InboxParams, IngestParams, ProbeParams, ReadParams,
    ReplyParams, SendParams, ThreadParams, TransportStatusParams, UnreadParams,
};

mod email_headers;
use email_headers::{
    build_references_header, external_id_unverifiable, message_id_match_candidates,
    outbound_email_message, parent_references_chain, parent_wire_message_id,
    verified_outbound_email_external_id,
};
#[cfg(test)]
use email_headers::{sanitize_reference_token, wrap_message_id};

mod health;
mod inbox;
mod ingest;
#[cfg(test)]
mod ingest_degradation_tests;
mod parameter_aliases;
mod probe_cursor;
mod quarantine_heartbeat;
mod read_marking;
mod reply;
mod thread;
mod validation;
#[doc(hidden)]
pub use probe_cursor::PROBE_SQL;
pub(crate) use probe_cursor::{handle_cursor_commit, handle_cursor_get, handle_probe};
pub(crate) use thread::handle_thread;

#[cfg(test)]
use health::channel_stalled;
pub(crate) use health::handle_health;

#[cfg(test)]
use inbox::wait_for_inbox_response;
pub(crate) use inbox::{handle_inbox, handle_unread};

#[cfg(test)]
use ingest::committed_ingest_degradations;
pub(crate) use ingest::handle_ingest;

pub(crate) use parameter_aliases::{handle_delivered, handle_mark_read};

#[cfg(test)]
use quarantine_heartbeat::{
    detach_deleted_legacy_original, heartbeat_note_id, note_deleted_after_attempt,
};
pub(crate) use quarantine_heartbeat::{handle_cleanup_expired_quarantine, handle_heartbeat};

#[cfg(test)]
use read_marking::{bulk_read_response, read_response};
use read_marking::{
    mark_read_target, mark_read_targets_atomic, mark_read_targets_best_effort, read_message_fields,
    read_recheck_filter, read_result_with_body, validate_bulk_read_targets, validate_read_target,
};

pub(crate) use reply::handle_reply;

use validation::{
    addressed_recipient, caller_inherits_legacy_pool, canonicalize_ingest_sent_at,
    canonicalize_thread_id, inbox_note_matches, legacy_recipient, parse_inbox_timestamp,
    require_existing_thread_root, send_response_thread_id, thread_id_query_spellings,
    validate_actor_label, validate_inbox_substring,
};

fn add_embedding_truncation_warning(
    response: &mut Value,
    report: &khive_runtime::retrieval::EmbeddingTruncationReport,
) {
    if !report.any_truncated() {
        return;
    }
    if let Some(object) = response.as_object_mut() {
        object.insert(
            "warnings".to_string(),
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        );
    }
}

/// `send` — create a message note in the caller's namespace (outbound) AND
/// deliver an inbound copy addressed to the actor label in `to` (ADR-057).
/// Both copies land in the caller's namespace; no cross-namespace write occurs.
///
/// Caller-keyed sends reconcile through the atomic outbound claim and its
/// intact recipient sibling. Without a key each call creates a new message.
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_send
pub(crate) async fn handle_send(
    runtime: &KhiveRuntime,
    inbox_signal: &InboxSignal,
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
    let thread_id = p
        .thread_id
        .as_deref()
        .map(|raw| canonicalize_thread_id("send", raw))
        .transpose()?;
    if let Some(ref tid) = thread_id {
        require_existing_thread_root(runtime, token, "send", tid).await?;
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
    let sent_by_process = token.process_ref();

    // Pass caller_ns as both `from` and `to` so `from == recipient_ns_str` in
    // dual_write_message, naturally bypassing the cross-namespace allowlist gate
    // (ADR-057 §"Interaction with ADR-040"). Actor labels are stored via from_actor/to_actor.
    let attachments =
        crate::file_attachments::prepare(runtime, "comm.send", &to_actor, &p.attachments).await?;
    let identity = MessageIdentity::new(p.idempotency_key.as_deref(), || {
        crate::file_attachments::identify_request(
            json!({
                "version": 1, "op": "send", "to": to_actor, "content": p.content,
                "subject": p.subject, "thread_id": thread_id,
                "tags": p.tags.as_deref().unwrap_or_default(), "reply_parent_id": null,
            }),
            &p.attachments,
        )
    })?;
    let MessageWrite {
        outbound: outbound_note,
        embedding_truncation,
        replayed,
    } = dual_write_message_with_identity(
        runtime,
        token,
        "comm.send",
        &caller_ns,
        &caller_ns,
        p.subject.as_deref(),
        &p.content,
        thread_id.as_deref(),
        &sent_at,
        sent_by_process,
        Some(&from_actor),
        Some(&to_actor),
        None,
        None,
        p.tags.as_deref(),
        &attachments,
        identity.as_ref(),
    )
    .await?;
    if !replayed {
        inbox_signal.publish();
    }

    // `thread_id` is a strict full-UUID input on a later send. Surface the
    // canonical value persisted by `dual_write_message` so this response can
    // start or continue a thread without fetching the message first (#1482).
    // An empty stored value is treated as absent, and a missing/empty value
    // after a caller-supplied root fails closed instead of silently rooting a
    // new thread (#1623).
    let response_thread_id = send_response_thread_id(thread_id.as_deref(), &outbound_note)?;

    let mut response = json!({
        "id": short_id(outbound_note.id),
        "full_id": outbound_note.id.as_hyphenated().to_string(),
        "thread_id": response_thread_id,
        "from": from_actor,
        "to": p.to,
        "subject": p.subject,
        "sent_at": sent_at,
    });
    if let Some(identity) = identity {
        identity.annotate_response(&mut response, &outbound_note, replayed);
    }
    add_embedding_truncation_warning(&mut response, &embedding_truncation);
    Ok(response)
}

/// Read the runtime's own transport records and verified recipient outcomes.
pub(crate) async fn handle_transport_status(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: TransportStatusParams = deser(params)?;
    let outbound_id = Uuid::parse_str(p.id.trim()).map_err(|_| {
        RuntimeError::InvalidInput(
            "transport_status: a short prefix would require scoped resolution; `id` must \
             be the full outbound UUID returned as `full_id` by comm.send or comm.reply, \
             or surfaced as `outbound_id` in an ambiguous atomic-write error"
                .into(),
        )
    })?;
    let status = runtime.sender_transport_status(token, outbound_id).await?;
    Ok(json!({"id": outbound_id, "status": status}))
}

/// `inbox` — list inbound messages by default, or caller-authored sent rows (ADR-057).
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_inbox
pub const MAX_INBOX_WAIT_MS: u64 = 30_000;

/// `read` — retrieve an inbound message and mark it as read.
pub(crate) async fn handle_read(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ReadParams = deser(params)?;
    let include_body = p.body;
    match (p.id, p.ids) {
        (Some(_), Some(_)) => Err(RuntimeError::InvalidInput(
            "read: `id` and `ids` are mutually exclusive".into(),
        )),
        (None, None) => Err(RuntimeError::InvalidInput(
            "read: exactly one of `id` or `ids` is required".into(),
        )),
        (Some(raw), None) => {
            let (id, note) = validate_read_target(runtime, token, &raw).await?;
            let message = if include_body {
                Some(read_message_fields(runtime, &note).await?)
            } else {
                None
            };
            let result = mark_read_target(runtime, token, id, note).await?;
            Ok(read_result_with_body(result, message))
        }
        (None, Some(raw_ids)) => {
            let (requested_count, targets) =
                validate_bulk_read_targets(runtime, token, raw_ids, "read").await?;
            mark_read_targets_best_effort(runtime, token, requested_count, targets, include_body)
                .await
        }
    }
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

/// `comm.probe` response — a stable, minimal polling contract (khive daemon
/// hardening slice, ADR-D5). Field shape is frozen: do not add fields without
/// updating the frozen contract in the comm pack README.
#[derive(serde::Serialize)]
pub(crate) struct ProbeResponse {
    pub cursor_us: i64,
    pub new_messages: Vec<ProbeMessage>,
    pub stale_unread_count: i64,
    /// Present and `true` only when the caller's `since_us` was discarded and
    /// the page was taken from the baseline instead (#2400). A poller that
    /// cannot see this reads a full baseline page as arrivals, which is
    /// indistinguishable from real mail and repeats on every pass.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_reset: bool,
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

/// Test-only pause point at `handle_heartbeat`'s read/write boundary, so a
/// race between two concurrent callers of the PRODUCTION handler (not the
/// underlying `replace_note_if_unchanged` store primitive) can be reproduced
/// deterministically instead of relying on scheduler luck or sleeps. A no-op
/// unless the calling task runs inside `AFTER_READ_BARRIER.scope(...)`;
/// production dispatch never establishes that scope, so `pause_after_read`
/// costs nothing outside these regression tests, and it does not exist at
/// all in non-test builds.
#[cfg(test)]
mod race_seam;

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "read_cluster_tests.rs"]
mod read_cluster_tests;
