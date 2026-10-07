use super::*;

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

pub(super) async fn wait_for_inbox_response<Query, QueryFuture>(
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
