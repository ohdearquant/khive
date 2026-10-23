//! Core message primitives: ID helpers and the dual-write delivery function.

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{note::Note, StorageError, WriterTaskRequestState};
use khive_types::{Details, KhiveError};

pub(crate) const COMM_SCHEMA_VERSION: u64 = 1;
pub(crate) const COMM_STABLE_PROPERTY_KEYS: &[&str] = &[
    "comm_schema_version",
    "direction",
    "read",
    "from_actor",
    "to_actor",
    "from",
    "to",
    "thread_id",
    "subject",
    "sent_at",
    "outbound_ref",
    "sent_by_process",
];

/// Closed field vocabulary accepted by list-read message projections.
///
/// The first group is the ordinary top-level message view. The second group
/// exposes stable property keys as top-level aliases only when a caller opts
/// into projection, so callers can request routing/timestamp metadata without
/// paying for the entire `properties` object.
pub(crate) const MESSAGE_PROJECTION_FIELDS: &[&str] = &[
    "id",
    "short_id",
    "full_id",
    "kind",
    "from",
    "to",
    "subject",
    "read",
    "direction",
    "preview",
    "content",
    "namespace",
    "properties",
    "created_at",
    "updated_at",
    "comm_schema_version",
    "from_actor",
    "to_actor",
    "thread_id",
    "sent_at",
    "outbound_ref",
    "sent_by_process",
    "idempotency_key",
    "attachments",
    "attachments_error",
    "origin",
    "trust",
    "labels",
];

pub(crate) fn validate_message_projection_fields(
    verb: &str,
    fields: Option<&[String]>,
) -> Result<(), RuntimeError> {
    let Some(fields) = fields else {
        return Ok(());
    };
    if fields.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `fields` must contain at least one field"
        )));
    }
    if let Some(unknown) = fields
        .iter()
        .find(|field| !MESSAGE_PROJECTION_FIELDS.contains(&field.as_str()))
    {
        let unknown = mask_for_redaction_surface(RedactionSurface::GateProbe, unknown);
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: unknown projection field {unknown:?}; expected one of: {}",
            MESSAGE_PROJECTION_FIELDS.join(", ")
        )));
    }
    Ok(())
}

pub(crate) fn project_message_json(message: Value, fields: Option<&[String]>) -> Value {
    let Some(fields) = fields else {
        return message;
    };

    let mut projected = serde_json::Map::new();
    for field in fields {
        let value = message.get(field).cloned().unwrap_or_else(|| {
            message
                .get("properties")
                .and_then(|properties| properties.get(field))
                .cloned()
                .or_else(|| match field.as_str() {
                    "from_actor" => message.get("from").cloned(),
                    "to_actor" => message.get("to").cloned(),
                    // `id` is the round-trippable form of the same UUID; keep the
                    // compatibility aliases consistent with it rather than null.
                    "short_id" => message
                        .get("id")
                        .and_then(Value::as_str)
                        .map(|id| Value::String(id.chars().take(8).collect())),
                    "full_id" => message.get("id").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Null)
        });
        projected.insert(field.clone(), value);
    }
    Value::Object(projected)
}

pub(crate) fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

fn attach_outbound_id_to_ambiguous_write(outbound_id: Uuid, error: RuntimeError) -> RuntimeError {
    match error {
        RuntimeError::Storage(ref storage)
            if matches!(
                storage.without_sqlite_write_stage(),
                StorageError::WriterTaskTerminated {
                    request_state: WriterTaskRequestState::SideEffectsUnknown,
                    ..
                }
            ) =>
        {
            RuntimeError::Khive(
                KhiveError::conflict(format!(
                    "dual_write delivery outcome is uncertain (side_effects_unknown); \
                 call comm.delivered(id=\"{outbound_id}\") before retrying"
                ))
                .with_details(Details::new_owned([(
                    "outbound_id",
                    outbound_id.to_string(),
                )])),
            )
        }
        other => other,
    }
}

pub(crate) fn note_to_message_json(note: &Note) -> Value {
    #[cfg(test)]
    crate::handlers::read_cluster_tests::observe_message_render();
    let props = note.properties.as_ref();
    let full_id = note.id.as_hyphenated().to_string();

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
        // `id` is the round-trippable identifier. Keep `full_id` as a
        // compatibility alias and expose the ambiguous prefix only as an
        // explicitly display-oriented field (#1421).
        "id": full_id.clone(),
        "short_id": short_id(note.id),
        "full_id": full_id,
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

/// Writes an outbound copy (caller namespace) and an inbound copy (recipient
/// namespace) as ONE atomic unit
/// (`khive_runtime::create_notes_atomic_with_report`): one writer transaction
/// covers both notes' rows, FTS documents, and vector rows. Returns the
/// outbound `Note` and aggregate embedding-truncation report on success.
///
/// Resolved gap (external desk review, 2026-07-21; closed by construction
/// here): the two note writes used to be separate `create_note` calls with
/// only an in-process rollback compensating an inbound-write failure, so a
/// process crash between them could leave a durable orphan outbound note
/// with no inbound copy. `create_notes_atomic_with_report` commits both copies under one
/// `SqlAccess::atomic_unit` — a crash or failure anywhere in the unit rolls
/// back everything, so no partial pair can ever be observed durably: an
/// ordinary prepare/plan failure leaves neither copy, while a successful
/// commit leaves both. If the writer seam cannot establish whether an
/// accepted request committed (`side_effects_unknown`), the pre-generated
/// outbound UUID is attached to the error so `comm.delivered` can distinguish
/// the two complete outcomes without relying on message content.
///
/// Invariant: the outbound id is generated BEFORE either write (rather than
/// patched in after, as the old two-call version did) so both copies carry
/// the canonical `thread_id` AND `comm_schema_version` from their first
/// write: for a root send (`thread_id: None`), that id IS the canonical
/// thread_id; for a reply, the caller-supplied `thread_id` is forwarded
/// unchanged. Either way there is no separate patch write, and no row is
/// ever durably observable with a canonical thread_id but no version marker
/// (or vice versa) — `create_notes_atomic` commits both in the same
/// transaction as the row itself.
///
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#messagersdual_write_message for
/// the `in_reply_to_message_id`/`references_chain` header-threading contract.
// REASON: dual_write_message mirrors the send wire shape plus its persisted metadata and the
// two context args (runtime, token). Grouping them into a struct would not reduce overall
// complexity and would require an extra allocation on the hot path; the flat signature is
// intentional.
#[cfg(test)]
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
    sent_by_process: Option<&str>,
    from_actor: Option<&str>,
    to_actor: Option<&str>,
    in_reply_to_message_id: Option<&str>,
    references_chain: Option<&str>,
    tags: Option<&[String]>,
) -> Result<(Note, khive_runtime::retrieval::EmbeddingTruncationReport), RuntimeError> {
    let result = dual_write_message_with_identity(
        runtime,
        caller_token,
        "comm.send",
        from,
        to,
        subject,
        content,
        thread_id,
        sent_at,
        sent_by_process,
        from_actor,
        to_actor,
        in_reply_to_message_id,
        references_chain,
        tags,
        &[],
        None,
    )
    .await?;
    Ok((result.outbound, result.embedding_truncation))
}

pub(crate) struct MessageWrite {
    pub outbound: Note,
    pub embedding_truncation: khive_runtime::retrieval::EmbeddingTruncationReport,
    pub replayed: bool,
}

async fn recent_external_outbound_count(
    runtime: &KhiveRuntime,
    policy: &khive_runtime::OutboundEmailPolicy,
) -> Result<u64, RuntimeError> {
    const DAY_MICROS: i64 = 24 * 60 * 60 * 1_000_000;
    let now = chrono::Utc::now().timestamp_micros();
    let since = now.saturating_sub(DAY_MICROS);
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("comm_external_outbound_email_recipients_since").into(),
            params: vec![SqlValue::Integer(since), SqlValue::Integer(now)],
            label: Some("comm_external_outbound_email_recipients_since".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    rows.iter().try_fold(0u64, |count, row| {
        let to_actor = row.text("to_actor").map_err(|_| {
            RuntimeError::Internal("external email cap query returned a malformed recipient".into())
        })?;
        let recipient = to_actor.strip_prefix("email:").ok_or_else(|| {
            RuntimeError::Internal("external email cap query returned a non-email recipient".into())
        })?;
        if policy.allows(recipient) {
            Ok(count)
        } else {
            count
                .checked_add(1)
                .ok_or_else(|| RuntimeError::Internal("external email cap count overflowed".into()))
        }
    })
}

fn external_email_denied(verb: &str, condition: &str) -> RuntimeError {
    RuntimeError::permission_denied(
        verb,
        format!("external email admission refused: {condition}; no message was committed by this request"),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn dual_write_message_with_identity(
    runtime: &KhiveRuntime,
    caller_token: &NamespaceToken,
    verb: &str,
    from: &str,
    to: &str,
    subject: Option<&str>,
    content: &str,
    thread_id: Option<&str>,
    sent_at: &str,
    sent_by_process: Option<&str>,
    from_actor: Option<&str>,
    to_actor: Option<&str>,
    in_reply_to_message_id: Option<&str>,
    references_chain: Option<&str>,
    tags: Option<&[String]>,
    attachments: &[khive_storage::NewAttachment],
    identity: Option<&crate::idempotency::MessageIdentity>,
) -> Result<MessageWrite, RuntimeError> {
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
                    receipt: Box::new(khive_runtime::DenialReceipt::not_audited()),
                    reason: format!(
                        "cross-namespace delivery to {recipient_ns_str:?} is not permitted; \
                         add {recipient_ns_str:?} to actor.allowed_outbound_namespaces in \
                         the sender's khive.toml to enable delivery"
                    ),
                });
            }
            // 3. Allowlist hit: fall through to outbound note creation.
        }
    }

    let mut external_admission_guard: Option<tokio::sync::OwnedMutexGuard<()>> = None;
    if let Some(recipient) = to_actor.and_then(|actor| actor.strip_prefix("email:")) {
        if let Some(identity) = identity {
            #[cfg(test)]
            crate::email_admission_tests::pause(
                crate::email_admission_tests::Boundary::BeforeHolderLookup,
            )
            .await;
            let holder = khive_runtime::keyed_message::find_keyed_message_holder(
                runtime,
                caller_token.namespace().as_str(),
                &identity.physical_key(caller_token),
            )
            .await?;
            #[cfg(test)]
            crate::email_admission_tests::pause(
                crate::email_admission_tests::Boundary::AfterHolderLookup,
            )
            .await;
            if let Some(holder) = holder {
                return Ok(MessageWrite {
                    outbound: identity
                        .replay(runtime, caller_token, holder, attachments)
                        .await?,
                    embedding_truncation: Default::default(),
                    replayed: true,
                });
            }
        }
        // For a denied keyed request, the lookup above is the final no-holder
        // observation. A later competing commit is available to the next retry.
        let policy = runtime.outbound_email_policy();
        if !policy.allows(recipient) {
            // A recipient that does not parse has no normalized form; name it as requested.
            let named = khive_types::email_address::normalize_email_recipient(recipient)
                .unwrap_or_else(|| recipient.to_string());
            if !policy.has_external_senders() {
                let reason = format!(
                    "email recipient '{named}' is not permitted by the configured outbound policy; \
                     no message was committed by this request"
                );
                return Err(RuntimeError::permission_denied(verb, reason));
            }
            if !policy.allows_external_sender(&caller_token.actor().id) {
                return Err(external_email_denied(
                    verb,
                    "caller is not an external sender",
                ));
            }
            external_admission_guard = Some(policy.lock_external_admission().await);
            if let Some(identity) = identity {
                let holder = khive_runtime::keyed_message::find_keyed_message_holder(
                    runtime,
                    caller_token.namespace().as_str(),
                    &identity.physical_key(caller_token),
                )
                .await?;
                if let Some(holder) = holder {
                    return Ok(MessageWrite {
                        outbound: identity
                            .replay(runtime, caller_token, holder, attachments)
                            .await?,
                        embedding_truncation: Default::default(),
                        replayed: true,
                    });
                }
            }
            let normalized = khive_types::email_address::normalize_email_recipient(recipient)
                .ok_or_else(|| external_email_denied(verb, "recipient address is invalid"))?;
            let suppressions = crate::handlers::load_outbound_email_suppressions(runtime)
                .await
                .map_err(|_| external_email_denied(verb, "suppression check is unavailable"))?;
            let recipient_key = policy.recipient_key(&normalized);
            if khive_runtime::outbound_email_is_suppressed(
                &normalized,
                recipient_key.as_deref(),
                &suppressions,
            ) {
                return Err(external_email_denied(verb, "recipient is suppressed"));
            }
            let cap = policy
                .external_daily_cap()
                .ok_or_else(|| external_email_denied(verb, "daily external cap is unavailable"))?;
            let recent_count = recent_external_outbound_count(runtime, policy)
                .await
                .map_err(|_| {
                    external_email_denied(verb, "daily external cap check is unavailable")
                })?;
            if recent_count >= cap {
                return Err(external_email_denied(verb, "daily external cap reached"));
            }
            // ADR-200 §1 condition 4: the external footer must be configured before a fresh
            // external message is admitted. A policy-recipient send never reaches this branch.
            if let Some(Err(reason)) = policy.external_footer() {
                return Err(external_email_denied(
                    verb,
                    &format!("external email footer is unavailable: {reason}"),
                ));
            }
            // ADR-200 §1 condition 5 and §6: with a remote suppression mirror configured, the
            // time of its last successful refresh is read from the local store (admission does
            // no network I/O), and a refresh older than the configured maximum age refuses, so
            // a long mirror outage stops new external messages from queueing behind it.
            if let Some(mirror) = policy.suppression_mirror() {
                let last_refresh = crate::handlers::suppression_mirror_last_refresh(runtime)
                    .await
                    .map_err(|_| {
                        external_email_denied(verb, "suppression mirror check is unavailable")
                    })?;
                let fresh = match last_refresh {
                    None => false,
                    Some(at) => match (chrono::Utc::now() - at).to_std() {
                        Ok(age) => age <= mirror.max_age(),
                        // A refresh stamped ahead of this clock is not older than the maximum.
                        Err(_) => true,
                    },
                };
                if !fresh {
                    return Err(external_email_denied(verb, "suppression mirror stale"));
                }
            }
        }
    }

    // Pre-generate the outbound id so the canonical thread_id is known before
    // any write is attempted — both copies carry it from their first write,
    // eliminating the separate thread_id-patch transaction the two-call
    // version needed for root sends.
    let outbound_id = Uuid::new_v4();
    let inbound_id = Uuid::new_v4();
    let canonical_thread_id: String = match thread_id {
        Some(tid) => tid.to_string(),
        None => outbound_id.as_hyphenated().to_string(),
    };

    let mut outbound_props = json!({
        "comm_schema_version": COMM_SCHEMA_VERSION,
        "from": from,
        "to": to,
        "direction": "outbound",
        "subject": subject,
        "thread_id": canonical_thread_id,
        "read": false,
        "sent_at": sent_at,
    });
    if let Some(fa) = from_actor {
        outbound_props["from_actor"] = json!(fa);
    }
    if let Some(ta) = to_actor {
        outbound_props["to_actor"] = json!(ta);
    }
    if let Some(process_ref) = sent_by_process {
        outbound_props["sent_by_process"] = json!(process_ref);
    }
    if let Some(irt) = in_reply_to_message_id {
        outbound_props["in_reply_to_message_id"] = json!(irt);
    }
    if let Some(refs) = references_chain {
        outbound_props["references_chain"] = json!(refs);
    }
    if let Some(t) = tags {
        if !t.is_empty() {
            outbound_props["tags"] = json!(t);
        }
    }

    // When actor labels are provided (ADR-057 actor-addressed path), both copies
    // land in the caller's namespace — no cross-namespace write occurs.
    // When sender and recipient are in different namespaces (allowed cross-ns path),
    // mint a recipient-scoped read+write token used for the inbound write after the
    // allowlist check so the inbound note lands in the correct inbox. For
    // same-namespace sends (from == to), use caller_token unchanged (preserves
    // existing behavior).
    let cross_ns_token;
    let inbound_tok: &NamespaceToken = if from_actor.is_some() || from == recipient_ns_str {
        // Actor-addressed path or same-namespace send: inbound copy stays in caller ns.
        caller_token
    } else {
        cross_ns_token = caller_token.with_namespace(
            Namespace::parse(recipient_ns_str).expect("recipient_ns_str already validated above"),
        );
        &cross_ns_token
    };

    let mut inbound_props = json!({
        "comm_schema_version": COMM_SCHEMA_VERSION,
        "from": from,
        "to": to,
        "direction": "inbound",
        "subject": subject,
        "thread_id": canonical_thread_id,
        "read": false,
        "sent_at": sent_at,
        "outbound_ref": outbound_id,
    });
    if let Some(fa) = from_actor {
        inbound_props["from_actor"] = json!(fa);
    }
    if let Some(ta) = to_actor {
        inbound_props["to_actor"] = json!(ta);
    }
    if let Some(process_ref) = sent_by_process {
        inbound_props["sent_by_process"] = json!(process_ref);
    }
    if let Some(irt) = in_reply_to_message_id {
        inbound_props["in_reply_to_message_id"] = json!(irt);
    }
    if let Some(refs) = references_chain {
        inbound_props["references_chain"] = json!(refs);
    }
    if let Some(t) = tags {
        if !t.is_empty() {
            inbound_props["tags"] = json!(t);
        }
    }

    if let Some(identity) = identity {
        outbound_props["idempotency_key"] = json!(identity.key);
        inbound_props["idempotency_key"] = json!(identity.key);
        outbound_props["inbound_ref"] = json!(inbound_id);
        outbound_props["idempotency_request"] = identity.request.clone();
    }
    let specs = [
        khive_runtime::AtomicNoteSpec {
            token: caller_token,
            id: Some(outbound_id),
            kind: "message",
            name: subject,
            content,
            properties: Some(outbound_props),
        },
        khive_runtime::AtomicNoteSpec {
            token: inbound_tok,
            id: Some(inbound_id),
            kind: "message",
            name: subject,
            content,
            properties: Some(inbound_props),
        },
    ];
    let (mut notes, embedding_truncation) = if let Some(identity) = identity {
        match khive_runtime::keyed_message::create_keyed_message_pair_with_attachments(
            runtime,
            specs,
            &identity.physical_key(caller_token),
            attachments,
        )
        .await
        .map_err(|error| attach_outbound_id_to_ambiguous_write(outbound_id, error))?
        {
            khive_runtime::keyed_message::KeyedMessageWrite::Created {
                notes,
                embedding_truncation,
            } => (notes, embedding_truncation),
            khive_runtime::keyed_message::KeyedMessageWrite::Existing(holder) => {
                return Ok(MessageWrite {
                    outbound: identity
                        .replay(runtime, caller_token, holder, attachments)
                        .await?,
                    embedding_truncation: Default::default(),
                    replayed: true,
                });
            }
        }
    } else {
        khive_runtime::atomic_message::create_notes_atomic_with_attachments(
            runtime,
            specs.into(),
            attachments,
        )
        .await
        .map_err(|error| attach_outbound_id_to_ambiguous_write(outbound_id, error))?
    };
    drop(external_admission_guard);
    Ok(MessageWrite {
        outbound: notes.remove(0),
        embedding_truncation,
        replayed: false,
    })
}

#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;
