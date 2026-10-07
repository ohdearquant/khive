use super::{
    deser, json, validate_actor_label, CursorCommitParams, CursorGetParams, DateTime, KhiveRuntime,
    NamespaceToken, ProbeMessage, ProbeParams, ProbeResponse, RuntimeError, SqlValue, Utc, Value,
};

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
