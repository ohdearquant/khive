use super::*;

/// Persist `bridge` as v2 Vamana segments under `<db-file>.ann/<hex>/`.
///
/// Resolves the segment directory via `ann_segment_dir`. Returns `Ok(())` when the
/// backend is in-memory (no database file) — skipping persistence is not an error.
/// `save_atomic` binds the id-map sidecar to the commit-record digest internally;
/// callers do not need to supply a `CorpusFingerprint`.
#[cfg(test)]
pub(crate) fn persist_ann_v2(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    bridge: &AnnBridge,
) -> Result<(), String> {
    match ann_segment_dir(rt, ns, model) {
        Some(dir) => bridge.save_atomic(&dir),
        None => Ok(()), // in-memory backend — no filesystem, skip silently
    }
}

pub(super) fn persist_ann_v2_locked(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    bridge: &AnnBridge,
) -> Result<(), String> {
    match ann_segment_dir(rt, ns, model) {
        Some(dir) => bridge.save_atomic_locked(&dir),
        None => Ok(()),
    }
}

/// Stable, scope-bearing consumer identity for the knowledge atom index
/// (ADR-079 Amendment 1): pack name plus the corpus predicate's field value,
/// so the same predicate always maps to the same `ann_consumer_watermark`
/// row across restarts.
pub(super) const ANN_CONSUMER: &str = "knowledge:knowledge.atom";

/// Durably register this consumer's watermark row as pending (`-2`).
///
/// MUST run before the consumer persists or serves any extended-format
/// segment for the scope: pending blocks pair-wide compaction instead of
/// hiding this consumer from the registry `MIN`, but can be retired with a
/// warning if no first checkpoint ever activates it (ADR-079 Amendment 1 §A
/// step 1 and issue #1479).
#[cfg(test)]
pub(super) async fn register_consumer(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
) -> Result<(), String> {
    let sql = rt.sql();
    ann_registry::register_pending_dispatch(
        sql.as_ref(),
        "knowledge_",
        ANN_CONSUMER,
        ns,
        model,
        ann_segment_dir(rt, ns, model).is_none(),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Durable sentinel for registry-loss recovery.  `-1` is below every legal
/// sequence watermark, so it both blocks pair-wide compaction (`MIN = -1`)
/// and tells every process to reject its loaded/persisted bridge until one
/// authoritative full-corpus checkpoint raises the row to a normal `S >= 0`.
pub(super) async fn write_force_rebuild_sentinel_row(
    rt: &KhiveRuntime,
    key: &AnnKey,
) -> Result<(), String> {
    let sql = rt.sql();
    if ann_segment_dir(rt, &key.namespace, &key.model).is_none() {
        let mut writer = sql.writer().await.map_err(|error| error.to_string())?;
        writer
            .execute(ann_registry::pathless_mark_recovering(
                "knowledge_",
                ANN_CONSUMER,
                &key.namespace,
                &key.model,
            ))
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    ann_registry::mark_recovering(sql.as_ref(), ANN_CONSUMER, &key.namespace, &key.model)
        .await
        .map_err(|error| error.to_string())
}

pub(super) async fn write_force_rebuild_sentinel(
    rt: &KhiveRuntime,
    key: &AnnKey,
) -> Result<(), String> {
    // Serialize the sentinel with the complete bridge+sidecar publication and
    // watermark transition.  A checkpoint that began before registry loss
    // therefore either finishes before `-1` is published or observes `-1`
    // under this same lock and aborts without publishing.
    let _publication_lock = match ann_segment_dir(rt, &key.namespace, &key.model) {
        Some(dir) => Some(
            khive_retrieval::ann::acquire_checkpoint_lock_async(dir, BRIDGE_LOCK_MESSAGE_PREFIX)
                .await?,
        ),
        None => None,
    };
    // The detector may have waited behind a successful authoritative
    // checkpoint. Revalidate under the publication locks so that a delayed
    // request cannot demote the winner's normal row back to -1.
    if matches!(
        read_own_watermark(rt, &key.namespace, &key.model).await?,
        Some(watermark) if watermark >= 0
    ) {
        return Ok(());
    }
    write_force_rebuild_sentinel_row(rt, key).await
}

/// Establish the cross-process precondition for one authoritative rebuild
/// after consumer-registry loss.  The local marker is deliberately set
/// before the first await; the transactional SQL upsert then publishes the
/// same state to every process before this consumer can be treated as
/// registered again.
pub(super) async fn prepare_authoritative_rebuild(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> Result<(), String> {
    mark_force_rebuild(ann, key);
    let local_publication_lock = checkpoint_lock(ann, key);
    let _local_publication_guard = local_publication_lock.lock().await;
    write_force_rebuild_sentinel(rt, key).await
}

/// Read this consumer's own registry watermark. `None` means decision-rule-4
/// registry loss; knowledge publishes `-1` before its authoritative rebuild.
pub(super) async fn read_own_watermark(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
) -> Result<Option<i64>, String> {
    let sql = rt.sql();
    // This statement has always been labelled without a pack prefix.
    ann_registry::read_watermark(sql.as_ref(), "", ANN_CONSUMER, ns, model)
        .await
        .map_err(|e| e.to_string())
}

/// Raise this consumer's registered watermark monotonically after a durable
/// segment commit at `s` (ADR-079 Amendment 1 §A step 2). A crash before this
/// leaves the smaller watermark — under-compacts, never over-compacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckpointAuthority {
    Incremental,
    FullRegistered,
    FullSentinel,
}

/// Capture the durable registry state immediately before a full corpus scan.
/// An absent row is published as the cross-process sentinel rather than a
/// plain registration: the scan is authoritative and can safely clear it,
/// while peers must remain Cold for the entire scan window.
pub(crate) async fn prepare_full_corpus_scan(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> Result<CheckpointAuthority, String> {
    match read_own_watermark(rt, &key.namespace, &key.model).await? {
        Some(watermark) if watermark >= 0 => Ok(CheckpointAuthority::FullRegistered),
        Some(watermark) if watermark == ann_registry::RECOVERING_WATERMARK => {
            mark_force_rebuild(ann, key);
            Ok(CheckpointAuthority::FullSentinel)
        }
        Some(_) | None => {
            // Pending (`-2`) is a bounded first-checkpoint grace state, not
            // the authoritative recovery fence this scan is allowed to
            // clear. Direct full-reindex callers do not pass through the
            // ordinary ensure path, so promote it to durable `-1` here
            // before scanning rather than failing their first publication.
            prepare_authoritative_rebuild(rt, ann, key).await?;
            Ok(CheckpointAuthority::FullSentinel)
        }
    }
}

pub(super) async fn raise_watermark(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
    authority: CheckpointAuthority,
) -> Result<(), String> {
    let shared_authority = match authority {
        CheckpointAuthority::FullSentinel => WatermarkAuthority::Recovering,
        CheckpointAuthority::Incremental => WatermarkAuthority::Active,
        CheckpointAuthority::FullRegistered => WatermarkAuthority::PendingOrActive,
    };
    let sql = rt.sql();
    let pathless = ann_segment_dir(rt, ns, model).is_none();
    if pathless {
        i64::try_from(s)
            .map_err(|_| format!("knowledge ANN watermark {s} exceeds SQLite INTEGER range"))?;
    }
    let raised = ann_registry::raise_watermark_dispatch(
        sql.as_ref(),
        "knowledge_",
        ANN_CONSUMER,
        ns,
        model,
        s,
        shared_authority,
        pathless,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !raised {
        return Err(format!(
            "ANN watermark publication fence rejected {authority:?}: affected 0 rows"
        ));
    }
    Ok(())
}

pub(super) fn knowledge_corpus(ns: &str) -> CorpusScope<'_> {
    CorpusScope {
        namespace: Some(ns),
        record_kind: None,
        field: "knowledge.atom",
        live_join: None,
        watermark_capture: WatermarkCapture::LogHighWater,
    }
}

/// Compact the write log through the pair-wide registry minimum ONLY (ADR-079
/// Amendment 1 §A step 3, universal wildcard-inclusive form). Wildcard rows
/// (`namespace = '*'`) are global-scope consumers whose corpus spans every
/// namespace; their watermark bounds this pair's compaction too. The scalar
/// subquery yields NULL when no consumer has registered, and `seq <= NULL`
/// matches nothing — an unregistered pair never compacts.
pub(super) async fn compact_log(rt: &KhiveRuntime, ns: &str, model: &str) -> Result<(), String> {
    let sql = rt.sql();
    ann_registry::compact_dispatch(
        sql.as_ref(),
        "knowledge_",
        knowledge_corpus(ns).compaction_scope(),
        model,
        ann_segment_dir(rt, ns, model).is_none(),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Whether any tail row exists above `s` for this consumer's scope. A pure
/// `ann_write_log` index probe (`idx_ann_write_log_ns_model_seq`) — never
/// touches the vec0 corpus, which is what keeps Hot classification free of
/// corpus IO (the amendment's rule 5/6 evaluation-order note).
pub(super) async fn tail_exists(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
) -> Result<bool, String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(knowledge_corpus(ns).tail_exists(model, s, "ann_tail_probe"))
        .await
        .map_err(|e| e.to_string())?;
    match rows.first().and_then(|r| r.get("has_tail")) {
        Some(SqlValue::Integer(n)) => Ok(*n != 0),
        other => Err(format!("tail probe: unexpected value {other:?}")),
    }
}

/// Live corpus count and tail count for this consumer's scope, captured in ONE
/// statement so both come from the same SQLite read snapshot (the decision
/// table requires the live count and the tail to describe one state).
pub(super) async fn scope_counts(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
) -> Result<(u64, u64), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(knowledge_corpus(ns).scope_counts(&table_name, model, s, "ann_scope_counts"))
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

/// Classification needs the exact *decision*, not the full live count when
/// the tail is small. This statement counts log rows and reads at most `cap`
/// scope-matching vec0 rows in the same SQLite snapshot. If the cap is reached,
/// it is a proven lower bound high enough for Stale-tail; otherwise the count
/// is exact (including zero, which must classify Empty). A tail of one needs
/// only a scope-existence probe. Extreme cap arithmetic falls back to an
/// unlimited, exact count inside this same statement.
pub(super) struct ClassificationScopeCounts {
    pub(super) live_lower_bound: u64,
    pub(super) tail: u64,
    pub(super) live_count_exact: bool,
}

pub(super) async fn classification_scope_counts(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
    threshold: f64,
) -> Result<ClassificationScopeCounts, String> {
    // For tail > 1, cap >= 2 * tail / threshold guarantees that
    // ceil(threshold * cap) >= tail despite floating-point rounding. A
    // multiplier that cannot fit in SQLite INTEGER instead selects the
    // unbounded exact count. The tail=1 case needs just one live row.
    let multiplier = (2.0 / threshold).ceil();
    let multiplier = if multiplier.is_finite() && multiplier < i64::MAX as f64 {
        SqlValue::Integer(multiplier as i64)
    } else {
        SqlValue::Null
    };
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(knowledge_corpus(ns).classification_scope_counts(
            &table_name,
            model,
            s,
            multiplier,
            "ann_classification_scope_counts",
        ))
        .await
        .map_err(|e| e.to_string())?;
    let row = rows
        .into_iter()
        .next()
        .ok_or("classification_scope_counts returned no row")?;
    let get = |col: &str| match row.get(col) {
        Some(SqlValue::Integer(n)) => u64::try_from(*n).map_err(|_| format!("negative {col}")),
        other => Err(format!(
            "classification_scope_counts {col}: unexpected value {other:?}"
        )),
    };
    let live_lower_bound = get("live")?;
    let tail = get("tail")?;
    let cap = match row.get("cap") {
        Some(SqlValue::Integer(n)) => *n,
        other => return Err(format!("classification_scope_counts cap: {other:?}")),
    };
    Ok(ClassificationScopeCounts {
        live_lower_bound,
        tail,
        live_count_exact: cap < 0 || live_lower_bound < cap as u64,
    })
}
