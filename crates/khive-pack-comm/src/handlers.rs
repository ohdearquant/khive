//! Verb handler implementations for the comm pack.
//!
//! All eleven public verbs (`send`, `delivered`, `transport_status`, `inbox`, `unread`,
//! `read`, `mark_read`, `reply`, `thread`, `health`, `probe`) store or query comm state.
//! Message-specific metadata lives
//! in the `properties` JSON column; `content` is the message body.

use std::collections::{HashMap, HashSet};
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
    deser, CleanupExpiredQuarantineParams, CursorCommitParams, CursorGetParams, HeartbeatParams,
    InboxParams, IngestParams, ProbeParams, QuarantineCleanupMode, ReadParams, ReplyParams,
    SendParams, ThreadParams, TransportStatusParams, UnreadParams,
};

#[cfg(test)]
mod ingest_degradation_tests;
mod parameter_aliases;
mod read_marking;
mod thread;
mod validation;
pub(crate) use thread::handle_thread;

pub(crate) use parameter_aliases::{handle_delivered, handle_mark_read};

#[cfg(test)]
use read_marking::{bulk_read_response, read_response};
use read_marking::{
    mark_read_target, mark_read_targets_atomic, mark_read_targets_best_effort, read_message_fields,
    read_recheck_filter, read_result_with_body, validate_bulk_read_targets, validate_read_target,
};

use validation::{
    addressed_recipient, caller_inherits_legacy_pool, caller_is_addressee,
    canonicalize_ingest_sent_at, canonicalize_thread_id, inbox_note_matches, legacy_recipient,
    parse_inbox_timestamp, parse_supplied_timestamp, require_existing_thread_root,
    send_response_thread_id, thread_id_query_spellings, validate_actor_label,
    validate_inbox_substring,
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

pub(crate) async fn handle_inbox(
    runtime: &KhiveRuntime,
    inbox_signal: &InboxSignal,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: InboxParams = deser(params.clone())?;
    let view =
        runtime.authorize_mailbox_view(token, "comm.inbox", p.mailbox_actor.as_deref(), &params)?;
    let include_legacy = !view.delegated && caller_inherits_legacy_pool(token);
    let thread_id = p
        .thread_id
        .as_deref()
        .map(|raw| canonicalize_thread_id("inbox", raw))
        .transpose()?;
    validate_message_projection_fields("inbox", p.fields.as_deref())?;
    let wait_ms = p.wait_ms.unwrap_or(0);
    if wait_ms > MAX_INBOX_WAIT_MS {
        return Err(RuntimeError::InvalidInput(format!(
            "inbox: `wait_ms` must be at most {MAX_INBOX_WAIT_MS}"
        )));
    }
    let raw_limit = p.limit.unwrap_or(20);
    if raw_limit > 200 {
        return Err(RuntimeError::InvalidInput(format!(
            "inbox: `limit` must be at most 200, got {raw_limit}"
        )));
    }
    let offset = p.offset.unwrap_or(0);
    if offset > i64::MAX as u64 {
        return Err(RuntimeError::InvalidInput(format!(
            "inbox: `offset` must be <= {}, got {offset}",
            i64::MAX
        )));
    }

    let mailbox = match p.mailbox.as_deref().unwrap_or("inbox") {
        mailbox @ ("inbox" | "sent") => mailbox,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "inbox: invalid `box` {other:?}; expected one of: inbox, sent"
            )));
        }
    };

    if mailbox == "sent" {
        if view.delegated {
            return Err(RuntimeError::InvalidInput(
                "inbox: delegated mailbox reads do not support box=\"sent\"".into(),
            ));
        }
        if p.status.is_some() {
            return Err(RuntimeError::InvalidInput(
                "inbox: `status` applies only to box=\"inbox\"; omit it for box=\"sent\"".into(),
            ));
        }
        if p.from_actor.is_some() || p.from_prefix.is_some() || p.exclude_from_actor.is_some() {
            return Err(RuntimeError::InvalidInput(
                "inbox: sender filters apply only to box=\"inbox\"; use `to_actor` to filter box=\"sent\""
                    .into(),
            ));
        }
    } else if p.to_actor.is_some() {
        return Err(RuntimeError::InvalidInput(
            "inbox: `to_actor` applies only to box=\"sent\"".into(),
        ));
    }

    // #493: from_actor / from_prefix sender filter — mutually exclusive.
    if p.from_actor.is_some() && p.from_prefix.is_some() {
        return Err(RuntimeError::InvalidInput(
            "inbox: `from_actor` and `from_prefix` are mutually exclusive".into(),
        ));
    }

    let status =
        match p
            .status
            .as_deref()
            .unwrap_or(if mailbox == "inbox" { "unread" } else { "all" })
        {
            s @ ("unread" | "read" | "all") => s,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "inbox: invalid status {other:?}; expected one of: unread, read, all"
                )));
            }
        };

    validate_inbox_substring("subject_contains", p.subject_contains.as_deref())?;
    validate_inbox_substring("content_contains", p.content_contains.as_deref())?;
    // Stored actor labels are never empty (`send`/`ingest` validate them), so
    // an empty exact-match filter can only be caller error; reject it like the
    // substring filters above instead of silently matching nothing.
    validate_inbox_substring("to_actor", p.to_actor.as_deref())?;

    let since_micros = p
        .since
        .as_deref()
        .map(|raw| parse_inbox_timestamp("since", raw))
        .transpose()?;
    let before_micros = p
        .before
        .as_deref()
        .map(|raw| parse_inbox_timestamp("before", raw))
        .transpose()?;
    if matches!((since_micros, before_micros), (Some(since), Some(before)) if since >= before) {
        return Err(RuntimeError::InvalidInput(
            "inbox: `since` must be earlier than `before`".into(),
        ));
    }

    if raw_limit == 0 {
        let unread = if mailbox == "inbox" {
            let store = runtime.notes(token)?;
            count_unread_messages(
                store.as_ref(),
                token.namespace().as_str(),
                &view,
                include_legacy,
            )
            .await?
        } else {
            UnreadCount::zero()
        };
        return Ok(json!({
            "messages": [],
            "count": 0,
            "unread_count": unread.count,
            "unread_count_cap": unread.cap,
            "unread_count_saturated": unread.saturated,
            "offset": offset,
            "next_offset": Value::Null,
            "has_more": false,
        }));
    }
    let limit = raw_limit as usize;

    // Push direction + read-status into SQL for idx_comm_message_direction; json_type
    // read-check keeps only JSON boolean `true` as read (matches prior as_bool semantics).
    let mut property_filters = vec![PropertyFilter {
        json_path: "$.direction".to_string(),
        op: FilterOp::Eq,
        value: SqlValue::Text(
            if mailbox == "inbox" {
                "inbound"
            } else {
                "outbound"
            }
            .to_string(),
        ),
    }];
    if mailbox == "inbox" {
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
    }

    if mailbox == "inbox" {
        // Only the anonymous fallback inherits the missing/null partition.
        // Every named/delegated view uses the typed exact-recipient seek.
        property_filters.push(PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op: if !include_legacy {
                FilterOp::EqOrMissingIndexed
            } else {
                FilterOp::EqOrLegacyIndexed
            },
            value: SqlValue::Text(view.actor_id.clone()),
        });
        if !include_legacy {
            property_filters.push(PropertyFilter {
                json_path: "$.to_actor".to_string(),
                op: FilterOp::JsonTypeEq,
                value: SqlValue::Text("text".to_string()),
            });
        }
        if let Some(from_actor) = p.from_actor.as_ref() {
            property_filters.push(PropertyFilter {
                json_path: "$.from_actor".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text(from_actor.clone()),
            });
        }
    } else {
        property_filters.push(PropertyFilter {
            json_path: "$.from_actor".to_string(),
            op: if include_legacy {
                FilterOp::EqOrMissing
            } else {
                FilterOp::Eq
            },
            value: SqlValue::Text(view.actor_id.clone()),
        });
        if let Some(to_actor) = p.to_actor.as_ref() {
            property_filters.push(PropertyFilter {
                json_path: "$.to_actor".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text(to_actor.clone()),
            });
        }
    }

    if let Some(thread_id) = thread_id {
        property_filters.push(PropertyFilter {
            json_path: "$.thread_id".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(thread_id),
        });
    }

    let filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        order_by: None, // preserves existing created_at DESC ordering
        min_created_at: since_micros,
        ..Default::default()
    };
    let store = runtime.notes(token)?;
    let deadline = (wait_ms > 0)
        .then(|| tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms));
    let subject_needle = p
        .subject_contains
        .as_ref()
        .map(|value| value.to_lowercase());
    let content_needle = p
        .content_contains
        .as_ref()
        .map(|value| value.to_lowercase());

    let store = store.as_ref();
    let namespace = token.namespace().as_str();
    wait_for_inbox_response(inbox_signal, deadline, || {
        query_inbox_response(
            runtime,
            store,
            namespace,
            &view,
            include_legacy,
            &filter,
            &p,
            before_micros,
            subject_needle.as_deref(),
            content_needle.as_deref(),
            offset,
            limit,
        )
    })
    .await
}

async fn wait_for_inbox_response<Query, QueryFuture>(
    inbox_signal: &InboxSignal,
    deadline: Option<tokio::time::Instant>,
    mut query: Query,
) -> Result<Value, RuntimeError>
where
    Query: FnMut() -> QueryFuture,
    QueryFuture: std::future::Future<Output = Result<Value, RuntimeError>>,
{
    loop {
        // Snapshot before querying. If a writer commits between this query and
        // waiter registration, the generation change makes the wait immediately ready.
        let observed = inbox_signal.snapshot();
        let response = query().await?;
        let is_empty = response["messages"]
            .as_array()
            .ok_or_else(|| {
                RuntimeError::Internal("inbox: response is missing the `messages` array".into())
            })?
            .is_empty();
        if !is_empty || deadline.is_none() {
            return Ok(response);
        }

        // Enforce one fixed deadline before registering another wait. If the
        // query itself crossed that deadline, only a publish observed during
        // the query earns one final re-query; unrelated publish streams can
        // never reset the budget.
        let deadline_at = *deadline.as_ref().expect("non-empty wait has a deadline");
        if tokio::time::Instant::now() >= deadline_at {
            // The query itself may have crossed the deadline. If a writer
            // published after that query took its storage snapshot, returning
            // `response` here would lose the wake. Re-query once to observe the
            // committed row; the fixed deadline still prevents another wait.
            if inbox_signal.snapshot() != observed {
                return query().await;
            }
            return Ok(response);
        }

        let wait_result =
            tokio::time::timeout_at(deadline_at, inbox_signal.wait_for_change(observed)).await;
        if wait_result.is_err() {
            // Close the timeout-edge race: a commit concurrent with deadline
            // expiry is still visible even if the timer wins the select.
            return query().await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn query_inbox_response(
    runtime: &KhiveRuntime,
    store: &dyn khive_storage::NoteStore,
    namespace: &str,
    view: &MailboxView,
    include_legacy: bool,
    filter: &NoteFilter,
    params: &InboxParams,
    before_micros: Option<i64>,
    subject_needle: Option<&str>,
    content_needle: Option<&str>,
    offset: u64,
    limit: usize,
) -> Result<Value, RuntimeError> {
    let has_post_filter = params.kind.is_some()
        || params.tags.as_ref().is_some_and(|tags| !tags.is_empty())
        || params.from_prefix.is_some()
        || params.exclude_from_actor.is_some()
        || before_micros.is_some()
        || subject_needle.is_some()
        || content_needle.is_some();

    // Offset is defined over the fully-filtered sequence. When a filter cannot
    // be represented by `NoteFilter`, scan the indexed base query and count only
    // matching rows before collecting one lookahead item for `has_more`.
    //
    // Each page is fetched from a keyset boundary (`NoteFilter.after`), not a
    // growing `PageRequest.offset`: an offset re-walks every earlier row on
    // every page, so this loop's total work was quadratic in the number of
    // pages scanned before a post-filter match was found. Seeking from the
    // last row's `(created_at, id)` makes each page's fetch cost independent
    // of how many pages came before it. `filter.order_by` is always `None`
    // here (see its construction above), which `after` requires.
    let mut messages: Vec<Value> = if has_post_filter {
        const PAGE_SIZE: u32 = 200;
        let mut collected: Vec<Value> = Vec::new();
        let mut matched: u64 = 0;
        let mut cursor: Option<khive_storage::note::NoteSeekAfter> = None;
        loop {
            let mut page_filter = filter.clone();
            page_filter.after = cursor;
            let page = store
                .query_notes_filtered_count_free(
                    namespace,
                    &page_filter,
                    PageRequest {
                        limit: PAGE_SIZE,
                        offset: 0,
                    },
                )
                .await?;
            let fetched = page.items.len() as u32;
            cursor = page
                .items
                .last()
                .map(|n| khive_storage::note::NoteSeekAfter {
                    created_at: n.created_at,
                    id: n.id,
                });
            for n in &page.items {
                if !inbox_note_matches(n, params, before_micros, subject_needle, content_needle) {
                    continue;
                }
                if matched < offset {
                    matched += 1;
                    continue;
                }
                collected.push(note_to_message_json(n));
                if collected.len() > limit {
                    break;
                }
            }
            if collected.len() > limit || fetched < PAGE_SIZE {
                break;
            }
        }
        collected
    } else {
        let page = store
            .query_notes_filtered_count_free(
                namespace,
                filter,
                PageRequest {
                    limit: (limit + 1) as u32,
                    offset,
                },
            )
            .await?;
        page.items.iter().map(note_to_message_json).collect()
    };

    let has_more = messages.len() > limit;
    if has_more {
        messages.truncate(limit);
    }
    crate::file_attachments::enrich_many(runtime, messages.iter_mut().collect()).await?;
    let count = messages.len();
    // This is a mailbox-wide signal; page and status filters only shape `messages`.
    let unread = if params.mailbox.as_deref().unwrap_or("inbox") == "inbox" {
        count_unread_messages(store, namespace, view, include_legacy).await?
    } else {
        UnreadCount::zero()
    };
    let next_offset = if has_more {
        Some(offset.checked_add(count as u64).ok_or_else(|| {
            RuntimeError::InvalidInput("inbox: pagination offset overflowed".into())
        })?)
    } else {
        None
    };
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|message| project_message_json(message, params.fields.as_deref()))
        .collect();
    Ok(json!({
        "messages": messages,
        "count": count,
        "unread_count": unread.count,
        "unread_count_cap": unread.cap,
        "unread_count_saturated": unread.saturated,
        "offset": offset,
        "next_offset": next_offset,
        "has_more": has_more,
    }))
}

/// `unread` — count-only view of the caller's unread inbound messages (#66):
/// same filter stack as `inbox(status="unread")`.
pub(crate) async fn handle_unread(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let _: UnreadParams = deser(params)?;
    let caller_actor = token.actor().id.clone();
    let store = runtime.notes(token)?;
    let view = MailboxView {
        actor_id: caller_actor.clone(),
        delegated: false,
    };
    let unread = count_unread_messages(
        store.as_ref(),
        token.namespace().as_str(),
        &view,
        caller_inherits_legacy_pool(token),
    )
    .await?;

    Ok(json!({
        "count": unread.count,
        "count_cap": unread.cap,
        "count_saturated": unread.saturated,
        "actor": caller_actor,
    }))
}

const UNREAD_COUNT_CAP: u32 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UnreadCount {
    count: u64,
    cap: u64,
    saturated: bool,
}

impl UnreadCount {
    fn zero() -> Self {
        Self {
            count: 0,
            cap: u64::from(UNREAD_COUNT_CAP),
            saturated: false,
        }
    }
}

async fn count_unread_messages(
    store: &dyn khive_storage::NoteStore,
    namespace: &str,
    view: &MailboxView,
    include_legacy: bool,
) -> Result<UnreadCount, RuntimeError> {
    let mut base_filters = vec![
        PropertyFilter {
            json_path: "$.direction".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("inbound".to_string()),
        },
        PropertyFilter {
            json_path: "$.read".to_string(),
            op: FilterOp::JsonTypeNeMissing,
            value: SqlValue::Text("true".to_string()),
        },
    ];
    if !include_legacy {
        // json_extract also returns object/array JSON as text; only string labels
        // may match an attributed recipient, even if an actor label looks like JSON.
        base_filters.push(PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op: FilterOp::JsonTypeEq,
            value: SqlValue::Text("text".to_string()),
        });
    }
    let count_filter = |op| {
        let mut property_filters = base_filters.clone();
        property_filters.push(PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op,
            value: SqlValue::Text(view.actor_id.clone()),
        });
        NoteFilter {
            kind: Some("message".to_string()),
            property_filters,
            order_by: None,
            ..Default::default()
        }
    };
    // Anonymous fallback views count addressed and missing/null partitions in
    // one snapshot. Named/delegated views count only the typed exact partition;
    // every bounded subquery retains the corresponding pinned recipient seek.
    let mut filters = vec![count_filter(FilterOp::EqOrMissingIndexed)];
    if include_legacy {
        filters.push(count_filter(FilterOp::JsonTypeMissingOrNullIndexed));
    }
    let counts = store
        .count_notes_filtered_bounded_in_snapshot(namespace, &filters, UNREAD_COUNT_CAP)
        .await?;
    if counts.len() != filters.len() {
        return Err(RuntimeError::Internal(
            "comm.unread: storage returned an invalid partition count vector".into(),
        ));
    }
    let cap = u64::from(UNREAD_COUNT_CAP);
    if counts.iter().any(|count| count.cap != cap) {
        return Err(RuntimeError::Internal(
            "comm.unread: storage returned an invalid bounded-count cap".into(),
        ));
    }
    let observed = counts
        .iter()
        .fold(0_u64, |sum, count| sum.saturating_add(count.count));
    Ok(UnreadCount {
        count: observed.min(cap),
        cap,
        saturated: counts.iter().any(|count| count.saturated) || observed > cap,
    })
}

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

/// Reuse the stored thread identity for either form of duplicate detection.
fn duplicate_ingest_ack(duplicate: &Note, external_id: Option<&str>) -> Value {
    // Preserve the stored thread identity, including a pre-v1 free-form label.
    // A row without thread_id falls back to its own UUID and is flagged.
    let existing_thread_id = duplicate
        .properties
        .as_ref()
        .and_then(|properties| properties.get("thread_id"))
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string);
    let mut ack = json!({
        "ok": true,
        "deduplicated": true,
        "external_id": external_id,
        "thread_id": existing_thread_id
            .clone()
            .unwrap_or_else(|| duplicate.id.as_hyphenated().to_string()),
    });
    if existing_thread_id.is_none() {
        ack["thread_id_warning"] = json!(
            "stored duplicate has no thread_id (legacy row); echoed the message's \
             own note UUID as the thread root"
        );
    } else if existing_thread_id
        .as_deref()
        .is_some_and(|raw| raw.parse::<Uuid>().is_err())
    {
        ack["thread_id_canonical"] = json!(false);
    }
    ack
}

/// A duplicate quarantine must prove replay ownership before acknowledgement.
/// This applies equally to the account key and its one-release legacy IMAP key.
#[allow(clippy::too_many_arguments)]
async fn repair_duplicate_quarantine(
    runtime: &KhiveRuntime,
    ns: &str,
    duplicate: &Note,
    attachment: Option<&NewAttachment>,
    channel_kind: Option<&str>,
    channel_slug: Option<&str>,
    retention: std::time::Duration,
    extend_retention: bool,
) -> Result<(), RuntimeError> {
    let Some(attachment) = attachment else {
        return Ok(());
    };
    let stored_ref = duplicate
        .properties
        .as_ref()
        .and_then(|properties| properties.get("quarantine_content_ref"));
    let needs_ref_backfill = stored_ref.is_none();
    if stored_ref.is_some_and(|value| value.as_str() != Some(attachment.content_ref.as_str())) {
        return Err(RuntimeError::InvalidInput(
            "ingest: duplicate quarantine external_id holds different original bytes".to_string(),
        ));
    }
    if needs_ref_backfill
        && !duplicate.properties.as_ref().is_some_and(|properties| {
            matches!(properties.get("quarantined"), Some(Value::Bool(true)))
                || properties.get("quarantined").and_then(Value::as_str) == Some("true")
        })
    {
        return Err(RuntimeError::InvalidInput(
            "ingest: duplicate without an original reference is not quarantined".to_string(),
        ));
    }
    let stored_channel_kind = duplicate
        .properties
        .as_ref()
        .and_then(|properties| properties.get("channel_kind"))
        .and_then(Value::as_str);
    let stored_channel_slug = duplicate
        .properties
        .as_ref()
        .and_then(|properties| properties.get("channel_slug"))
        .and_then(Value::as_str);
    if stored_channel_kind != channel_kind
        || stored_channel_slug.is_some_and(|slug| Some(slug) != channel_slug)
    {
        return Err(RuntimeError::InvalidInput(
            "ingest: duplicate quarantine channel identity disagrees with replay".to_string(),
        ));
    }
    if needs_ref_backfill && (stored_channel_kind.is_none() || channel_slug.is_none()) {
        return Err(RuntimeError::InvalidInput(
            "ingest: duplicate quarantine needs channel identity to attach original bytes"
                .to_string(),
        ));
    }
    // Compute the replay grace before installing the owner, so an
    // unrepresentable deadline cannot leave a partially repaired row.
    // A current-key replay extends retention. A legacy-key row with no
    // deadline at all also gets one, because the repair makes it the owner of
    // the original bytes and no cleanup selector reaches a slugged row whose
    // expiry is NULL. A legacy row that already has a deadline keeps it.
    let install_deadline = extend_retention || duplicate.expires_at.is_none();
    let replay_deadline =
        if install_deadline && stored_channel_kind.is_some() && channel_slug.is_some() {
            let grace_us = i64::try_from(retention.as_micros()).map_err(|_| {
                RuntimeError::InvalidInput(
                    "ingest: quarantine replay retention exceeds i64 microseconds".into(),
                )
            })?;
            Some(
                Utc::now()
                    .timestamp_micros()
                    .checked_add(grace_us)
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "ingest: quarantine replay expiry exceeds i64 microseconds".into(),
                        )
                    })?,
            )
        } else {
            None
        };
    let store = runtime.core().attachments()?;
    match store
        .get_attachment(duplicate.id, "quarantine-original")
        .await?
    {
        Some(existing) if existing.content_ref == attachment.content_ref => {}
        Some(_) => {
            return Err(RuntimeError::Internal(
                "ingest: duplicate quarantine attachment disagrees with note metadata".to_string(),
            ));
        }
        None => {
            // Retry/backfill an older metadata-only quarantine.
            // A failed owner write keeps the channel cursor stalled.
            let blob = runtime.blob_store().ok_or_else(|| {
                RuntimeError::Unconfigured(
                    "ingest: quarantine replay requires a BlobStore".to_string(),
                )
            })?;
            if !blob.exists(&attachment.content_ref).await? {
                return Err(RuntimeError::InvalidInput(
                    "ingest: duplicate quarantine original is not published".to_string(),
                ));
            }
            #[cfg(test)]
            race_seam::pause_after_quarantine_role_read().await;
            if !store
                .try_insert_attachment(Attachment::from_new(
                    duplicate.id,
                    AttachmentSubstrate::Note,
                    (*attachment).clone(),
                    duplicate.created_at,
                ))
                .await?
            {
                // A writer installed the role after our first read.
                // A matching reference is an idempotent replay; a
                // different one must not be acknowledged as repaired.
                let current = store
                    .get_attachment(duplicate.id, "quarantine-original")
                    .await?;
                if !matches!(current, Some(ref existing)
                    if existing.substrate == AttachmentSubstrate::Note
                        && existing.content_ref == attachment.content_ref)
                {
                    return Err(RuntimeError::Internal(
                        "ingest: duplicate quarantine attachment changed during repair".to_string(),
                    ));
                }
            }
        }
    }
    if let (Some(channel_kind), Some(channel_slug)) = (stored_channel_kind, channel_slug) {
        if needs_ref_backfill || replay_deadline.is_some() {
            // The duplicate lookup has already matched the exact channel kind
            // and slug. Repair a pre-attachment quarantine's ContentRef before
            // acknowledging it, and install or extend retention for a
            // current-key replay or a row with no deadline. A concurrent
            // identity change cannot redirect cleanup.
            let sql = runtime.sql();
            let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
            let repaired = writer
                .execute(SqlStatement {
                    sql: khive_runtime::sql!("quarantine_duplicate_retention_repair").into(),
                    params: vec![
                        SqlValue::Text(duplicate.id.as_hyphenated().to_string()),
                        SqlValue::Text(ns.to_string()),
                        SqlValue::Text(attachment.content_ref.to_string()),
                        SqlValue::Text(channel_kind.to_string()),
                        SqlValue::Text(channel_slug.to_string()),
                        replay_deadline.map_or(SqlValue::Null, SqlValue::Integer),
                        SqlValue::Integer(Utc::now().timestamp_micros()),
                    ],
                    label: Some("comm_quarantine_duplicate_retention_repair".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;
            if repaired != 1 {
                return Err(RuntimeError::InvalidInput(
                    "ingest: duplicate quarantine changed during retention repair".to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// Match the same channel-scoped key enforced by the durable external-ID index.
/// An absent channel field occupies the empty index partition; transport-owned
/// fields on a validated ingest are non-empty and compared exactly.
fn ingest_external_id_filter(
    external_id: &str,
    channel_kind: Option<&str>,
    channel_slug: Option<&str>,
) -> NoteFilter {
    let mut property_filters = vec![PropertyFilter {
        json_path: "$.external_id".to_string(),
        op: FilterOp::Eq,
        value: SqlValue::Text(external_id.to_string()),
    }];
    for (json_path, value) in [
        ("$.channel_kind", channel_kind),
        ("$.channel_slug", channel_slug),
    ] {
        property_filters.push(PropertyFilter {
            json_path: json_path.to_string(),
            op: if value.is_some() {
                FilterOp::Eq
            } else {
                FilterOp::EqOrMissingIndexed
            },
            value: SqlValue::Text(value.unwrap_or_default().to_string()),
        });
    }
    NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        ..Default::default()
    }
}

/// `ingest` — write a single inbound message note from a channel adapter.
/// `Visibility::Subhandler`: not accessible via the MCP wire, only callable
/// in-process (e.g. the polling loop in `khive-mcp`); the authoritative write
/// path for all channel-delivered messages. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_ingest
pub(crate) async fn handle_ingest(
    runtime: &KhiveRuntime,
    inbox_signal: &InboxSignal,
    channel_ingest_capability: Option<&khive_runtime::ChannelIngestCapability>,
    email_domains: &Result<Option<EmailMessageIdDomains>, String>,
    token: &NamespaceToken,
    params: Value,
    quarantine_retention: std::time::Duration,
) -> Result<Value, RuntimeError> {
    // Note: IngestParams does not use deny_unknown_fields.
    let mut p: IngestParams = serde_json::from_value(params)
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
    if let Some(raw) = p.channel_kind.as_deref() {
        let normalized = raw.trim();
        if normalized.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "ingest: `channel_kind` must not be empty when supplied".into(),
            ));
        }
        p.channel_kind = Some(normalized.to_string());
    }
    if let Some(raw) = p.channel_slug.as_deref() {
        let normalized = raw.trim();
        if normalized.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "ingest: `channel_slug` must not be empty when supplied".into(),
            ));
        }
        if p.channel_kind.is_none() {
            return Err(RuntimeError::InvalidInput(
                "ingest: `channel_slug` requires `channel_kind`".into(),
            ));
        }
        p.channel_slug = Some(normalized.to_string());
    }
    // #479a: a non-empty malformed thread_id must fail closed, not silently get a
    // fresh UUID (which would split the message into the wrong conversation).
    // Accepted compact/braced UUID inputs are canonicalized before any v1 row
    // can be stamped so exact-string thread lookups remain coherent.
    let supplied_thread_id = p
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw| canonicalize_thread_id("ingest", raw))
        .transpose()?;

    // An omitted timestamp means "observed now". A supplied value, including
    // an empty string, must resolve to an instant before the row is labelled as
    // message-properties v1; accepting arbitrary text would make the marker lie.
    let sent_at = match p.sent_at.as_deref() {
        Some(raw) => canonicalize_ingest_sent_at(raw)?,
        None => Utc::now().to_rfc3339(),
    };

    // Trusted-ingest entry point: comm.ingest is the sole legitimate writer of
    // transport-owned quarantine disposition and channel provenance (`quarantined`,
    // `channel_kind`, `channel_slug`), derived above from the inbound transport
    // itself. Every other write path uses `try_create_note`, which refuses them.
    // A missing grant is a composition/startup defect (this `CommPack` instance
    // was never granted the capability), not a caller input error — classified
    // as `Unconfigured` so it is not confused with a malformed request.
    // Check before duplicate acknowledgements as well as the trusted write.
    let capability = channel_ingest_capability.ok_or_else(|| {
        RuntimeError::Unconfigured(
            "comm pack instance holds no channel-ingest capability grant; refusing to \
             establish transport-owned message properties"
                .to_string(),
        )
    })?;

    let ns = token.namespace().as_str();
    let store = runtime.notes(token)?;

    // Parse quarantine replay ownership before either dedup path can acknowledge it.
    // Adapter metadata is merged below without replacing these keys.
    let is_quarantined = p.metadata.as_ref().is_some_and(|metadata| {
        matches!(metadata.get("quarantined"), Some(Value::Bool(true)))
            || metadata.get("quarantined").and_then(Value::as_str) == Some("true")
    });
    let quarantine_attachment = if is_quarantined {
        match p
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("quarantine_content_ref"))
        {
            None => None,
            Some(Value::String(raw)) => Some(NewAttachment {
                role: "quarantine-original".to_string(),
                content_ref: ContentRef::from_hex(raw).map_err(|error| {
                    RuntimeError::InvalidInput(format!(
                        "ingest: invalid quarantine_content_ref: {error}"
                    ))
                })?,
                media_type: None,
                size_bytes: None,
            }),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(
                    "ingest: quarantine_content_ref must be a ContentRef string".to_string(),
                ));
            }
        }
    } else {
        None
    };

    // One-release IMAP migration: read the pre-account key and keep its stored
    // external_id; a quarantine replay may only repair ownership and retention
    // on the row. The old key was shared across accounts on one host, so
    // the lookup MUST include the credential slug; an old row for account A
    // must not suppress account B's first delivery of the same UID.
    if let Some(ref old_id) = p.legacy_external_id {
        let (Some(slug), Some(new_id)) = (p.channel_slug.as_deref(), p.external_id.as_deref())
        else {
            return Err(RuntimeError::InvalidInput(
                "ingest: `legacy_external_id` requires email channel kind, slug, and new external_id"
                    .into(),
            ));
        };
        if p.channel_kind.as_deref() != Some("email") || old_id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "ingest: `legacy_external_id` requires email channel kind, slug, and new external_id"
                    .into(),
            ));
        }
        // Prefer a row already stored under the account-scoped key. This
        // explicit read also keeps the migration lookup order observable;
        // the unique index below remains the final atomic race guard.
        let new_filter = ingest_external_id_filter(new_id, p.channel_kind.as_deref(), Some(slug));
        let new_page = store
            .query_notes_filtered_count_free(
                ns,
                &new_filter,
                PageRequest {
                    limit: 1,
                    offset: 0,
                },
            )
            .await?;
        if let Some(duplicate) = new_page.items.first() {
            repair_duplicate_quarantine(
                runtime,
                ns,
                duplicate,
                quarantine_attachment.as_ref(),
                p.channel_kind.as_deref(),
                p.channel_slug.as_deref(),
                quarantine_retention,
                true,
            )
            .await?;
            return Ok(duplicate_ingest_ack(duplicate, p.external_id.as_deref()));
        }
        let old_filter = NoteFilter {
            kind: Some("message".to_string()),
            property_filters: vec![
                PropertyFilter {
                    json_path: "$.external_id".to_string(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text(old_id.clone()),
                },
                PropertyFilter {
                    json_path: "$.direction".to_string(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text("inbound".to_string()),
                },
                PropertyFilter {
                    json_path: "$.channel_kind".to_string(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text("email".to_string()),
                },
                PropertyFilter {
                    json_path: "$.channel_slug".to_string(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text(slug.to_string()),
                },
            ],
            ..Default::default()
        };
        let old_page = store
            .query_notes_filtered_count_free(
                ns,
                &old_filter,
                PageRequest {
                    limit: 1,
                    offset: 0,
                },
            )
            .await?;
        if let Some(duplicate) = old_page.items.first() {
            repair_duplicate_quarantine(
                runtime,
                ns,
                duplicate,
                quarantine_attachment.as_ref(),
                p.channel_kind.as_deref(),
                p.channel_slug.as_deref(),
                quarantine_retention,
                false,
            )
            .await?;
            return Ok(duplicate_ingest_ack(duplicate, p.external_id.as_deref()));
        }
    }

    // Thread resolution: resolve correlation_external_id to the original message's
    // thread_id + from_actor. Two-query fallback (Message-ID pass, then thread-UUID
    // pass) — see crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_ingest
    let resolved: Option<(String, String)> = if let Some(ref corr) = p.correlation_external_id {
        if !corr.is_empty() {
            // Pass 1: match by $.external_id (RFC 822 Message-ID, standard In-Reply-To path).
            let mut pass1 = None;
            let email_reply =
                p.channel_kind.as_deref() == Some("email") || p.from.trim().starts_with("email:");
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
                let mut offset = 0;
                loop {
                    let corr_page = store
                        .query_notes_filtered_count_free(
                            ns,
                            &corr_filter,
                            PageRequest { limit: 100, offset },
                        )
                        .await?;
                    let count = corr_page.items.len();
                    if let Some(n) = corr_page.items.iter().find(|n| {
                        if email_reply || outbound_email_message(n) {
                            verified_outbound_email_external_id(n, email_domains)
                        } else {
                            true
                        }
                    }) {
                        // A copied Message-ID can sort first. Only the row that
                        // owns its UUID may supply the thread and actor.
                        let thread_id = n
                            .properties
                            .as_ref()
                            .and_then(|props| props.get("thread_id"))
                            .and_then(Value::as_str)
                            .and_then(|s| s.parse::<Uuid>().ok())
                            .map(|id| id.as_hyphenated().to_string())
                            .unwrap_or_else(|| n.id.as_hyphenated().to_string());
                        let from_actor = n
                            .properties
                            .as_ref()
                            .and_then(|props| props.get("from_actor"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        pass1 = Some((thread_id, from_actor));
                        break;
                    }
                    if count < 100 {
                        break;
                    }
                    offset = offset.checked_add(count as u64).ok_or_else(|| {
                        RuntimeError::Internal(
                            "ingest: correlation pagination offset overflowed".into(),
                        )
                    })?;
                }
                if pass1.is_some() {
                    break;
                }
            }

            if pass1.is_some() {
                pass1
            } else if let Ok(correlation_root) = corr.trim().parse::<Uuid>() {
                // Pass 2: `corr` is a UUID — may be a thread UUID from X-Khive-Thread-ID.
                // Match the canonical spelling written by v1 against $.thread_id on an
                // outbound note to recover from_actor. The selected root stays canonical
                // even when the transport supplied a compact or braced UUID.
                let canonical_correlation_root = correlation_root.as_hyphenated().to_string();
                // No backfill rewrites pre-v1 rows, so probe every spelling older
                // handlers could have stored (canonical, compact, braced, URN, and
                // upper-hex) as well as the canonical v1 form. Whatever spelling
                // matched, the selected root returned below is always canonical.
                let candidates = thread_id_query_spellings(correlation_root, Some(corr.trim()));

                let mut thread_match = None;
                for candidate in candidates {
                    let thread_filter = NoteFilter {
                        kind: Some("message".to_string()),
                        property_filters: vec![
                            PropertyFilter {
                                json_path: "$.thread_id".to_string(),
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
                    let thread_page = store
                        .query_notes_filtered_count_free(
                            ns,
                            &thread_filter,
                            PageRequest {
                                limit: 1,
                                offset: 0,
                            },
                        )
                        .await?;
                    if let Some(note) = thread_page.items.first() {
                        let from_actor = note
                            .properties
                            .as_ref()
                            .and_then(|props| props.get("from_actor"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        thread_match = Some((canonical_correlation_root.clone(), from_actor));
                        break;
                    }
                }
                thread_match
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
    // Both supplied and correlation-derived roots have already been normalized
    // to the v1 full-hyphenated representation above.
    let thread_id: String = supplied_thread_id
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

    let mut props = json!({
        "comm_schema_version": COMM_SCHEMA_VERSION,
        "from": p.from.trim(),
        "to": p.to.trim(),
        "from_actor": p.from.trim(),
        "to_actor": to_actor,
        "direction": "inbound",
        "read": false,
        "thread_id": thread_id,
        "sent_at": sent_at,
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
    if let Some(ref slug) = p.channel_slug {
        props["channel_slug"] = json!(slug);
    }
    // Metadata passthrough (#448): merged additively so it never clobbers a
    // field set above. Stable v1 names are reserved even when their optional
    // field is absent on an ingest (`subject`, `outbound_ref`,
    // `sent_by_process`), preventing adapter metadata from fabricating a
    // contract field or process provenance.
    if let Some(metadata) = p.metadata {
        if let Some(obj) = props.as_object_mut() {
            for (k, v) in metadata {
                if COMM_STABLE_PROPERTY_KEYS.contains(&k.as_str())
                    || matches!(k.as_str(), "channel_kind" | "channel_slug")
                {
                    continue;
                }
                obj.entry(k).or_insert(v);
            }
        }
    }

    let created = if let Some(attachment) = quarantine_attachment.clone() {
        runtime
            .try_create_note_as_trusted_ingest_with_attachment(
                capability,
                token,
                "message",
                p.subject.as_deref(),
                p.content.trim(),
                Some(props),
                attachment,
                is_quarantined.then_some(quarantine_retention),
            )
            .await
    } else {
        runtime
            .try_create_note_as_trusted_ingest(
                capability,
                token,
                "message",
                p.subject.as_deref(),
                p.content.trim(),
                Some(props),
                is_quarantined.then_some(quarantine_retention),
            )
            .await
    };
    let (note_id, degradations) = match created {
        Ok(Some(note)) => (note.id, None),
        Err(error) => match committed_ingest_degradations(&error) {
            Some((id, report)) => (id, Some(report)),
            None => return Err(error),
        },
        Ok(None) => {
            tracing::debug!(
                external_id = ?p.external_id,
                "comm.ingest: duplicate message skipped"
            );
            let external_id = p.external_id.as_deref().ok_or_else(|| {
                RuntimeError::Internal(
                    "comm.ingest: storage reported a duplicate without an external_id".into(),
                )
            })?;
            let duplicate_filter = ingest_external_id_filter(
                external_id,
                p.channel_kind.as_deref(),
                p.channel_slug.as_deref(),
            );
            let duplicate_page = store
                .query_notes_filtered_count_free(
                    ns,
                    &duplicate_filter,
                    PageRequest {
                        limit: 1,
                        offset: 0,
                    },
                )
                .await?;
            let duplicate = duplicate_page.items.first().ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "comm.ingest: duplicate external_id {external_id:?} has no existing row"
                ))
            })?;
            repair_duplicate_quarantine(
                runtime,
                ns,
                duplicate,
                quarantine_attachment.as_ref(),
                p.channel_kind.as_deref(),
                p.channel_slug.as_deref(),
                quarantine_retention,
                true,
            )
            .await?;
            return Ok(duplicate_ingest_ack(duplicate, p.external_id.as_deref()));
        }
    };
    inbox_signal.publish();

    let mut response = json!({
        "id": short_id(note_id),
        "full_id": note_id.as_hyphenated().to_string(),
        "thread_id": thread_id,
        "external_id": p.external_id,
        "deduplicated": false,
    });
    if let Some(report) = degradations {
        response["post_commit_degradations"] = report;
    }
    Ok(response)
}

fn committed_ingest_degradations(error: &RuntimeError) -> Option<(Uuid, Value)> {
    let RuntimeError::Khive(domain) = error.refusal_source() else {
        return None;
    };
    if domain.kind() != khive_types::ErrorKind::Internal {
        return None;
    }
    let details = domain.details()?;
    if details.get("reason") != Some("post_commit_degraded")
        || details.get("operation") != Some("try_create_note")
        || details.get("committed") != Some("true")
        || details.get("retryable") != Some("false")
    {
        return None;
    }
    let raw_id = details.get("record_id")?;
    let id = Uuid::parse_str(raw_id).ok()?;
    if id.as_hyphenated().to_string() != raw_id {
        return None;
    }
    let report: Value = serde_json::from_str(details.get("post_commit_degradations")?).ok()?;
    let failures = report.as_array()?;
    if failures.is_empty()
        || !failures.iter().all(|failure| {
            failure.as_object().is_some_and(|entry| {
                entry.len() == 2
                    && entry
                        .get("stage")
                        .and_then(Value::as_str)
                        .is_some_and(|stage| {
                            khive_runtime::ConditionalInsertStage::from_label(stage).is_some()
                        })
                    && entry
                        .get("error")
                        .and_then(Value::as_str)
                        .is_some_and(|error| !error.is_empty())
            })
        })
    {
        return None;
    }
    Some((id, report))
}

/// Detach only the main-backend original owned by this already hard-deleted
/// legacy note. The conditional DELETE rejects a competing replacement of the
/// role after the owner read; it never scans for unrelated ownerless rows.
async fn detach_deleted_legacy_original(
    runtime: &KhiveRuntime,
    id: Uuid,
    expected_ref: Option<&str>,
) -> Result<bool, RuntimeError> {
    let core = runtime.core();
    let attachments = core.attachments()?;
    let Some(owner) = attachments
        .get_attachment(id, "quarantine-original")
        .await?
    else {
        return Ok(false);
    };
    let expected = expected_ref.ok_or_else(|| {
        RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} has an original owner but no original ref"
        ))
    })?;
    let expected = ContentRef::from_hex(expected.to_string()).map_err(|error| {
        RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} has an invalid original ref: {error}"
        ))
    })?;
    if owner.substrate != AttachmentSubstrate::Note || owner.content_ref != expected {
        return Err(RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} has a mismatched original owner"
        )));
    }
    let detached = core
        .sql()
        .writer()
        .await?
        .execute(SqlStatement {
            sql: khive_runtime::sql!("quarantine_original_detach").into(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(expected.as_str().to_string()),
            ],
            label: Some("comm_cleanup_legacy_quarantine_original".into()),
        })
        .await?;
    if detached != 1 {
        return Err(RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} original owner changed during cleanup"
        )));
    }
    Ok(true)
}

fn delete_note_error_committed(error: &RuntimeError, id: Uuid) -> bool {
    let RuntimeError::Khive(domain) = error.refusal_source() else {
        return false;
    };
    let Some(details) = domain.details() else {
        return false;
    };
    details.get("reason") == Some("post_commit_degraded")
        && details.get("operation") == Some("delete_note")
        && details
            .get("record_id")
            .is_some_and(|stored| Uuid::parse_str(stored).ok() == Some(id))
        && details.get("committed") == Some("true")
}

async fn note_deleted_after_attempt<F>(
    delete_result: &Result<bool, RuntimeError>,
    id: Uuid,
    probe_absent: F,
) -> Result<bool, RuntimeError>
where
    F: Future<Output = Result<bool, RuntimeError>>,
{
    match delete_result {
        Ok(deleted) => Ok(*deleted),
        Err(error) if delete_note_error_committed(error, id) => Ok(true),
        Err(_) => probe_absent.await,
    }
}

/// Internal channel-poller maintenance. One bounded page per tick ensures an
/// empty poll still makes progress without monopolizing the writer. The token
/// carries the ingest namespace explicitly; heartbeat rows use a different
/// namespace and must not be used as the maintenance scope.
pub(crate) async fn handle_cleanup_expired_quarantine(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
    quarantine_retention: std::time::Duration,
) -> Result<Value, RuntimeError> {
    let p: CleanupExpiredQuarantineParams = deser(params)?;
    if p.channel_kind.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cleanup_expired_quarantine: channel_kind must be nonblank".into(),
        ));
    }
    match p.mode {
        QuarantineCleanupMode::Channel if p.channel_slug.trim().is_empty() => {
            return Err(RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: channel_kind and channel_slug must be nonblank".into(),
            ));
        }
        QuarantineCleanupMode::LegacySlugless if !p.channel_slug.is_empty() => {
            return Err(RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: legacy_slugless requires an empty channel_slug".into(),
            ));
        }
        _ => {}
    }
    let as_of = p
        .as_of_micros
        .unwrap_or_else(|| Utc::now().timestamp_micros());
    let legacy_cutoff = if p.mode == QuarantineCleanupMode::LegacySlugless {
        let retention_us = i64::try_from(quarantine_retention.as_micros()).map_err(|_| {
            RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: retention exceeds i64 microseconds".into(),
            )
        })?;
        Some(as_of.checked_sub(retention_us).ok_or_else(|| {
            RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: retention cutoff underflow".into(),
            )
        })?)
    } else {
        None
    };
    let namespace = token.namespace().as_str();
    let (sql, sql_params) = if let Some(cutoff) = legacy_cutoff {
        // Match the #3497 boot-repair selector: missing, JSON null, or a
        // SQLite-space-only text slug. Existing tombstones stay eligible,
        // including an operator-soft-deleted historical quarantine.
        (
            khive_runtime::sql!("quarantine_legacy_expired_select"),
            vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(as_of),
                SqlValue::Integer(cutoff),
                SqlValue::Text(p.channel_kind.clone()),
            ],
        )
    } else {
        (
            khive_runtime::sql!("quarantine_expired_select"),
            vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(as_of),
                SqlValue::Text(p.channel_kind.clone()),
                SqlValue::Text(p.channel_slug.clone()),
            ],
        )
    };
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql.into(),
            params: sql_params,
            label: Some("comm_cleanup_expired_quarantine".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    drop(reader);

    // Hard deletion by ID is not namespace-scoped. Re-read each UUID through
    // the authorized store and enforce the query's full predicate.
    let store = runtime.notes(token)?;
    let mut deleted = 0usize;
    let mut routed_owner_detached = 0usize;
    for row in rows {
        let id = match row.get("id") {
            Some(SqlValue::Text(id)) => Uuid::parse_str(id).map_err(|error| {
                RuntimeError::Internal(format!(
                    "cleanup_expired_quarantine: invalid stored note id: {error}"
                ))
            })?,
            _ => {
                return Err(RuntimeError::Internal(
                    "cleanup_expired_quarantine: query returned no text id".into(),
                ));
            }
        };
        let note = if legacy_cutoff.is_some() {
            store.get_note_including_deleted(id).await?
        } else {
            store.get_note(id).await?
        };
        let Some(note) = note else {
            continue;
        };
        let properties = note.properties.as_ref();
        let slug = properties.and_then(|props| props.get("channel_slug"));
        let slug_matches = match p.mode {
            QuarantineCleanupMode::Channel => {
                slug.and_then(Value::as_str) == Some(p.channel_slug.as_str())
            }
            QuarantineCleanupMode::LegacySlugless => match slug {
                None | Some(Value::Null) => true,
                Some(Value::String(value)) => value.trim().is_empty(),
                _ => false,
            },
        };
        let still_due = note.namespace == namespace
            && note.kind == "message"
            && match legacy_cutoff {
                Some(cutoff) => note
                    .expires_at
                    .map_or(note.created_at <= cutoff, |expires| expires <= as_of),
                None => {
                    note.deleted_at.is_none()
                        && note.expires_at.is_some_and(|expires| expires <= as_of)
                }
            }
            && properties
                .and_then(|props| props.get("channel_kind"))
                .and_then(Value::as_str)
                == Some(p.channel_kind.as_str())
            && slug_matches
            && (properties.and_then(|props| props.get("quarantined")) == Some(&Value::Bool(true))
                || properties
                    .and_then(|props| props.get("quarantined"))
                    .and_then(Value::as_str)
                    == Some("true"));
        if !still_due {
            continue;
        }
        let routed_legacy = p.mode == QuarantineCleanupMode::LegacySlugless
            && runtime.backend_id() != runtime.core().backend_id();
        let expected_ref = properties
            .and_then(|props| props.get("quarantine_content_ref"))
            .and_then(Value::as_str);
        // The runtime hard-delete removes incident graph edges alongside the
        // note and its local attachments in one transaction. A routed note's
        // repaired original lives on canonical main and is detached by exact
        // ID/ref only after that transaction commits.
        let delete_result = runtime.delete_note(token, id, true).await;
        // A typed post-commit error already proves the row/edge transaction
        // committed. A second read can fail and must not block detaching the
        // routed original that the now-absent note can never select again.
        let note_deleted = note_deleted_after_attempt(&delete_result, id, async {
            Ok(store.get_note_including_deleted(id).await?.is_none())
        })
        .await?;
        if note_deleted {
            deleted += 1;
            if routed_legacy {
                for attempt in 1..=3 {
                    match detach_deleted_legacy_original(runtime, id, expected_ref).await {
                        Ok(detached) => {
                            routed_owner_detached += usize::from(detached);
                            break;
                        }
                        Err(error) if attempt < 3 => {
                            tracing::warn!(
                                note_id = %id,
                                attempt,
                                error = %error,
                                "targeted legacy quarantine owner detach will retry"
                            );
                            tokio::task::yield_now().await;
                        }
                        Err(error) => {
                            // There is no note left to select on a later tick.
                            // The possible residue belongs to the operator's
                            // blob ownerless-row inspection path (#3178).
                            return Err(RuntimeError::Internal(format!(
                                "cleanup_expired_quarantine: deleted_notes={deleted}, \
                                 routed_owners_detached={routed_owner_detached}, \
                                 possible_owner_residue=1 for note {id} after 3 attempts: {error}"
                            )));
                        }
                    }
                }
            }
        }
        delete_result?;
    }
    Ok(json!({
        "ok": true,
        "deleted": deleted,
        "routed_owners_detached": routed_owner_detached,
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
/// heartbeat row (khive #606). Internal subhandler with no MCP wire path: its
/// production local caller is the daemon's channel poll loop, and khive #917
/// also lets authorized per-tenant writers reach it via `dispatch_as`.
/// Read-modify-write: `created_at` is preserved across updates, `last_error`
/// is RETAINED across a subsequent success (design review amendment 3), and
/// `consecutive_failures` resets on success / increments on failure, read from
/// the prior row (correct across restarts).
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
    if p.poll_interval_secs == Some(0) {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `poll_interval_secs` must be greater than zero".into(),
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
    #[cfg(test)]
    race_seam::pause_after_read().await;

    let now = Utc::now();
    // A supplied `at` must resolve to an instant before it is stored: the
    // staleness reader parses `last_poll_attempt_at` inside an Option chain,
    // so an unparseable stored value makes staleness silently unknown and the
    // channel can never read as stale — failing toward looking healthy. Same
    // rule as ingest's `sent_at` (`canonicalize_ingest_sent_at`).
    let at = match p.at.as_deref() {
        Some(raw) => {
            parse_supplied_timestamp("heartbeat", "at", raw)?;
            raw.trim().to_string()
        }
        None => now.to_rfc3339(),
    };

    // Preserve the current row's other properties, then validate the final
    // object: a legacy row may already carry a runtime-owned key.
    let mut props = existing
        .as_ref()
        .and_then(|n| n.properties.clone())
        .unwrap_or_else(|| json!({}));

    props["channel_kind"] = json!(p.channel_kind);
    props["channel_slug"] = json!(p.channel_slug);
    props["last_poll_attempt_at"] = json!(at);
    if let Some(poll_interval_secs) = p.poll_interval_secs {
        props["poll_interval_secs"] = json!(poll_interval_secs);
    }

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

    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&props))?;
    khive_runtime::secret_gate::check_json_at(&props, "channel", "properties")?;

    let content = format!("channel heartbeat: {}:{}", p.channel_kind, p.channel_slug);
    khive_runtime::secret_gate::check_at(&content, "channel", "content")?;

    let created_at = existing
        .as_ref()
        .map(|n| n.created_at)
        .unwrap_or_else(|| now.timestamp_micros());

    // `updated_at` is also the optimistic-concurrency revision `replace_note_if_unchanged`
    // checks with `?10 > updated_at`. Two heartbeats landing in the same stored microsecond,
    // or a backward wall-clock step, must not report a conflict when no concurrent writer
    // touched the row — make the replacement revision strictly advance past the existing
    // snapshot instead of trusting `Utc::now()` alone (mirrors
    // `update_note_from_snapshot_with_embedding_report`). A brand-new channel has no prior
    // revision to advance past.
    let updated_at_micros = match existing.as_ref() {
        Some(snapshot) => {
            let minimum = snapshot.updated_at.checked_add(1).ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "heartbeat: channel {}:{} updated_at is already at i64::MAX and cannot advance",
                    p.channel_kind, p.channel_slug
                ))
            })?;
            now.timestamp_micros().max(minimum)
        }
        None => now.timestamp_micros(),
    };

    let note = Note {
        version: 1,
        key: None,
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
        updated_at: updated_at_micros,
        deleted_at: None,
    };

    // Guard the read-modify-write: `props` above was carried forward from
    // `existing`, so a concurrent heartbeat that committed after that read
    // (e.g. flipping `consecutive_failures`/`last_error`) must not be
    // silently discarded by this write — the same shape
    // `replace_note_if_unchanged` uses for every other guarded note update
    // (khive-runtime's `update_note_from_snapshot_with_embedding_report`).
    //
    // The `None` branch needs its own guard, and being a new row is not what
    // makes it safe. The hazard there is not losing prior state, it is two
    // concurrent FIRST writes: the note id is deterministic per channel, so two
    // heartbeats racing on a channel's first report can both read `None` and
    // both take that branch. `upsert_note` rewrites every mutable column on an
    // id conflict and reports no conflict outcome, so the later write would
    // silently replace the earlier report. `insert_note_if_absent` leaves an
    // existing row untouched and reports whether this call inserted it, so the
    // loser is told it lost — the same conflict the `Some` arm returns, reached
    // from the other direction.
    match existing {
        Some(snapshot) => {
            let persisted = store
                .replace_note_if_unchanged(note, snapshot.updated_at, snapshot.deleted_at)
                .await
                .map_err(|e| {
                    RuntimeError::Internal(format!("heartbeat: replace_note_if_unchanged: {e}"))
                })?;
            if !persisted {
                return Err(RuntimeError::Khive(khive_types::KhiveError::conflict(
                    format!(
                        "heartbeat: channel {}:{} changed concurrently after it was read; retry",
                        p.channel_kind, p.channel_slug
                    ),
                )));
            }
        }
        None => {
            let inserted = store.insert_note_if_absent(note).await.map_err(|e| {
                RuntimeError::Internal(format!("heartbeat: insert_note_if_absent: {e}"))
            })?;
            if !inserted {
                return Err(RuntimeError::Khive(khive_types::KhiveError::conflict(
                    format!(
                        "heartbeat: channel {}:{} was first reported concurrently; retry",
                        p.channel_kind, p.channel_slug
                    ),
                )));
            }
        }
    }

    Ok(json!({
        "ok": true,
        "channel_kind": p.channel_kind,
        "channel_slug": p.channel_slug,
        "outcome": outcome,
    }))
}

/// A channel is schedule-stale after three complete nominal poll intervals.
/// The grace avoids flagging a live poller during ordinary tick and I/O jitter.
const STALLED_AFTER_INTERVALS: u64 = 3;

fn channel_stalled(props: &Value, as_of: &DateTime<Utc>) -> Option<bool> {
    // A known failure enters intentional exponential backoff, so the nominal
    // cadence cannot distinguish an overdue poll from a scheduled retry.
    let consecutive_failures = props.get("consecutive_failures").and_then(Value::as_u64)?;
    if consecutive_failures > 0 {
        return None;
    }
    let poll_interval_secs = props.get("poll_interval_secs")?.as_u64()?;
    if poll_interval_secs == 0 {
        return None;
    }
    let stall_after_millis = poll_interval_secs
        .checked_mul(STALLED_AFTER_INTERVALS)?
        .checked_mul(1_000)?;
    let last_poll_attempt =
        DateTime::parse_from_rfc3339(props.get("last_poll_attempt_at")?.as_str()?)
            .ok()?
            .with_timezone(&Utc);
    let elapsed_millis = u64::try_from(
        as_of
            .signed_duration_since(last_poll_attempt)
            .num_milliseconds(),
    )
    .ok()?;
    Some(elapsed_millis > stall_after_millis)
}

/// Project a persisted `channel_health` note into the `comm.health()` channel
/// entry shape. Missing or malformed cadence/timestamp fields (including rows
/// written before #1472) produce `stalled: null` rather than pretending the
/// channel is current.
fn channel_health_to_json(note: &Note, as_of: &DateTime<Utc>, quarantined_count: u64) -> Value {
    let props = note.properties.clone().unwrap_or_else(|| json!({}));
    let poll_interval_secs = props
        .get("poll_interval_secs")
        .and_then(Value::as_u64)
        .filter(|interval| *interval > 0);
    let stalled = channel_stalled(&props, as_of);
    json!({
        "channel_kind": props.get("channel_kind").cloned().unwrap_or(Value::Null),
        "channel_slug": props.get("channel_slug").cloned().unwrap_or(Value::Null),
        "poll_interval_secs": poll_interval_secs,
        "stalled": stalled,
        "last_success_at": props.get("last_success_at").cloned().unwrap_or(Value::Null),
        "last_poll_attempt_at": props.get("last_poll_attempt_at").cloned().unwrap_or(Value::Null),
        "last_failure_at": props.get("last_failure_at").cloned().unwrap_or(Value::Null),
        "last_error": props.get("last_error").cloned().unwrap_or(Value::Null),
        "consecutive_failures": props.get("consecutive_failures").cloned().unwrap_or(json!(0)),
        "quarantined_count": quarantined_count,
    })
}

#[derive(Default)]
struct QuarantineCounts {
    total: u64,
    unattributed: u64,
    by_channel: HashMap<(String, String), u64>,
}

/// Count live quarantine dispositions in one authorized namespace. The
/// generic marker accepts both the string spelling emitted by today's channel
/// envelope metadata and a JSON boolean so future adapters do not need an
/// email-specific encoding. Rows lacking a complete transport identity remain
/// visible in `unattributed` and are never guessed onto a heartbeat.
async fn load_quarantine_counts(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<QuarantineCounts, RuntimeError> {
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("quarantine_counts_select").into(),
            params: vec![SqlValue::Text(token.namespace().as_str().to_string())],
            label: Some("comm_health_quarantined_counts".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    let mut counts = QuarantineCounts::default();
    for row in rows {
        let count = match row.get("quarantined_count") {
            Some(SqlValue::Integer(count)) if *count >= 0 => *count as u64,
            other => {
                return Err(RuntimeError::Internal(format!(
                    "comm.health: storage returned an invalid quarantine count: {other:?}"
                )))
            }
        };
        counts.total = counts.total.checked_add(count).ok_or_else(|| {
            RuntimeError::Internal("comm.health: quarantine count overflow".into())
        })?;

        let kind = row.get("channel_kind").and_then(|value| match value {
            SqlValue::Text(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            _ => None,
        });
        let slug = row.get("channel_slug").and_then(|value| match value {
            SqlValue::Text(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            _ => None,
        });
        if let (Some(kind), Some(slug)) = (kind, slug) {
            let channel_count = counts.by_channel.entry((kind, slug)).or_default();
            *channel_count = channel_count.checked_add(count).ok_or_else(|| {
                RuntimeError::Internal("comm.health: per-channel quarantine count overflow".into())
            })?;
        } else {
            counts.unattributed = counts.unattributed.checked_add(count).ok_or_else(|| {
                RuntimeError::Internal("comm.health: unattributed quarantine count overflow".into())
            })?;
        }
    }

    Ok(counts)
}

fn quarantine_only_channel_to_json(
    channel_kind: String,
    channel_slug: String,
    quarantined_count: u64,
) -> Value {
    json!({
        "channel_kind": channel_kind,
        "channel_slug": channel_slug,
        "poll_interval_secs": Value::Null,
        "stalled": Value::Null,
        "last_success_at": Value::Null,
        "last_poll_attempt_at": Value::Null,
        "last_failure_at": Value::Null,
        "last_error": Value::Null,
        "consecutive_failures": Value::Null,
        "quarantined_count": quarantined_count,
    })
}

/// `health` — read-only per-channel health snapshot (khive #606). Reads
/// `channel_health` rows from `token.namespace()` (khive #877 namespace
/// scoping), then unions exact channel identities found on live quarantine
/// notes in that same namespace (khive #1383). The additive `stalled`
/// schedule fact is deliberately narrower than a computed `healthy: bool`;
/// quarantine counts are an independent backlog observation, and overall
/// health judgment still belongs to the caller. See
/// crates/khive-pack-comm/docs/api/channel-health.md#handlersrshandle_health
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
    const MAX_CHANNELS: usize = 200;
    let filter = NoteFilter {
        kind: Some("channel_health".to_string()),
        ..Default::default()
    };
    let page = store
        .query_notes_filtered_count_free(
            token.namespace().as_str(),
            &filter,
            PageRequest {
                limit: MAX_CHANNELS as u32,
                offset: 0,
            },
        )
        .await?;

    if page.items.len() == MAX_CHANNELS {
        tracing::debug!(
            max_channels = MAX_CHANNELS,
            "comm.health: channel_health row count hit the page limit; \
             heartbeat rows take the full response budget and later rows are omitted"
        );
    }

    let now = Utc::now();
    let mut quarantine_counts = load_quarantine_counts(runtime, token).await?;
    let has_heartbeat_state = !page.items.is_empty();
    let mut channels: Vec<Value> = page
        .items
        .iter()
        .map(|note| {
            let key = note.properties.as_ref().and_then(|props| {
                Some((
                    props.get("channel_kind")?.as_str()?.trim().to_string(),
                    props.get("channel_slug")?.as_str()?.trim().to_string(),
                ))
            });
            let quarantined_count = key
                .and_then(|key| quarantine_counts.by_channel.remove(&key))
                .unwrap_or(0);
            channel_health_to_json(note, &now, quarantined_count)
        })
        .collect();

    // A message namespace can intentionally differ from the operational
    // heartbeat namespace. Preserve those exact channel identities as
    // quarantine-only entries instead of hiding them merely because this
    // scoped read has no heartbeat row. Heartbeats consume the bounded response
    // budget first: if their page is full, an identity whose heartbeat exists
    // beyond that page is omitted rather than misclassified as quarantine-only.
    // When heartbeat rows leave capacity, exact quarantine-only identities fill
    // it in deterministic lexical order without changing heartbeat order.
    let mut quarantine_only: Vec<_> = quarantine_counts.by_channel.into_iter().collect();
    quarantine_only.sort_by(|(left, _), (right, _)| left.cmp(right));
    let quarantine_capacity = MAX_CHANNELS.saturating_sub(channels.len());
    let omitted_quarantine_channels = quarantine_only.len().saturating_sub(quarantine_capacity);
    channels.extend(quarantine_only.into_iter().take(quarantine_capacity).map(
        |((channel_kind, channel_slug), count)| {
            quarantine_only_channel_to_json(channel_kind, channel_slug, count)
        },
    ));
    if omitted_quarantine_channels > 0 {
        tracing::debug!(
            max_channels = MAX_CHANNELS,
            omitted_quarantine_channels,
            "comm.health: quarantine-only channel identities exceeded the capacity left after \
             heartbeat rows; later lexical identities are omitted"
        );
    }
    let as_of = now.to_rfc3339();

    let (role, source) = if !has_heartbeat_state {
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
        "quarantined_count": quarantine_counts.total,
        "unattributed_quarantined_count": quarantine_counts.unattributed,
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

/// The page and bounded stale count share one SQL snapshot (ADR-D5). The
/// stale-count index excludes read history and seeks directly below the cutoff.
/// The shared bounded NoteStore counter cannot express that strict upper bound
/// or join this statement's snapshot, so this count stays inside the probe.
/// `cursor_us`/`since_us` are keyed on `notes_seq.seq`, NOT `created_at` or
/// SQLite `rowid` — both can regress/collide across concurrent writers, VACUUM,
/// or hard-delete. Do not revert to either. See
/// crates/khive-pack-comm/docs/api/probe-cursor.md#handlersrsprobe_sql for the full
/// #780/#827 incident history.
#[doc(hidden)]
pub const PROBE_SQL: &str = khive_runtime::sql!("probe_messages_select");

/// `probe` — strictly read-only poll for new inbound message metadata and a
/// stale-unread count capped at 1000 (ADR-D5). No read-flag mutation, no writes:
/// the earliest unseen sequence page and stale count share one indexed statement.
pub(crate) async fn handle_probe(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ProbeParams = deser(params.clone())?;
    validate_actor_label("probe", &p.actor, "actor")?;
    runtime.authorize_mailbox_view(token, "comm.probe", Some(&p.actor), &params)?;
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
            sql: khive_runtime::sql!("notes_seq_high_water_select").into(),
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
            // #2400: this reset also travels back to the caller as
            // `cursor_reset`. A log line the poller cannot read leaves it
            // deduplicating a baseline page against its own inbox, which is the
            // cost this warning was silently imposing.
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

    let mut cursor_us = effective_since.unwrap_or(0);
    let mut stale_unread_count = 0i64;
    let mut new_messages = Vec::new();

    for row in &rows {
        if let Some(SqlValue::Integer(v)) = row.get("stale_unread_count") {
            stale_unread_count = *v;
        }

        let message_cursor = match row.get("cursor_us") {
            Some(SqlValue::Integer(v)) => *v,
            _ => continue,
        };
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
        // The displayed page is timestamp-ordered, not sequence-ordered. Only
        // emitted rows advance the cursor; unseen later pages must remain visible.
        cursor_us = cursor_us.max(message_cursor);
    }

    // #827: never let the returned cursor regress below what the caller already
    // holds, including an empty page after a high-sequence row was hard-deleted.
    if let Some(floor) = effective_since {
        if cursor_us < floor {
            cursor_us = floor;
        }
    }

    Ok(ProbeResponse {
        cursor_us,
        new_messages,
        stale_unread_count,
        cursor_reset: since_us.is_some() && effective_since.is_none(),
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
            sql: khive_runtime::sql!("channel_cursor_select").into(),
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
        sql: khive_runtime::sql!("channel_cursor_upsert").into(),
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

fn outbound_email_message(note: &Note) -> bool {
    let props = note.properties.as_ref();
    props
        .and_then(|p| p.get("direction"))
        .and_then(Value::as_str)
        == Some("outbound")
        && (props
            .and_then(|p| p.get("channel_kind"))
            .and_then(Value::as_str)
            == Some("email")
            || props
                .and_then(|p| p.get("channel_slug"))
                .and_then(Value::as_str)
                .is_some_and(|slug| slug.contains('@'))
            || ["to_actor", "to", "from_actor", "from"].iter().any(|key| {
                props
                    .and_then(|p| p.get(*key))
                    .and_then(Value::as_str)
                    .is_some_and(|actor| actor.starts_with("email:"))
            }))
}

fn verified_outbound_email_external_id(
    note: &Note,
    domains: &Result<Option<EmailMessageIdDomains>, String>,
) -> bool {
    let Some(domains) = domains.as_ref().ok().and_then(Option::as_ref) else {
        return false;
    };
    let props = note.properties.as_ref();
    domains.verifies_channel_slug(
        props
            .and_then(|p| p.get("channel_slug"))
            .and_then(Value::as_str),
    ) && props
        .and_then(|p| p.get("external_id"))
        .and_then(Value::as_str)
        .is_some_and(|external_id| domains.verify(note.id, external_id))
}

fn external_id_unverifiable(note_id: Uuid, reason: &str) -> RuntimeError {
    khive_types::KhiveError::invalid_input(format!(
        "external_id_unverifiable: outbound message {note_id}: {reason}"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "external_id_unverifiable".to_string()),
        ("note_id", note_id.to_string()),
    ]))
    .into()
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
