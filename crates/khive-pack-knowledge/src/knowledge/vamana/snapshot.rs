use super::*;

/// Persist `bridge` at its applied watermark, reopen and publish the mmap
/// segment, then raise this consumer's registry row and compact through the
/// pair MIN. Publishing before the durable raise makes the ADR-118 mismatch
/// window empty for this process; a crash before the raise merely
/// under-compacts. Registration still precedes persistence (ADR-079 Amendment
/// 1 §A step 1). On persist/reopen failure the Owned bridge is installed.
pub(crate) async fn checkpoint_raise_compact_readopt(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    bridge: AnnBridge,
    generation: u64,
    authority: CheckpointAuthority,
) -> bool {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    let local_publication_lock = checkpoint_lock(ann, key);
    let _local_publication_guard = local_publication_lock.lock().await;

    // This lock spans the complete knowledge-level publication: Vamana
    // segments, UUID sidecar, mmap verification, and the durable registry
    // transition.  The sentinel writer takes the same lock, so an ordinary
    // replay checkpoint that predates registry loss cannot publish after -1.
    let publication_lock = match ann_segment_dir(rt, ns, model) {
        Some(dir) => match khive_retrieval::ann::acquire_checkpoint_lock_async(
            dir,
            BRIDGE_LOCK_MESSAGE_PREFIX,
        )
        .await
        {
            Ok(lock) => Some(lock),
            Err(error) => {
                tracing::warn!(error = %error, "failed to acquire ANN checkpoint lock");
                return false;
            }
        },
        None => None,
    };

    let applied = bridge.index.last_applied_seq().unwrap_or(0);
    let own_watermark = match read_own_watermark(rt, ns, model).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, "ann checkpoint registry read failed");
            return false;
        }
    };
    if authority == CheckpointAuthority::FullSentinel {
        if let Some(observed) = own_watermark.filter(|watermark| *watermark >= 0) {
            tracing::info!(
                key = ?key,
                observed_watermark = observed,
                "authoritative ANN rebuild lost publication race; adopting winner"
            );
            let observed = u64::try_from(observed).unwrap_or(0);
            return adopt_checkpoint_winner(rt, ann, key, generation, observed).await;
        }
    } else if let Some(observed) = own_watermark.filter(|watermark| *watermark >= 0) {
        let observed = u64::try_from(observed).unwrap_or(0);
        if observed > applied {
            tracing::info!(
                key = ?key,
                candidate_watermark = applied,
                observed_watermark = observed,
                "stale ANN checkpoint lost publication race; adopting winner"
            );
            return adopt_checkpoint_winner(rt, ann, key, generation, observed).await;
        }
    }
    let authorized = match authority {
        CheckpointAuthority::FullSentinel => own_watermark == Some(-1),
        CheckpointAuthority::Incremental | CheckpointAuthority::FullRegistered => {
            own_watermark.is_some_and(|watermark| watermark >= 0)
        }
    };
    if !authorized {
        mark_force_rebuild(ann, key);
        if let Err(error) = write_force_rebuild_sentinel_row(rt, key).await {
            tracing::warn!(error = %error, "failed to fence unauthorized ANN checkpoint");
        }
        return false;
    }

    if let Err(e) = persist_ann_v2_locked(rt, ns, model, &bridge) {
        tracing::error!(error = %e, "failed to persist v2 Vamana segment");
        install_replacing(ann, key, bridge.with_generation(generation)).await;
        return false;
    }
    let published = match ann_segment_dir(rt, ns, model) {
        Some(dir) => match AnnBridge::load(&dir) {
            Ok(mmap_bridge) => mmap_bridge,
            Err(e) => {
                tracing::warn!(error = %e, "mmap re-adoption failed; serving Owned build");
                bridge
            }
        },
        None => bridge,
    };
    // File-backed publication must replace the in-process bridge before the
    // durable raise, closing the same-process mismatch window. An in-memory
    // runtime has no cross-process segment or compaction peer, so defer its
    // install until the conditional raise succeeds; a losing concurrent full
    // scan then cannot overwrite the winner before discovering the lost race.
    let file_backed = publication_lock.is_some();
    let mut pending_in_memory = Some(published.with_generation(generation));
    let mut installed = false;
    if file_backed {
        installed = install_replacing(
            ann,
            key,
            pending_in_memory.take().expect("published bridge"),
        )
        .await;
    }

    // The durable segment already covers `applied`, and this process now
    // serves that same state. A failed raise only retains extra log rows.
    if let Err(e) = raise_watermark(rt, ns, model, applied, authority).await {
        tracing::warn!(error = %e, "ann watermark raise failed (under-compacts; safe)");
        match read_own_watermark(rt, ns, model).await {
            Ok(Some(observed)) if observed >= 0 => {
                let observed = u64::try_from(observed).unwrap_or(0);
                if observed > applied {
                    return adopt_checkpoint_winner(rt, ann, key, generation, observed).await;
                }
                // Either our conditional transition committed despite an
                // uncertain client result, or the durable row remains behind
                // this candidate. Both are safe under-compaction states.
                if let Some(published) = pending_in_memory.take() {
                    installed = install_replacing(ann, key, published).await;
                }
                clear_force_rebuild(ann, key);
                return installed || has_current_index_at_watermark(ann, key, observed).await;
            }
            Ok(_) => {}
            Err(read_error) => {
                tracing::warn!(
                    error = %read_error,
                    "failed to reconcile ANN checkpoint race"
                );
            }
        }
        mark_force_rebuild(ann, key);
        if let Err(error) = write_force_rebuild_sentinel_row(rt, key).await {
            tracing::warn!(error = %error, "failed to restore ANN checkpoint fence");
        }
        return false;
    }
    if let Some(published) = pending_in_memory {
        installed = install_replacing(ann, key, published).await;
    }
    if authority != CheckpointAuthority::Incremental {
        clear_force_rebuild_if_current(ann, key, generation);
    }
    drop(publication_lock);
    if let Err(e) = compact_log(rt, ns, model).await {
        tracing::warn!(error = %e, "ann log compaction failed (retries next checkpoint)");
    }
    installed
}

/// Try to load a Vamana snapshot for `namespace`+`model` from `retrieval_snapshots`.
///
/// Returns `Ok(None)` when the table is absent, the row is missing, or
/// deserialization fails — all of which are treated as cache-miss signals.
pub(super) async fn try_load_snapshot(
    rt: &KhiveRuntime,
    namespace: &str,
    model: &str,
) -> Option<VamanaSnapshot> {
    let key = snapshot_key(namespace, model);
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: ::khive_runtime::sql!("knowledge_legacy_snapshot_load").into(),
            params: vec![SqlValue::Text(key), SqlValue::Text("vamana".into())],
            label: None,
        })
        .await
        .ok()?;

    let row = rows.into_iter().next()?;
    let blob = match row.get("snapshot")? {
        SqlValue::Blob(b) => b.clone(),
        _ => return None,
    };
    serde_json::from_slice::<VamanaSnapshot>(&blob).ok()
}

/// Get the corpus fingerprint by querying the vector store.
pub(crate) async fn compute_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let info = store.info().await.ok()?;
    Some(CorpusFingerprint {
        vector_count: info.entry_count,
        dimensions: info.dimensions as u32,
    })
}

/// Scan the sqlite-vec corpus for `model` and return raw (un-normalized) flat
/// vectors alongside the ordered UUID id-map.
///
/// Rows are fetched `ORDER BY subject_id` so the mapping is deterministic.
/// Returns `Ok(None)` only when a scan COMPLETED and found nothing: the table
/// is empty or no rows pass the byte-length validity check. Store-opening
/// failures propagate as `Err` — `Ok(None)` feeds the terminal unavailable
/// marker (issue #1026), so an operational error must never masquerade as a
/// verified empty corpus. The caller derives `dims` as
/// `flat.len() / id_map.len()`.
pub(super) async fn scan_corpus_raw(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<(Vec<f32>, Vec<Uuid>, u64)>, RuntimeError> {
    let store = rt
        .vectors_for_model(token, model)
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    let info = store
        .info()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    let count = info.entry_count;
    let dims = info.dimensions;

    if count == 0 || dims == 0 {
        return Ok(None);
    }

    let ns = token.namespace().as_str().to_owned();
    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");
    let model_str = model.to_owned();

    let sql = rt.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    // The global AUTOINCREMENT high-water evaluates inside the SAME statement
    // — and therefore the same SQLite read snapshot — as the corpus rows.
    // Unlike MAX over retained rows it survives compaction, so an
    // authoritative recovery checkpoint never regresses below the untrusted
    // segment it replaces. Future writes have a strictly larger global seq;
    // rows from sibling scopes below S are irrelevant to this corpus.
    let rows = reader
        .query_all(knowledge_corpus(&ns).corpus_scan(&table_name, &model_str, "vamana_corpus_scan"))
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut id_map: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut flat: Vec<f32> = Vec::with_capacity(rows.len() * dims);
    let scan_watermark = rows
        .first()
        .and_then(|row| row.i64_or_none("log_s"))
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0);

    for row in &rows {
        let id_str = match row.get("subject_id") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let uuid = match Uuid::parse_str(id_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(b)) => b.as_slice(),
            _ => continue,
        };
        if bytes.len() != dims * 4 {
            continue;
        }
        let Ok(vec) = khive_storage::decode_f32_native(bytes) else {
            continue;
        };
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }

    if id_map.is_empty() {
        return Ok(None);
    }

    Ok(Some((flat, id_map, scan_watermark)))
}

/// Scan the sqlite-vec table and build a fresh `AnnBridge`.
///
/// Returns `None` when there are no vectors or the model is not configured.
pub(crate) async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
    let Some((flat, id_map, scan_watermark)) = scan_corpus_raw(rt, token, model).await? else {
        return Ok(None);
    };
    let dims = flat.len() / id_map.len();
    // This async task builds inline. The ignored measure_inline_ann_build_tick_gap
    // (docs/benchmarks.md) measured scan/load + build at ~2 s for 4,096 rows
    // on 2 workers: the 10 ms ticker's largest gap was 10.37 ms versus a
    // 10.47 ms baseline, with no extra observed stall. A single worker would
    // be occupied during the inline CPU build; revisit if the runtime or
    // workload changes.
    AnnBridge::build(flat, dims, id_map)
        .map(|mut bridge| {
            bridge.set_applied_seq(scan_watermark);
            Some(bridge)
        })
        .map_err(RuntimeError::Internal)
}

/// Delete all Vamana snapshots for `namespace` from `retrieval_snapshots`.
///
/// Called after any vector-corpus mutation to guarantee `ensure_ann_for_model` cannot
/// load a snapshot that no longer matches the live corpus.  Best-effort: if
/// the `retrieval_snapshots` table doesn't exist yet, the call is a no-op.
pub(crate) async fn invalidate_snapshot(rt: &KhiveRuntime, namespace: &str) {
    let sql = rt.sql();
    let mut w = match sql.writer().await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open writer for Vamana snapshot invalidation");
            return;
        }
    };
    match invalidate_legacy_vamana_snapshots(w.as_mut(), namespace, "invalidate_vamana_snapshot")
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("no such table") => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to invalidate Vamana snapshot");
        }
    }
}
