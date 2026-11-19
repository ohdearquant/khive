use super::*;

/// Stable, scope-bearing consumer identity for the memory note index
/// (ADR-079 Amendment 1, global-scope addendum): pack name plus the corpus
/// predicate's field value.
pub(super) const ANN_CONSUMER: &str = "memory-notes:note.content";
/// The same global note-content graph has a second ADR-118 consumer with its
/// own durable protection; it never borrows the memory consumer's row.
pub(crate) const NOTE_SEARCH_CONSUMER: &str = "note_search";

/// Registry namespace for a global-scope consumer (one row per model spanning
/// every namespace). `'*'` is not a valid `Namespace` value, so wildcard rows
/// cannot collide with per-namespace rows.
pub(super) const ANN_WILDCARD_NS: &str = "*";

/// Durably register this consumer's wildcard row as pending (`-2`). MUST run
/// before the first full scan, persist, or serve: pending blocks compaction in
/// every namespace but, unlike an active checkpoint at `S = 0`, can be retired
/// with a visible warning if it never activates.
pub(super) async fn register_consumer(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    register_consumer_identity(rt, model, ANN_CONSUMER).await
}

pub(super) async fn register_consumer_identity(
    rt: &KhiveRuntime,
    model: &str,
    consumer: &str,
) -> Result<(), String> {
    let sql = rt.sql();
    ann_registry::register_pending_dispatch(
        sql.as_ref(),
        "memory_",
        consumer,
        ANN_WILDCARD_NS,
        model,
        ann_segment_dir(rt, model).is_none(),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Read this consumer's own wildcard registry watermark. `None` = no row
/// (decision rule 4: Cold after re-registering as pending).
pub(super) async fn read_own_watermark(
    rt: &KhiveRuntime,
    model: &str,
) -> Result<Option<i64>, String> {
    read_consumer_watermark(rt, model, ANN_CONSUMER).await
}

pub(crate) async fn read_note_search_watermark(
    rt: &KhiveRuntime,
    model: &str,
) -> Result<Option<i64>, String> {
    read_consumer_watermark(rt, model, NOTE_SEARCH_CONSUMER).await
}

pub(super) async fn note_search_requires_full_rebuild(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
) -> bool {
    if !note_search_consumer_enabled(ann) {
        return false;
    }
    match read_note_search_watermark(rt, model).await {
        Ok(Some(watermark)) if watermark >= 0 => false,
        Ok(Some(_)) => true,
        Ok(None) => {
            if let Err(error) = register_consumer_identity(rt, model, NOTE_SEARCH_CONSUMER).await {
                tracing::warn!(%error, model, "note-search ANN registration failed");
            }
            true
        }
        Err(error) => {
            tracing::warn!(%error, model, "note-search ANN registry read failed");
            false // Memory recall's existing consumer remains independently valid.
        }
    }
}

pub(super) async fn advance_note_search_watermark(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
    s: u64,
    authority: WatermarkAuthority,
) {
    if !note_search_consumer_enabled(ann) {
        return;
    }
    // An incremental checkpoint cannot activate a pending consumer: only a
    // full-corpus checkpoint under PendingOrActive may do that.
    if authority == WatermarkAuthority::Active
        && !matches!(read_note_search_watermark(rt, model).await, Ok(Some(n)) if n >= 0)
    {
        return;
    }
    if let Err(error) =
        raise_consumer_watermark_with_authority(rt, model, NOTE_SEARCH_CONSUMER, s, authority).await
    {
        tracing::warn!(%error, model, "note-search ANN watermark remains unavailable");
    }
}

pub(super) async fn read_consumer_watermark(
    rt: &KhiveRuntime,
    model: &str,
    consumer: &str,
) -> Result<Option<i64>, String> {
    let sql = rt.sql();
    ann_registry::read_watermark(sql.as_ref(), "memory_", consumer, ANN_WILDCARD_NS, model)
        .await
        .map_err(|e| e.to_string())
}

/// Conditionally raise this consumer's registered watermark after a durable
/// segment commit at `s`. Retirement or a newer publisher causes a fenced
/// failure rather than installing state whose tail is no longer protected.
pub(super) async fn raise_watermark_with_authority(
    rt: &KhiveRuntime,
    model: &str,
    s: u64,
    authority: WatermarkAuthority,
) -> Result<(), String> {
    raise_consumer_watermark_with_authority(rt, model, ANN_CONSUMER, s, authority).await
}

pub(super) async fn raise_consumer_watermark_with_authority(
    rt: &KhiveRuntime,
    model: &str,
    consumer: &str,
    s: u64,
    authority: WatermarkAuthority,
) -> Result<(), String> {
    let sql = rt.sql();
    let pathless = ann_segment_dir(rt, model).is_none();
    if pathless {
        i64::try_from(s)
            .map_err(|_| format!("memory ANN watermark {s} exceeds SQLite INTEGER range"))?;
    }
    let raised = ann_registry::raise_watermark_dispatch(
        sql.as_ref(),
        "memory_",
        consumer,
        ANN_WILDCARD_NS,
        model,
        s,
        authority,
        pathless,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !raised {
        return Err(format!(
            "{consumer} ANN watermark publication fence rejected {authority:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(super) async fn raise_watermark(rt: &KhiveRuntime, model: &str, s: u64) -> Result<(), String> {
    raise_watermark_with_authority(rt, model, s, WatermarkAuthority::Active).await
}

pub(super) fn memory_corpus() -> CorpusScope<'static> {
    CorpusScope {
        namespace: None,
        record_kind: Some("note"),
        field: "note.content",
        live_join: Some(LiveRowJoin::Notes),
        watermark_capture: WatermarkCapture::ScopedMaximumWithOwnFloor {
            consumer: ANN_CONSUMER,
            registry_namespace: ANN_WILDCARD_NS,
        },
    }
}

/// Compact the write log for `model` across every namespace, each bounded by
/// its own wildcard-inclusive registry minimum (ADR-079 Amendment 1 §A step
/// 3). A namespace with no registered rows yields `seq <= NULL`, which
/// matches nothing.
pub(super) async fn compact_log(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    let sql = rt.sql();
    ann_registry::compact_dispatch(
        sql.as_ref(),
        "memory_",
        memory_corpus().compaction_scope(),
        model,
        ann_segment_dir(rt, model).is_none(),
    )
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) async fn compact_log_for_test(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    compact_log(rt, model).await
}

/// Live corpus count and tail count for this consumer's scope, captured in ONE
/// statement so both come from the same SQLite read snapshot. Live is the
/// join-filtered global corpus; the tail spans every namespace under the same
/// kind/field predicate as the corpus scan.
pub(super) async fn scope_counts(
    rt: &KhiveRuntime,
    model: &str,
    s: u64,
) -> Result<(u64, u64), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(memory_corpus().scope_counts(&table_name, model, s, "memory_ann_scope_counts"))
        .await
        .map_err(|e| e.to_string())?;
    let row = rows
        .into_iter()
        .next()
        .ok_or("scope_counts returned no row")?;
    let get = |col: &str| match row.get(col) {
        Some(SqlValue::Integer(n)) => u64::try_from(*n).map_err(|_| format!("negative {col}")),
        other => Err(format!("scope_counts {col}: unexpected value {other:?}")),
    };
    Ok((get("live")?, get("tail")?))
}

/// Log-table-only probe: does any write-log row exist above `s`? Touches no
/// corpus table, so the Hot path (empty tail) adopts with zero corpus I/O
/// (ADR-079 Amendment 1 restart classifier, rule 6; see `docs/ann.md`).
pub(super) async fn tail_exists(rt: &KhiveRuntime, model: &str, s: u64) -> Result<bool, String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(memory_corpus().tail_exists(model, s, "memory_ann_tail_exists"))
        .await
        .map_err(|e| e.to_string())?;
    match rows.first().and_then(|row| row.get("has_tail")) {
        Some(SqlValue::Integer(n)) => Ok(*n != 0),
        other => Err(format!("tail_exists: unexpected value {other:?}")),
    }
}

/// Fetch the scope's tail (rows above `s`, all namespaces, ordered), coalesce
/// to the final op per subject, and hydrate embeddings for final upserts. A
/// missing vector row is a log/corpus contradiction (`Err` → Cold); a present
/// row whose note fails the join predicate replays as a delete (see
/// `docs/ann.md` for the full replay-outcome table). One statement is the
/// snapshot boundary, including for pool-backed readers. Returns the ops and
/// the new watermark.
pub(crate) async fn fetch_final_tail(
    rt: &KhiveRuntime,
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> Result<(Vec<(Uuid, Option<Vec<f32>>)>, u64), String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    fetch_final_tail_on(reader.as_mut(), model, s, live_threshold).await
}

/// Read registry protection, capped raw delta size, and final vector state in
/// one statement. The count is exact when it is within `max_delta`; above the
/// cap, `max_delta + 1` rows suffice to require a rebuild without materializing
/// the remaining suffix. Pathless SQLite readers share the writer connection,
/// so keeping an explicit read transaction across several async reads would
/// retain the only pooled connection while the task is suspended.
pub(super) async fn fetch_protected_tail_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
    s: u64,
    max_delta: u64,
) -> Result<(Option<(Vec<(Uuid, Option<Vec<f32>>)>, u64, u64)>, u64), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let minimum = ann_registry::min_watermark_subquery(3, 1);
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "WITH tail AS MATERIALIZED (\
                   SELECT seq, subject_id, op FROM ann_write_log \
                   WHERE embedding_model = ?1 \
                     AND kind = 'note' AND field = 'note.content' AND seq > ?2 \
                   ORDER BY seq LIMIT ?5\
                 ), summary AS MATERIALIZED (\
                   SELECT COUNT(*) AS raw_count, \
                          ({minimum}) AS min_watermark \
                   FROM tail\
                 ), selected AS MATERIALIZED (\
                   SELECT seq, subject_id, op FROM tail \
                   WHERE (SELECT raw_count FROM summary) <= ?4\
                 ) \
                 SELECT 0 AS is_summary, summary.raw_count, summary.min_watermark, \
                        NULL AS seq, NULL AS subject_id, NULL AS op, \
                        NULL AS vector_model, NULL AS vector_kind, NULL AS vector_field, \
                        NULL AS embedding, NULL AS live_note_id \
                 FROM summary \
                 UNION ALL \
                 SELECT 1 AS is_summary, NULL AS raw_count, NULL AS min_watermark, \
                        selected.seq, selected.subject_id, selected.op, \
                        vectors.embedding_model AS vector_model, \
                        vectors.kind AS vector_kind, vectors.field AS vector_field, \
                        vectors.embedding, live_note.id AS live_note_id \
                 FROM selected \
                 LEFT JOIN {table_name} AS vectors \
                   ON vectors.subject_id = selected.subject_id \
                 LEFT JOIN notes AS live_note \
                   ON live_note.id = selected.subject_id \
                  AND live_note.deleted_at IS NULL \
                 ORDER BY is_summary, seq"
            ),
            params: vec![
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
                SqlValue::Text(ANN_WILDCARD_NS.to_owned()),
                SqlValue::Integer(max_delta.min(i64::MAX as u64) as i64),
                SqlValue::Integer(max_delta.saturating_add(1).min(i64::MAX as u64) as i64),
            ],
            label: Some("memory_ann_incremental_protected_tail".into()),
        })
        .await
        .map_err(|e| e.to_string())?;

    let summary = rows
        .first()
        .filter(|row| matches!(row.get("is_summary"), Some(SqlValue::Integer(0))))
        .ok_or_else(|| "incremental tail summary is missing".to_owned())?;
    let raw_count = match summary.get("raw_count") {
        Some(SqlValue::Integer(n)) if *n >= 0 => *n as u64,
        _ => return Err("incremental tail count is invalid".into()),
    };
    if let Some(SqlValue::Integer(minimum)) = summary.get("min_watermark") {
        if u64::try_from(*minimum).is_ok_and(|minimum| minimum > s) {
            return Err("installed watermark is behind compacted history".into());
        }
    } else if !matches!(summary.get("min_watermark"), Some(SqlValue::Null)) {
        return Err("incremental tail registry minimum is invalid".into());
    }
    if raw_count > max_delta {
        return Ok((None, raw_count));
    }

    let tail_rows = &rows[1..];
    let (ops, end) = parse_final_tail_rows(tail_rows, model, s)?;
    Ok((Some((ops, end, raw_count)), raw_count))
}

/// Final op per subject, in sequence order, plus the last applied sequence.
pub(super) type FinalTail = (Vec<(Uuid, Option<Vec<f32>>)>, u64);

pub(super) fn parse_final_tail_rows(
    rows: &[khive_storage::types::SqlRow],
    model: &str,
    s: u64,
) -> Result<FinalTail, String> {
    let mut new_s = s;
    type RawVector = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
    );
    // Ordered iteration + insert-overwrite = final op per subject wins.
    let mut finals: Vec<(Uuid, bool, RawVector, bool)> = Vec::new();
    let mut index_of: HashMap<Uuid, usize> = HashMap::new();
    for row in rows {
        let seq = match row.get("seq") {
            Some(SqlValue::Integer(n)) => *n,
            _ => return Err("ann_write_log.seq: unexpected value".into()),
        };
        new_s = new_s.max(u64::try_from(seq).map_err(|_| "negative seq")?);
        let uuid = match row.get("subject_id") {
            Some(SqlValue::Text(t)) => {
                Uuid::parse_str(t).map_err(|e| format!("tail subject_id {t}: {e}"))?
            }
            _ => return Err("ann_write_log.subject_id: unexpected value".into()),
        };
        let is_delete = match row.get("op") {
            Some(SqlValue::Text(t)) => t == "delete",
            _ => return Err("ann_write_log.op: unexpected value".into()),
        };
        let raw_vector = (
            match row.get("vector_model") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("vector_kind") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("vector_field") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("embedding") {
                Some(SqlValue::Blob(value)) => Some(value.clone()),
                _ => None,
            },
        );
        let is_live = matches!(
            row.get("live_note_id"),
            Some(SqlValue::Text(id)) if Uuid::parse_str(id).ok() == Some(uuid)
        );
        match index_of.get(&uuid) {
            Some(&i) => finals[i] = (uuid, is_delete, raw_vector, is_live),
            None => {
                index_of.insert(uuid, finals.len());
                finals.push((uuid, is_delete, raw_vector, is_live));
            }
        }
    }

    let mut ops: Vec<(Uuid, Option<Vec<f32>>)> = Vec::with_capacity(finals.len());
    for (subject, is_delete, (vector_model, vector_kind, vector_field, embedding), is_live) in
        finals
    {
        if is_delete {
            ops.push((subject, None));
            continue;
        }
        // A vec row that is absent or outside this consumer's scope
        // contradicts the committed upsert. The LEFT JOIN keeps that absence
        // visible to this check instead of silently dropping the log row.
        if vector_model.is_none()
            && vector_kind.is_none()
            && vector_field.is_none()
            && embedding.is_none()
        {
            return Err(format!(
                "tail upsert {subject}: vector row absent (log/corpus contradiction)"
            ));
        }
        let in_scope = vector_model.as_deref() == Some(model)
            && vector_kind.as_deref() == Some("note")
            && vector_field.as_deref() == Some("note.content");
        if !in_scope {
            return Err(format!(
                "tail upsert {subject}: vector row out of consumer scope (log/corpus contradiction)"
            ));
        }
        let Some(bytes) = embedding else {
            return Err(format!("tail upsert {subject}: embedding is not a blob"));
        };
        // Preserve this adapter's existing complete-chunk policy.
        let vector =
            khive_storage::decode_f32_native(&bytes[..bytes.len() / 4 * 4]).unwrap_or_default();
        if is_live {
            ops.push((subject, Some(vector)));
        } else {
            // A vector whose note is soft-deleted or gone is a valid
            // join-filtered absence, represented as a delete in the replay.
            ops.push((subject, None));
        }
    }
    Ok((ops, new_s))
}
