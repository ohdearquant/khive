//! Channel ingest and duplicate acknowledgement handlers.

#[cfg(test)]
use super::race_seam;
use super::{
    canonicalize_ingest_sent_at, canonicalize_thread_id, json, message_id_match_candidates,
    outbound_email_message, short_id, thread_id_query_spellings,
    verified_outbound_email_external_id, Attachment, AttachmentSubstrate, ContentRef,
    EmailMessageIdDomains, FilterOp, InboxSignal, IngestParams, KhiveRuntime, NamespaceToken,
    NewAttachment, Note, NoteFilter, PageRequest, PropertyFilter, RuntimeError, SqlValue, Utc,
    Uuid, Value, COMM_SCHEMA_VERSION, COMM_STABLE_PROPERTY_KEYS,
};

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
    capability: &khive_runtime::ChannelIngestCapability,
    token: &NamespaceToken,
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
            let repaired = runtime
                .try_repair_quarantined_note_retention(
                    capability,
                    token,
                    duplicate.id,
                    channel_kind,
                    channel_slug,
                    &attachment.content_ref,
                    replay_deadline,
                    Utc::now().timestamp_micros(),
                )
                .await?;
            if !repaired {
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
                capability,
                token,
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
                capability,
                token,
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
                capability,
                token,
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

pub(super) fn committed_ingest_degradations(error: &RuntimeError) -> Option<(Uuid, Value)> {
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
