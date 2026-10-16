//! The `comm.thread` read handler.

use super::*;

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
    let p: ThreadParams = deser(params.clone())?;
    let view = runtime.authorize_mailbox_view(
        token,
        "comm.thread",
        p.mailbox_actor.as_deref(),
        &params,
    )?;
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
    let passed_uuid = runtime
        .resolve_uuid_or_prefix_for_verb(token, &p.id, "thread")
        .await?;

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
    let mut physical_cursor = None;
    let mut seen_row_ids = HashSet::new();
    loop {
        let mut page_filter = thread_filter.clone();
        page_filter.after = physical_cursor;
        let page = thread_store
            .query_notes_filtered_count_free(
                token.namespace().as_str(),
                &page_filter,
                PageRequest {
                    limit: PAGE_SIZE,
                    offset: 0,
                },
            )
            .await?;
        let fetched = page.items.len() as u32;
        // Advance from the last physical row before mailbox filtering or
        // logical twin folding, as inbox does. The response limit stays late.
        physical_cursor = page
            .items
            .last()
            .map(|note| khive_storage::note::NoteSeekAfter {
                created_at: note.created_at,
                id: note.id,
            });
        #[cfg(test)]
        read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadPage(
            page.items.iter().map(|note| note.id).collect(),
        ))
        .await;
        #[cfg(test)]
        let rendered_before = rows.len();
        for n in &page.items {
            if seen_row_ids.insert(n.id) {
                rows.push(ThreadRow {
                    created_at: n.created_at,
                    full_id: n.id,
                    json: note_to_message_json(n),
                });
            }
        }
        #[cfg(test)]
        read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRendered(
            rows.len() - rendered_before,
        ))
        .await;
        #[cfg(test)]
        read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRetained {
            stage: "physical",
            rows: rows.len(),
        })
        .await;
        if fetched < PAGE_SIZE {
            break;
        }
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
        #[cfg(test)]
        read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRendered(1)).await;
    }
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRetained {
        stage: "with_selected_root",
        rows: rows.len(),
    })
    .await;

    // Exclude pool/malformed physical rows before pair deduplication and read
    // folding. Named own views retain participation only on addressed rows.
    rows.retain(|r| {
        let props = r.json.get("properties");
        let to_actor = props
            .and_then(|p| p.get("to_actor"))
            .and_then(Value::as_str);
        let from_actor = props
            .and_then(|p| p.get("from_actor"))
            .and_then(Value::as_str);
        if view.delegated {
            to_actor.is_some_and(|recipient| {
                is_valid_mailbox_actor_label(recipient)
                    && (from_actor == Some(view.actor_id.as_str())
                        || recipient == view.actor_id.as_str())
            })
        } else {
            addressed_recipient(props).is_some_and(|recipient| {
                from_actor == Some(view.actor_id.as_str()) || recipient == view.actor_id.as_str()
            }) || (caller_inherits_legacy_pool(token) && legacy_recipient(props))
        }
    });
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRetained {
        stage: "visible",
        rows: rows.len(),
    })
    .await;

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
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRetained {
        stage: "folded",
        rows: rows.len(),
    })
    .await;

    // #494: `after` cursor — message id or RFC 3339 timestamp; a hard error if
    // neither. See crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_thread
    let after_cursor: Option<AfterCursor> = match p.after.as_deref() {
        None => None,
        Some(raw) => {
            let looks_like_id = raw.parse::<Uuid>().is_ok()
                || (raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()));
            if looks_like_id {
                let cursor_uuid = runtime
                    .resolve_uuid_or_prefix_for_verb(token, raw, "thread")
                    .await?;
                let cursor_store = runtime.notes(token)?;
                let cursor_note = cursor_store
                    .get_note(cursor_uuid)
                    .await
                    .map_err(|e| RuntimeError::Internal(format!("thread: get_note (after): {e}")))?
                    .filter(|note| note.kind == "message")
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
                let micros = khive_runtime::rfc3339_to_utc_micros(raw).map_err(|e| {
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
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRetained {
        stage: "after_cursor",
        rows: rows.len(),
    })
    .await;

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
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadRetained {
        stage: "limited",
        rows: rows.len(),
    })
    .await;
    #[cfg(test)]
    read_cluster_tests::observe_phase(read_cluster_tests::Phase::ThreadOwners(
        rows.iter().map(|row| row.full_id).collect(),
    ))
    .await;
    crate::file_attachments::enrich_many(
        runtime,
        rows.iter_mut().map(|row| &mut row.json).collect(),
    )
    .await?;
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
