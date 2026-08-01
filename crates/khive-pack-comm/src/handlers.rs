//! Verb handler implementations for the comm pack.
//!
//! Public comm verbs store and query `message` notes in the standard notes
//! table. Message-specific metadata lives in the `properties` JSON column;
//! `content` is the message body.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

use crate::message::{
    dual_write_message, note_to_message_json, project_message_json, resolve_id, short_id,
    validate_message_projection_fields, COMM_SCHEMA_VERSION, COMM_STABLE_PROPERTY_KEYS,
};
use crate::params::{
    deser, CursorCommitParams, CursorGetParams, DeliveredParams, HeartbeatParams, InboxParams,
    IngestParams, ProbeParams, ReadParams, ReplyParams, SendParams, ThreadParams, UnreadParams,
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

fn parse_inbox_timestamp(field: &str, raw: &str) -> Result<i64, RuntimeError> {
    DateTime::parse_from_rfc3339(raw.trim())
        .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
        .map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "inbox: `{field}` must be a valid RFC 3339 timestamp, got {raw:?}: {e}"
            ))
        })
}

/// Parse a caller- or transport-supplied thread root and return the one wire
/// spelling accepted by message-properties v1. `Uuid` deliberately accepts
/// compact, braced, URN, and upper-hex input forms; normalizing here keeps
/// those convenient inputs from leaking into the stored contract or splitting
/// SQL thread lookups, which compare the JSON string exactly.
fn canonicalize_thread_id(verb: &str, raw: &str) -> Result<String, RuntimeError> {
    raw.trim()
        .parse::<Uuid>()
        .map(|id| id.as_hyphenated().to_string())
        .map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "{verb}: `thread_id` must be a valid UUID, got: {raw:?}"
            ))
        })
}

fn validate_inbox_substring(field: &str, value: Option<&str>) -> Result<(), RuntimeError> {
    if value.is_some_and(|raw| raw.trim().is_empty()) {
        return Err(RuntimeError::InvalidInput(format!(
            "inbox: `{field}` must not be empty"
        )));
    }
    Ok(())
}

fn inbox_note_matches(
    note: &Note,
    params: &InboxParams,
    before_micros: Option<i64>,
    subject_needle: Option<&str>,
    content_needle: Option<&str>,
) -> bool {
    let props = note.properties.as_ref();
    let sender = props
        .and_then(|properties| properties.get("from_actor"))
        .and_then(Value::as_str);

    if params
        .from_prefix
        .as_deref()
        .is_some_and(|prefix| !sender.is_some_and(|value| value.starts_with(prefix)))
    {
        return false;
    }
    if params
        .exclude_from_actor
        .as_deref()
        .is_some_and(|excluded| sender == Some(excluded))
    {
        return false;
    }
    if before_micros.is_some_and(|before| note.created_at >= before) {
        return false;
    }
    if subject_needle.is_some_and(|needle| {
        !props
            .and_then(|properties| properties.get("subject"))
            .and_then(Value::as_str)
            .is_some_and(|subject| subject.to_lowercase().contains(needle))
    }) {
        return false;
    }
    if content_needle.is_some_and(|needle| !note.content.to_lowercase().contains(needle)) {
        return false;
    }

    true
}

/// Return the exact, indexable spellings a pre-v1 handler could have stored
/// for one UUID root. Before v1, valid caller input was persisted verbatim
/// after `Uuid` parsing, so compact, braced, URN, and upper-hex formatter
/// outputs may coexist with the canonical lower-case hyphenated value.
///
/// `selected_raw` retains an arbitrary mixed-case spelling from the row the
/// caller selected. The common lower/upper formatter outputs cover rows other
/// than that selected row without falling back to a namespace-wide scan.
fn thread_id_query_spellings(root: Uuid, selected_raw: Option<&str>) -> Vec<String> {
    let mut spellings = vec![
        root.as_hyphenated().to_string(),
        root.simple().to_string(),
        root.braced().to_string(),
        root.urn().to_string(),
        format!("{:X}", root.as_hyphenated()),
        format!("{:X}", root.simple()),
        format!("{:X}", root.braced()),
        format!("{:X}", root.urn()),
    ];
    if let Some(raw) = selected_raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        spellings.push(raw.to_string());
    }

    let mut seen = HashSet::new();
    spellings.retain(|spelling| seen.insert(spelling.clone()));
    spellings
}

/// Validate an adapter timestamp before it can be certified as a v1 `sent_at`
/// value, then serialize the instant in one RFC 3339 representation (UTC).
fn canonicalize_ingest_sent_at(raw: &str) -> Result<String, RuntimeError> {
    DateTime::parse_from_rfc3339(raw.trim())
        .map(|timestamp| timestamp.with_timezone(&Utc).to_rfc3339())
        .map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "ingest: `sent_at` must be a valid RFC 3339 timestamp, got {raw:?}: {error}"
            ))
        })
}

/// `send` — create a message note in the caller's namespace (outbound) AND
/// deliver an inbound copy addressed to the actor label in `to` (ADR-057).
/// Both copies land in the caller's namespace; no cross-namespace write occurs.
///
/// Known gap (external desk review, 2026-07-21): there is no idempotency
/// guard here, so a retrying caller that repeats an identical `send` (same
/// `to`/`content`) produces a fresh duplicate outbound+inbound pair every
/// call. `comm.ingest`'s `external_id` dedup key is a different mechanism
/// (transport-level dedup for channel-delivered inbound mail) and does not
/// apply to caller-composed sends. Fixing this needs a caller-supplied
/// idempotency key param on `SendParams` (additive) — a content-hash dedup
/// invented here would risk collapsing legitimate repeated messages, so this
/// is left as a design decision rather than implemented speculatively.
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
    let thread_id = p
        .thread_id
        .as_deref()
        .map(|raw| canonicalize_thread_id("send", raw))
        .transpose()?;

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
    let (outbound_note, embedding_truncation) = dual_write_message(
        runtime,
        token,
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
    )
    .await?;

    let mut response = json!({
        "id": short_id(outbound_note.id),
        "full_id": outbound_note.id.as_hyphenated().to_string(),
        "from": from_actor,
        "to": p.to,
        "subject": p.subject,
        "sent_at": sent_at,
    });
    add_embedding_truncation_warning(&mut response, &embedding_truncation);
    Ok(response)
}

/// `delivered` — confirm that the inbound half of an internal dual-write
/// exists, using the outbound UUID copied into `properties.outbound_ref`.
///
/// The outbound row is deliberately not resolved or fetched first. An
/// ambiguous atomic-write outcome may have committed both copies or neither,
/// and legacy/injected half-pairs can lack the outbound row. A full UUID is
/// therefore required instead of a display prefix.
/// This result says nothing about a later external transport attempt (SMTP,
/// Telegram, and so on); it only confirms the comm pack's inbound sibling.
pub(crate) async fn handle_delivered(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: DeliveredParams = deser(params)?;
    let outbound_id = Uuid::parse_str(p.id.trim()).map_err(|_| {
        RuntimeError::InvalidInput(
            "delivered: `id` must be the full outbound UUID returned as `full_id` by \
             comm.send or comm.reply, or surfaced as `outbound_id` in an ambiguous \
             atomic-write error"
                .into(),
        )
    })?;

    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
    let row = reader
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT COUNT(*) AS inbound_count \
                  FROM notes \
                  WHERE namespace = ?1 \
                    AND kind = 'message' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.direction') = 'inbound' \
                    AND json_extract(properties, '$.from_actor') = ?2 \
                    AND json_extract(properties, '$.outbound_ref') = ?3"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(token.actor().id.clone()),
                SqlValue::Text(outbound_id.to_string()),
            ],
            label: Some("comm_delivered".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    let inbound_count = match row.and_then(|row| row.get("inbound_count").cloned()) {
        Some(SqlValue::Integer(count)) if count >= 0 => count,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "delivered: storage returned an invalid inbound count: {other:?}"
            )))
        }
    };
    let delivered = inbound_count > 0;

    Ok(json!({
        "id": outbound_id,
        "status": if delivered { "delivered" } else { "undelivered" },
        "delivered": delivered,
        "inbound_count": inbound_count,
    }))
}

/// `inbox` — list inbound messages by default, or caller-authored sent rows (ADR-057).
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_inbox
pub(crate) async fn handle_inbox(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: InboxParams = deser(params)?;
    validate_message_projection_fields("inbox", p.fields.as_deref())?;
    let raw_limit = p.limit.unwrap_or(20);
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
        let unread_count = if mailbox == "inbox" {
            count_unread_messages(runtime, token, &token.actor().id).await?
        } else {
            0
        };
        return Ok(json!({
            "messages": [],
            "count": 0,
            "unread_count": unread_count,
            "offset": offset,
            "next_offset": Value::Null,
            "has_more": false,
        }));
    }
    let limit = raw_limit.clamp(1, 200) as usize;

    let caller_actor = token.actor().id.clone();

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
        // ADR-057 Q3: to_actor filter, EqOrMissing so legacy to_actor-less messages stay
        // visible; closes the #199 multi-actor read leak for non-"local" callers.
        property_filters.push(PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op: FilterOp::EqOrMissing,
            value: SqlValue::Text(caller_actor.clone()),
        });
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
            op: if caller_actor == "local" {
                FilterOp::EqOrMissing
            } else {
                FilterOp::Eq
            },
            value: SqlValue::Text(caller_actor.clone()),
        });
        if let Some(to_actor) = p.to_actor.as_ref() {
            property_filters.push(PropertyFilter {
                json_path: "$.to_actor".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text(to_actor.clone()),
            });
        }
    }

    let filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        order_by: None, // preserves existing created_at DESC ordering
        min_created_at: since_micros,
        ..Default::default()
    };
    let store = runtime.notes(token)?;

    let subject_needle = p
        .subject_contains
        .as_ref()
        .map(|value| value.to_lowercase());
    let content_needle = p
        .content_contains
        .as_ref()
        .map(|value| value.to_lowercase());
    let has_post_filter = p.from_prefix.is_some()
        || p.exclude_from_actor.is_some()
        || before_micros.is_some()
        || subject_needle.is_some()
        || content_needle.is_some();

    // Offset is defined over the fully-filtered sequence. When a filter cannot
    // be represented by `NoteFilter`, scan the indexed base query and count only
    // matching rows before collecting one lookahead item for `has_more`.
    let mut messages: Vec<Value> = if has_post_filter {
        const PAGE_SIZE: u32 = 200;
        let mut collected: Vec<Value> = Vec::new();
        let mut matched: u64 = 0;
        let mut db_offset: u64 = 0;
        loop {
            let page = store
                .query_notes_filtered(
                    token.namespace().as_str(),
                    &filter,
                    PageRequest {
                        limit: PAGE_SIZE,
                        offset: db_offset,
                    },
                )
                .await?;
            let fetched = page.items.len() as u32;
            for n in &page.items {
                if !inbox_note_matches(
                    n,
                    &p,
                    before_micros,
                    subject_needle.as_deref(),
                    content_needle.as_deref(),
                ) {
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
            db_offset = db_offset.checked_add(u64::from(PAGE_SIZE)).ok_or_else(|| {
                RuntimeError::InvalidInput("inbox: pagination offset overflowed".into())
            })?;
        }
        collected
    } else {
        let page = store
            .query_notes_filtered(
                token.namespace().as_str(),
                &filter,
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
    let count = messages.len();
    // #66: cheap derived stat over the page already fetched above — no extra
    // DB round-trip. The count is inbox-only: sent rows have no recipient read
    // state and report zero. For inbox `status="unread"`, this equals `count`;
    // for `"read"`/`"all"`, it counts unread rows in this page (not a global
    // total — `comm.unread` is the verb for that).
    let unread_count = if mailbox == "inbox" {
        messages
            .iter()
            .filter(|m| !m["read"].as_bool().unwrap_or(false))
            .count()
    } else {
        0
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
        .map(|message| project_message_json(message, p.fields.as_deref()))
        .collect();
    Ok(json!({
        "messages": messages,
        "count": count,
        "unread_count": unread_count,
        "offset": offset,
        "next_offset": next_offset,
        "has_more": has_more,
    }))
}

/// `unread` — count-only view of the caller's unread inbound messages (#66):
/// same filter stack as `inbox(status="unread")`.
///
/// `NoteStore` has no filtered `COUNT(*)` projection (only `count_notes`,
/// which counts a whole namespace/kind with no property filter) — adding one
/// is an OSS `khive-storage` change, out of scope here. This pages through
/// `query_notes_filtered` the same way `handle_inbox`'s `#493` from_actor/
/// from_prefix path already does, summing page lengths instead of fetching a
/// bounded `limit` of full payloads — heavier than a real `COUNT(*)` but
/// correct, and consistent with the pagination style already in this file.
pub(crate) async fn handle_unread(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let _: UnreadParams = deser(params)?;
    let caller_actor = token.actor().id.clone();
    let count = count_unread_messages(runtime, token, &caller_actor).await?;

    Ok(json!({ "count": count, "actor": caller_actor }))
}

async fn count_unread_messages(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    caller_actor: &str,
) -> Result<u64, RuntimeError> {
    let property_filters = vec![
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
        // ADR-057 Q3: EqOrMissing so legacy to_actor-less messages still count
        // (same visibility rule `inbox` applies).
        PropertyFilter {
            json_path: "$.to_actor".to_string(),
            op: FilterOp::EqOrMissing,
            value: SqlValue::Text(caller_actor.to_string()),
        },
    ];

    let filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters,
        order_by: None,
        ..Default::default()
    };
    let store = runtime.notes(token)?;

    const PAGE_SIZE: u32 = 200;
    let mut count: u64 = 0;
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
        count += u64::from(fetched);
        if fetched < PAGE_SIZE {
            break;
        }
        db_offset += PAGE_SIZE;
    }

    Ok(count)
}

/// `read` — mark a message as read.
pub(crate) async fn handle_read(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ReadParams = deser(params)?;
    match (p.id, p.ids) {
        (Some(_), Some(_)) => Err(RuntimeError::InvalidInput(
            "read: `id` and `ids` are mutually exclusive".into(),
        )),
        (None, None) => Err(RuntimeError::InvalidInput(
            "read: exactly one of `id` or `ids` is required".into(),
        )),
        (Some(raw), None) => {
            let (id, note) = validate_read_target(runtime, token, &raw).await?;
            mark_read_target(runtime, token, id, note).await
        }
        (None, Some(raw_ids)) => {
            const MAX_BULK_READ_IDS: usize = 500;
            if raw_ids.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "read: `ids` must contain at least one message id".into(),
                ));
            }
            if raw_ids.len() > MAX_BULK_READ_IDS {
                return Err(RuntimeError::InvalidInput(format!(
                    "read: `ids` accepts at most {MAX_BULK_READ_IDS} message ids, got {}",
                    raw_ids.len()
                )));
            }

            // Validate every target before the first mutation so malformed,
            // outbound, or wrong-addressee input cannot produce a partial bulk read.
            let requested_count = raw_ids.len();
            let mut seen = HashSet::new();
            let mut targets = Vec::with_capacity(requested_count);
            for raw in raw_ids {
                let (id, note) = validate_read_target(runtime, token, &raw).await?;
                if seen.insert(id) {
                    targets.push((id, note));
                }
            }

            let mut results = Vec::with_capacity(targets.len());
            for (id, note) in targets {
                let original_properties = note.properties.clone();
                match mark_read_target(runtime, token, id, note).await {
                    Ok(result) => results.push(result),
                    Err(error) => results.push(json!({
                        "id": short_id(id),
                        "full_id": id.as_hyphenated().to_string(),
                        "read": false,
                        "mark_error": error.to_string(),
                        "properties": original_properties,
                    })),
                }
            }
            let marked_count = results
                .iter()
                .filter(|result| result["read"].as_bool() == Some(true))
                .count();
            let unique_count = results.len();
            let failed_count = unique_count - marked_count;
            Ok(json!({
                "results": results,
                "requested_count": requested_count,
                "unique_count": unique_count,
                "marked_count": marked_count,
                "failed_count": failed_count,
            }))
        }
    }
}

async fn validate_read_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
) -> Result<(Uuid, Note), RuntimeError> {
    let id = resolve_id(runtime, token, raw, "read").await?;

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

    // #87: `read` mutates delivery state that belongs to the addressee — restrict
    // it to the message's own `to_actor`, mirroring the strictness of the direction
    // check above. Without this, any caller actor in the namespace could flip another
    // actor's inbound message to read, corrupting the unread counts that fleet-wide
    // wake/sweep logic polls. The error names only the caller's own actor, never the
    // real addressee, so a non-addressee cannot use this to enumerate who a message
    // was meant for.
    //
    // Pre-ADR-057 messages may carry no `to_actor` at all. That is treated as
    // fail-open (with a warning), matching the inbox `EqOrMissing` filter's
    // precedent (#199): those legacy messages are already visible to any caller via
    // `comm.inbox` (no to_actor to filter on), so fail-closed here would leave them
    // permanently unreadable and stuck "unread" — defeating the same wake/sweep logic
    // this fix protects. The anonymous single-tenant default ("local") deployment is
    // unaffected either way: caller and to_actor are both "local", so the equality
    // check passes normally.
    let caller_actor = token.actor().id.as_str();
    if let Some(to_actor) = note
        .properties
        .as_ref()
        .and_then(|p| p.get("to_actor"))
        .and_then(Value::as_str)
    {
        if to_actor != caller_actor {
            return Err(RuntimeError::InvalidInput(format!(
                "read: message {id} is not addressed to caller actor {caller_actor:?}"
            )));
        }
    } else {
        tracing::warn!(
            id = %id,
            caller_actor = %caller_actor,
            "comm.read: message has no `to_actor` (pre-ADR-057 legacy); allowing read \
             without addressee verification (issue #87)"
        );
    }

    Ok((id, note))
}

async fn mark_read_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    note: Note,
) -> Result<Value, RuntimeError> {
    // Patch via one atomic JSON-property `UPDATE`, not a get/replace cycle or
    // `upsert_note` (#1483, #780). See docs/api/message-lifecycle.md#handlersrshandle_read
    let store = runtime.notes(token)?;

    // `orig_props` is kept as the stored `Option<Value>` (a SQL-NULL
    // properties column is a real, distinct state from `{}`) so a degraded
    // response can report exactly what is stored.
    let orig_props = note.properties.clone();
    let updated_at = Utc::now().timestamp_micros();
    let caller_actor = token.actor().id.as_str();

    // Storage-side compare-and-swap: patches only the `$.read` key via
    // `json_set` instead of overwriting the whole `properties` column with
    // this call's snapshot (which bulk read's up-to-500-target
    // validate-then-mark window can leave stale — a concurrent write to any
    // other property between validation and this call must survive), and
    // rechecks kind/direction/addressee against the row's *current* state in
    // the same `UPDATE` — the same eligibility predicate
    // `validate_read_target` already checked, re-evaluated at mutation time
    // rather than trusted from an earlier read.
    let recheck_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.direction".to_string(),
                op: FilterOp::NotInOrMissing(vec![SqlValue::Text("outbound".to_string())]),
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.to_actor".to_string(),
                op: FilterOp::EqOrMissing,
                value: SqlValue::Text(caller_actor.to_string()),
            },
        ],
        ..Default::default()
    };

    // Best-effort: under multi-client writer contention the pool checkout can
    // time out. The read itself already succeeded above — failing the whole
    // call over a delivery-state patch would throw away a successful read for
    // a caller who cannot retry the fetch half. Mirrors handle_reply's
    // fold-in mark-read: `Ok(false)` (no live row currently matches, e.g.
    // soft-deleted or an eligibility property changed mid-flight) and `Err`
    // both degrade to `read: false` + `mark_error` instead of failing the
    // response. A caller polling unread counts simply sees the message still
    // unread and can re-issue `read` — self-healing, no retry loop needed here.
    let patch_result = store
        .try_patch_note_property(
            id,
            token.namespace().as_str(),
            &recheck_filter,
            "$.read",
            json!(true),
            updated_at,
        )
        .await;

    // Only a successful patch needs the fresh row: `read_response`'s
    // `Ok(false)`/`Err` arms report `orig_props` (what is still stored), not
    // this value.
    let patched_properties = if matches!(patch_result, Ok(true)) {
        match store.get_note(id).await {
            Ok(Some(fresh)) => fresh.properties.unwrap_or_else(|| json!({})),
            _ => {
                let mut fallback = orig_props.clone().unwrap_or_else(|| json!({}));
                fallback["read"] = json!(true);
                fallback
            }
        }
    } else {
        Value::Null
    };

    Ok(read_response(
        short_id(id),
        id.as_hyphenated().to_string(),
        patch_result,
        orig_props,
        patched_properties,
    ))
}

/// Assemble `comm.read`'s response from the mark-read patch outcome.
///
/// Factored out so the three degrade arms (`Ok(true)`, `Ok(false)`, `Err`)
/// are unit-testable directly: the `Ok(false)`/soft-delete-mid-flight race
/// cannot be arranged honestly through the public dispatch path (`handle_read`
/// fetches and patches within a single sequential call, with no seam to
/// inject a concurrent delete between the two), so the response shape is
/// verified against this pure function instead of a racing integration test.
fn read_response(
    short: String,
    full: String,
    patch_result: Result<bool, khive_storage::StorageError>,
    original_properties: Option<Value>,
    patched_properties: Value,
) -> Value {
    match patch_result {
        Ok(true) => json!({
            "id": short,
            "full_id": full,
            "read": true,
            "properties": patched_properties,
        }),
        Ok(false) => json!({
            "id": short,
            "full_id": full,
            "read": false,
            "mark_error": "no live row updated",
            "properties": original_properties,
        }),
        Err(e) => {
            tracing::warn!(
                id = %full,
                error = %e,
                "comm.read: mark-read update failed under writer contention; \
                 degrading to read:false (best-effort)"
            );
            json!({
                "id": short,
                "full_id": full,
                "read": false,
                "mark_error": e.to_string(),
                "properties": original_properties,
            })
        }
    }
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

    // #113: sibling of #87 — `reply` never checked who the caller is, so a
    // caller holding a message id could reply to a message addressed to a
    // different actor entirely (the reply then routes via the "other party"
    // logic below, which assumes the caller IS one of the two parties). The
    // rule chosen is thread-participant, not addressee-only: either party to
    // the exchange (the addressee or the original sender) may reply, mirroring
    // #94's thread-visibility filter rather than #87's stricter read-only
    // rule — a reply from either party is a normal continuation of the
    // exchange, unlike a third party silently flipping delivery state. #85's
    // read-mark scoping at `d782709` closed the read-state side of this hole;
    // the reply itself was still open.
    //
    // Fail open only when the original carries neither `to_actor` nor
    // `from_actor` (pre-ADR-057 legacy — no attributed party to restrict
    // against, matching #87/#94's rule for such rows). An unattributed caller
    // is still actor `local`, so it may reply to local party-line messages but
    // not messages attributed to other participants.
    if original_to_actor.is_some() || original_from_actor.is_some() {
        let caller_actor = token.actor().id.as_str();
        let is_participant = original_from_actor.as_deref() == Some(caller_actor)
            || original_to_actor.as_deref() == Some(caller_actor);
        if !is_participant {
            return Err(RuntimeError::InvalidInput(format!(
                "reply: message {id} is not addressed to or from caller actor {caller_actor:?}"
            )));
        }
    } else {
        tracing::warn!(
            id = %id,
            caller_actor = %token.actor().id,
            "comm.reply: message has no `to_actor`/`from_actor` (pre-ADR-057 legacy); \
             allowing reply without participant verification (issue #113)"
        );
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

    let reply_subject = if original_subject.starts_with("Re: ") || original_subject.is_empty() {
        original_subject.clone()
    } else {
        format!("Re: {original_subject}")
    };

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
    let (reply_note, embedding_truncation) = dual_write_message(
        runtime,
        token,
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
    )
    .await?;

    // Replying is the strongest possible read signal, and callers universally
    // chained `reply | read` to say so — fold it in. Skips only an explicitly
    // outbound original, matching handle_read's rejection exactly rather than
    // requiring a literal "inbound" (legacy messages may carry no direction).
    // Best-effort: the reply is already committed above, so a failed or
    // no-op patch degrades to `marked_read: false` rather than failing a
    // delivered reply.
    //
    // The mark is also addressee-scoped: "I read it" is only a claim the
    // addressee can make. Replying does not give a third party the right to
    // flip someone else's message, which is the same rule handle_read
    // enforces. A legacy original with no `to_actor` fails open, matching
    // handle_inbox's EqOrMissing visibility — a message anyone can see in
    // their inbox must stay markable by someone, or it inflates unread
    // counts forever.
    let original_direction = orig_props
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("");
    let caller_is_addressee = original_to_actor
        .as_deref()
        .is_none_or(|addressee| addressee == from_actor_label);
    let marked_read = if original_direction == "outbound" || !caller_is_addressee {
        None
    } else {
        let updated_at = Utc::now().timestamp_micros();
        // `Ok(false)` means no live row was updated (e.g. the original was
        // soft-deleted mid-flight, or its properties ceased to be an object)
        // — that is not a successful mark. The one-statement property set
        // preserves every unrelated key without a race window (#1483).
        Some(
            store
                .set_note_property(id, "read", json!(true), updated_at)
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
    add_embedding_truncation_warning(&mut response, &embedding_truncation);
    Ok(response)
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
    validate_message_projection_fields("thread", p.fields.as_deref())?;
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

    let (canonical_thread_id, selected_raw_thread_id, root_note): (String, Option<String>, Note) = {
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
        let stored_root = note
            .properties
            .as_ref()
            .and_then(|p| p.get("thread_id"))
            .and_then(Value::as_str)
            .and_then(|raw| raw.trim().parse::<Uuid>().ok().map(|id| (id, raw)));
        let canonical = match stored_root {
            Some((stored_root, _)) if stored_root != passed_uuid => {
                stored_root.as_hyphenated().to_string()
            }
            _ => passed_uuid.as_hyphenated().to_string(),
        };
        // Keep the selected row's exact pre-v1 spelling as an additional
        // compatibility probe. The full formatter-derived set is built below
        // once the canonical root UUID is known; no row is mutated.
        let selected_raw = stored_root.map(|(_, raw)| raw.trim().to_string());
        (canonical, selected_raw, note)
    };

    // Push every exact pre-v1 UUID spelling into one indexed IN predicate. This
    // keeps a mixed legacy/v1 conversation whole regardless of whether lookup
    // starts from its canonical root, a new v1 child, or a legacy child.
    let thread_store = runtime.notes(token)?;
    const PAGE_SIZE: u32 = 200;
    let mut rows: Vec<ThreadRow> = Vec::new();
    let canonical_root = canonical_thread_id
        .parse::<Uuid>()
        .expect("canonical_thread_id is produced from a parsed UUID");
    let thread_id_values =
        thread_id_query_spellings(canonical_root, selected_raw_thread_id.as_deref())
            .into_iter()
            .map(SqlValue::Text)
            .collect();
    let thread_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.thread_id".to_string(),
            op: FilterOp::In(thread_id_values),
            value: SqlValue::Null,
        }],
        order_by: None,
        ..Default::default()
    };
    let mut db_offset: u32 = 0;
    let mut seen_row_ids = HashSet::new();
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
            if seen_row_ids.insert(n.id) {
                rows.push(ThreadRow {
                    created_at: n.created_at,
                    full_id: n.id,
                    json: note_to_message_json(n),
                });
            }
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

    // #94 fix 1/2 — actor visibility: mirror `handle_inbox`'s EqOrMissing model
    // (ADR-057 Q3) instead of the unfiltered namespace-wide read `thread` had
    // before. A caller may see a row iff they are a party to it (its sender or
    // its addressee) or the row predates actor labeling (`to_actor` absent,
    // back-compat visible-to-all — same rule `inbox` already applies). Before
    // this filter, any caller who could resolve a thread id saw every actor's
    // copies in that thread, including another actor's unread inbound state
    // (issue #94 symptom 1/2: thread crossed the caller boundary that inbox
    // already enforced).
    let caller_actor = token.actor().id.clone();
    rows.retain(|r| {
        let props = r.json.get("properties");
        let to_actor = props
            .and_then(|p| p.get("to_actor"))
            .and_then(Value::as_str);
        let from_actor = props
            .and_then(|p| p.get("from_actor"))
            .and_then(Value::as_str);
        from_actor == Some(caller_actor.as_str())
            || to_actor.is_none()
            || to_actor == Some(caller_actor.as_str())
    });

    // #94 fix 2/2 — collapse the ADR-057 dual-write pair (outbound copy +
    // inbound copy) of one logical message into a single thread entry. The
    // inbound copy's `properties.outbound_ref` names its outbound twin's
    // note id (set by `dual_write_message`); a row without that link (a
    // directly-ingested inbound message, or legacy data) is its own logical
    // message. Without this, `thread` rendered both dual-write copies as two
    // entries — a reply appeared twice with no marker distinguishing the
    // copies (issue #94 symptom 3). The outbound copy (always the physically
    // earlier row — `dual_write_message` creates it first) is kept as the
    // canonical entry; the inbound twin's `read` state is folded in, since
    // that is the only field where the two copies can differ meaningfully.
    fn logical_id(row: &ThreadRow) -> Uuid {
        let props = row.json.get("properties");
        let direction = props
            .and_then(|p| p.get("direction"))
            .and_then(Value::as_str);
        if direction == Some("inbound") {
            if let Some(oref) = props
                .and_then(|p| p.get("outbound_ref"))
                .and_then(Value::as_str)
            {
                if let Ok(u) = oref.parse::<Uuid>() {
                    return u;
                }
            }
        }
        row.full_id
    }
    fn is_outbound(row: &ThreadRow) -> bool {
        row.json
            .get("properties")
            .and_then(|p| p.get("direction"))
            .and_then(Value::as_str)
            == Some("outbound")
    }

    let mut canonical_order: Vec<Uuid> = Vec::new();
    let mut canonical: HashMap<Uuid, ThreadRow> = HashMap::new();
    for row in rows {
        let lid = logical_id(&row);
        match canonical.get_mut(&lid) {
            None => {
                canonical_order.push(lid);
                canonical.insert(lid, row);
            }
            Some(existing) => {
                // Prefer the outbound copy as the canonical entry (earlier,
                // and it is what `comm.send`/`comm.reply` return as `id`),
                // but carry the inbound twin's `read` state across either way.
                if is_outbound(&row) && !is_outbound(existing) {
                    let read = existing.json.get("read").cloned();
                    let mut promoted = row;
                    if let (Some(read), Some(obj)) = (read, promoted.json.as_object_mut()) {
                        obj.insert("read".to_string(), read);
                    }
                    *existing = promoted;
                } else if !is_outbound(&row) && is_outbound(existing) {
                    if let (Some(read), Some(obj)) =
                        (row.json.get("read").cloned(), existing.json.as_object_mut())
                    {
                        obj.insert("read".to_string(), read);
                    }
                }
            }
        }
    }
    let mut rows: Vec<ThreadRow> = canonical_order
        .into_iter()
        .filter_map(|lid| canonical.remove(&lid))
        .collect();

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
    let messages: Vec<Value> = rows
        .into_iter()
        .map(|row| project_message_json(row.json, p.fields.as_deref()))
        .collect();

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
                    (thread_id, from_actor)
                });
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
                        .query_notes_filtered(
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
    // Metadata passthrough (#448): merged additively so it never clobbers a
    // field set above. Stable v1 names are reserved even when their optional
    // field is absent on an ingest (`subject`, `outbound_ref`,
    // `sent_by_process`), preventing adapter metadata from fabricating a
    // contract field or process provenance.
    if let Some(metadata) = p.metadata {
        if let Some(obj) = props.as_object_mut() {
            for (k, v) in metadata {
                if COMM_STABLE_PROPERTY_KEYS.contains(&k.as_str()) {
                    continue;
                }
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

    let now = Utc::now();
    let at = p.at.clone().unwrap_or_else(|| now.to_rfc3339());

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
fn channel_health_to_json(note: &Note, as_of: &DateTime<Utc>) -> Value {
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
    })
}

/// `health` — read-only per-channel health snapshot (khive #606). Reads
/// `channel_health` rows from `token.namespace()` (khive #877 namespace
/// scoping). The additive `stalled` schedule fact is deliberately narrower
/// than a computed `healthy: bool`; overall health judgment still belongs to
/// the caller. See crates/khive-pack-comm/docs/api/channel-health.md#handlersrshandle_health
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

    let now = Utc::now();
    let channels: Vec<Value> = page
        .items
        .iter()
        .map(|note| channel_health_to_json(note, &now))
        .collect();
    let as_of = now.to_rfc3339();

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
        add_embedding_truncation_warning, build_references_header, channel_stalled,
        heartbeat_note_id, mark_read_target, message_id_match_candidates, parent_references_chain,
        parent_wire_message_id, read_response, sanitize_reference_token, validate_read_target,
        wrap_message_id,
    };
    use khive_storage::StorageError;
    use serde_json::{json, Value};

    #[test]
    fn channel_stalled_uses_strict_three_interval_threshold() {
        let props = json!({
            "poll_interval_secs": 5,
            "last_poll_attempt_at": "2026-08-01T12:00:00Z",
            "consecutive_failures": 0,
        });
        let at_threshold = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:00:15Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let overdue = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:00:15.001Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        assert_eq!(channel_stalled(&props, &at_threshold), Some(false));
        assert_eq!(channel_stalled(&props, &overdue), Some(true));
    }

    #[test]
    fn channel_stalled_requires_valid_consecutive_failures() {
        let as_of = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut props = json!({
            "poll_interval_secs": 5,
            "last_poll_attempt_at": "2026-08-01T12:00:00Z",
        });

        assert_eq!(channel_stalled(&props, &as_of), None);

        for malformed in [json!("1"), json!(-1)] {
            props["consecutive_failures"] = malformed;
            assert_eq!(channel_stalled(&props, &as_of), None);
        }
    }

    #[test]
    fn comm_write_response_reports_atomic_note_embedding_truncation() {
        let mut response = json!({"id": "abc123"});
        add_embedding_truncation_warning(
            &mut response,
            &khive_runtime::retrieval::EmbeddingTruncationReport {
                truncated: 2,
                discarded_bytes: 18,
            },
        );
        assert_eq!(
            response["warnings"],
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING])
        );
    }

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

    // read_response's three arms are unit-tested directly because the
    // `Ok(false)` case (a live row vanishing between handle_read's `get_note`
    // and its `set_note_property` call) cannot be arranged honestly
    // through the public dispatch path: the two calls are sequential within
    // one handler invocation with no seam to inject a concurrent delete.

    #[test]
    fn read_response_ok_true_reports_read_and_patched_properties() {
        let original = json!({ "direction": "inbound", "read": false });
        let patched = json!({ "direction": "inbound", "read": true });
        let resp = read_response(
            "abc123".to_string(),
            "full-uuid".to_string(),
            Ok(true),
            Some(original),
            patched.clone(),
        );
        assert_eq!(resp["id"], json!("abc123"));
        assert_eq!(resp["full_id"], json!("full-uuid"));
        assert_eq!(resp["read"], json!(true));
        assert_eq!(resp["properties"], patched);
        assert!(
            resp.get("mark_error").is_none(),
            "a successful mark must not carry mark_error; got {resp}"
        );
    }

    #[test]
    fn read_response_ok_false_degrades_without_claiming_the_patch_landed() {
        let original = json!({ "direction": "inbound", "read": false });
        let patched = json!({ "direction": "inbound", "read": true });
        let resp = read_response(
            "abc123".to_string(),
            "full-uuid".to_string(),
            Ok(false),
            Some(original.clone()),
            patched,
        );
        assert_eq!(resp["id"], json!("abc123"));
        assert_eq!(resp["full_id"], json!("full-uuid"));
        assert_eq!(resp["read"], json!(false));
        assert_eq!(
            resp["mark_error"],
            json!("no live row updated"),
            "got {resp}"
        );
        assert_eq!(
            resp["properties"], original,
            "must report the ORIGINAL stored properties, never the attempted \
             patch, when the write did not land; got {resp}"
        );
    }

    #[test]
    fn read_response_ok_false_preserves_stored_null_properties() {
        let patched = json!({ "read": true });
        let resp = read_response(
            "abc123".to_string(),
            "full-uuid".to_string(),
            Ok(false),
            None,
            patched,
        );
        assert_eq!(resp["id"], json!("abc123"));
        assert_eq!(resp["full_id"], json!("full-uuid"));
        assert_eq!(resp["read"], json!(false));
        assert_eq!(
            resp["properties"],
            Value::Null,
            "a stored SQL-NULL properties column must round-trip as JSON \
             null, never as {{}}; got {resp}"
        );
    }

    #[test]
    fn read_response_err_degrades_and_reports_the_error_string() {
        let original = json!({ "direction": "inbound", "read": false });
        let patched = json!({ "direction": "inbound", "read": true });
        let err = StorageError::Timeout {
            operation: "set_note_property".into(),
        };
        let err_text = err.to_string();
        let resp = read_response(
            "abc123".to_string(),
            "full-uuid".to_string(),
            Err(err),
            Some(original.clone()),
            patched,
        );
        assert_eq!(resp["id"], json!("abc123"));
        assert_eq!(resp["full_id"], json!("full-uuid"));
        assert_eq!(resp["read"], json!(false));
        assert_eq!(resp["mark_error"], json!(err_text));
        assert_eq!(
            resp["properties"], original,
            "must report the ORIGINAL stored properties on a write error; got {resp}"
        );
    }

    #[test]
    fn read_response_err_preserves_stored_null_properties() {
        let patched = json!({ "read": true });
        let err = StorageError::Timeout {
            operation: "set_note_property".into(),
        };
        let resp = read_response(
            "abc123".to_string(),
            "full-uuid".to_string(),
            Err(err),
            None,
            patched,
        );
        assert_eq!(resp["id"], json!("abc123"));
        assert_eq!(resp["full_id"], json!("full-uuid"));
        assert_eq!(resp["read"], json!(false));
        assert_eq!(
            resp["properties"],
            Value::Null,
            "a stored SQL-NULL properties column must round-trip as JSON \
             null, never as {{}}; got {resp}"
        );
    }

    // Regression for the bulk-read lost-update: prevalidation (`validate_read_target`)
    // snapshots a `Note`, but bulk read's validate-then-mark window can span up to
    // 500 targets, during which another writer can change an unrelated property.
    // `mark_read_target` must never write that stale snapshot's `properties` back —
    // only the `read` key may change, and any property that landed after the
    // snapshot but before the mark must survive.
    #[tokio::test]
    async fn mark_read_target_preserves_a_property_written_after_prevalidation() {
        use khive_runtime::{AllowAllGate, BackendId, Namespace, RuntimeConfig};
        use khive_storage::note::Note;
        use uuid::Uuid;

        let ns = format!("mark-read-cas-{}", Uuid::new_v4().simple());
        let runtime = super::KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::parse(&ns).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("in-memory runtime");
        let token = runtime
            .authorize(Namespace::parse(&ns).unwrap())
            .expect("authorize");
        let store = runtime.notes(&token).expect("notes store");

        let id = Uuid::new_v4();
        let created_at = chrono::Utc::now().timestamp_micros();
        store
            .upsert_note(Note {
                id,
                namespace: ns.clone(),
                kind: "message".to_string(),
                status: "active".to_string(),
                name: None,
                content: "concurrency regression".to_string(),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(json!({
                    "direction": "inbound",
                    // `actor_id: None` in the config below resolves to the
                    // anonymous actor, whose id is always "local" regardless
                    // of namespace — see `khive_runtime::actor_identity::resolve_actor`.
                    "to_actor": "local",
                    "read": false,
                })),
                created_at,
                updated_at: created_at,
                deleted_at: None,
            })
            .await
            .expect("insert message");

        // Prevalidation snapshot — this is what a bulk read's validate phase
        // would have captured for this target before iterating the rest of a
        // (possibly large) id list.
        let (validated_id, stale_note) = validate_read_target(&runtime, &token, &id.to_string())
            .await
            .expect("prevalidation");
        assert_eq!(validated_id, id);

        // Simulate a concurrent write landing after prevalidation but before
        // this target's mark step: another property changes, `read` stays false.
        let concurrent_updated_at = created_at + 1;
        store
            .update_note_properties(
                id,
                Some(json!({
                    "direction": "inbound",
                    "to_actor": stale_note.properties.as_ref().unwrap()["to_actor"].clone(),
                    "read": false,
                    "flagged": true,
                })),
                concurrent_updated_at,
            )
            .await
            .expect("concurrent property write");

        // Mark using the now-stale snapshot, exactly as the bulk mark loop does.
        let result = mark_read_target(&runtime, &token, id, stale_note)
            .await
            .expect("mark_read_target");
        assert_eq!(result["read"], json!(true), "got {result}");

        let stored = store
            .get_note(id)
            .await
            .expect("get_note")
            .expect("note still present");
        let props = stored.properties.expect("properties present");
        assert_eq!(
            props["read"],
            json!(true),
            "the mark itself must still land; got {props}"
        );
        assert_eq!(
            props["flagged"],
            json!(true),
            "a property written after prevalidation but before the mark must \
             survive — the mark must never write back the stale snapshot; got {props}"
        );
    }

    // Regression: a message whose stored `properties` document is not a JSON
    // object (scalar or array) must never be reported as read. `json_set`
    // silently leaves such a document unchanged while still returning it, so
    // without a non-object guard the `UPDATE` would still match the row and
    // `comm.read` would falsely report `read: true` for a patch that stored
    // nothing.
    #[tokio::test]
    async fn mark_read_target_reports_unread_for_non_object_properties() {
        use khive_runtime::{AllowAllGate, BackendId, Namespace, RuntimeConfig};
        use khive_storage::note::Note;
        use uuid::Uuid;

        for (case, properties) in [
            ("scalar", json!(1)),
            ("array", json!(["not", "an", "object"])),
        ] {
            let ns = format!("mark-read-non-object-{case}-{}", Uuid::new_v4().simple());
            let runtime = super::KhiveRuntime::new(RuntimeConfig {
                git_write: Default::default(),
                db_path: None,
                default_namespace: Namespace::parse(&ns).unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                gate: std::sync::Arc::new(AllowAllGate),
                packs: vec!["kg".to_string(), "comm".to_string()],
                backend_id: BackendId::main(),
                brain_profile: None,
                visible_namespaces: vec![],
                allowed_outbound_namespaces: vec![],
                actor_id: None,
            })
            .expect("in-memory runtime");
            let token = runtime
                .authorize(Namespace::parse(&ns).unwrap())
                .expect("authorize");
            let store = runtime.notes(&token).expect("notes store");

            let id = Uuid::new_v4();
            let created_at = chrono::Utc::now().timestamp_micros();
            let note = Note {
                id,
                namespace: ns.clone(),
                kind: "message".to_string(),
                status: "active".to_string(),
                name: None,
                content: format!("{case} properties"),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(properties.clone()),
                created_at,
                updated_at: created_at,
                deleted_at: None,
            };
            store
                .upsert_note(note.clone())
                .await
                .expect("insert message");

            let result = mark_read_target(&runtime, &token, id, note)
                .await
                .expect("mark_read_target");
            assert_eq!(
                result["read"],
                json!(false),
                "{case} properties document must not be reported as read; got {result}"
            );
            assert_eq!(
                result["properties"], properties,
                "{case} properties must round-trip unchanged; got {result}"
            );

            let stored = store
                .get_note(id)
                .await
                .expect("get_note")
                .expect("note still present");
            assert_eq!(
                stored.properties,
                Some(properties),
                "{case} properties must remain unchanged in storage"
            );
            assert_eq!(
                stored.updated_at, created_at,
                "{case} updated_at must not advance when the patch is refused"
            );
        }
    }
}
