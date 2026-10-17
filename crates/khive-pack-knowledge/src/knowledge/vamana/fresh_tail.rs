use super::*;

/// Coalesce the scope's tail (rows above `s`) to the final op per subject in
/// ONE aggregate query — SQLite's bare-column-with-MAX guarantee makes `op`
/// the value from each subject's max-seq row. Returns `(subject, is_delete)`
/// pairs plus the new watermark; memory is O(distinct tail subjects), never
/// O(tail rows). Embeddings are read separately, per batch, by
/// [`replay_final_states`].
pub(super) async fn fetch_final_states(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
) -> Result<(Vec<(Uuid, bool)>, u64), String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: ::khive_runtime::sql!("knowledge_ann_final_states").into(),
            params: vec![
                SqlValue::Text(ns.to_owned()),
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
            ],
            label: Some("ann_fetch_final_states".into()),
        })
        .await
        .map_err(|e| e.to_string())?;

    let mut new_s = s;
    let mut finals: Vec<(Uuid, bool)> = Vec::with_capacity(rows.len());
    for row in &rows {
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
        finals.push((uuid, is_delete));
    }
    Ok((finals, new_s))
}

/// Subjects per streamed replay batch: bounds transient replay memory at
/// O(batch × dimensions) regardless of tail size.
pub(super) const REPLAY_BATCH: usize = 500;

/// Stream the coalesced final states onto `bridge`. Each batch hydrates only
/// its upserts through the scoped vector store before applying the original
/// final-op order. A missing or out-of-scope source is a contradiction -> Cold.
pub(super) async fn replay_final_states(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    bridge: &mut AnnBridge,
    ns: &str,
    model: &str,
    finals: &[(Uuid, bool)],
) -> Result<(), String> {
    let mut store: Option<Arc<dyn khive_storage::VectorStore>> = None;
    let mut reverse = bridge.reverse_map_for(finals.iter().map(|(uuid, _)| *uuid));

    for batch in finals.chunks(REPLAY_BATCH) {
        let ids: Vec<Uuid> = batch
            .iter()
            .filter_map(|(id, delete)| (!*delete).then_some(*id))
            .collect();
        if store.is_none() && !ids.is_empty() {
            // A persisted literal-model scope must not read a canonical alias's rows.
            if rt
                .resolve_embedding_model(Some(model))
                .is_ok_and(|resolved| resolved.to_string() != model)
            {
                return Err(format!(
                    "replay model {model} is not canonical (contradiction -> Cold)"
                ));
            }
            store = Some(
                rt.vectors_for_model(token, model)
                    .map_err(|e| e.to_string())?,
            );
        }
        let mut embeddings = match &store {
            Some(store) if !ids.is_empty() => store
                .get_vectors(&ids, ns, "knowledge.atom")
                .await
                .map_err(|e| e.to_string())?,
            _ => HashMap::new(),
        };
        for uuid in &ids {
            if !embeddings.contains_key(uuid) {
                return Err(format!(
                    "final upsert for {uuid} has no source row (contradiction → Cold)"
                ));
            }
        }
        for (uuid, is_delete) in batch {
            let op =
                if *is_delete {
                    None
                } else {
                    Some(embeddings.remove(uuid).ok_or_else(|| {
                        format!("final upsert for {uuid}: embedding lost in batch")
                    })?)
                };
            bridge.apply_final_op(&mut reverse, *uuid, op)?;
        }
    }
    Ok(())
}

// ── ADR-118: fresh-tail exact leg ─────────────────────────────────────────

pub(super) struct FreshTailSnapshot {
    pub(super) own_watermark: Option<i64>,
    pub(super) registry_min: Option<i64>,
    pub(super) live_count: Option<u64>,
    pub(super) ops: Vec<(Uuid, Option<Vec<f32>>)>,
}

/// The snapshot statement.  `finals` reduces the selected log suffix to one row
/// per subject (its last operation, at the position of its first appearance)
/// before the vector join, so the join reads one embedding per distinct
/// subject rather than one per raw log row.
pub(super) fn fresh_tail_snapshot_statement(
    ns: &str,
    model: &str,
    watermark: i64,
    live_threshold: Option<f64>,
) -> SqlStatement {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    knowledge_corpus(ns).final_tail(
        &table_name,
        model,
        watermark,
        live_threshold,
        khive_retrieval::ann::corpus::TailFloor::RegistryMinimum {
            registry_namespace: ns,
            consumer: ANN_CONSUMER,
        },
        "knowledge_ann_fresh_tail_snapshot",
    )
}

/// Read the registry guard, optional live-count cap, selected log suffix, and
/// every final upsert embedding in one SQLite statement.  A single statement
/// is the snapshot primitive on every backend, including the in-memory
/// pool-backed reader whose separate calls may use separate connections.
pub(super) async fn fetch_fresh_tail_snapshot(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    watermark: u64,
    live_threshold: Option<f64>,
) -> Result<FreshTailSnapshot, String> {
    let watermark = i64::try_from(watermark)
        .map_err(|_| "fresh-tail watermark exceeds SQLite INTEGER range".to_string())?;
    let statement = fresh_tail_snapshot_statement(ns, model, watermark, live_threshold);

    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|error| error.to_string())?;
    let rows = reader
        .query_all(statement)
        .await
        .map_err(|error| error.to_string())?;

    let first = rows
        .first()
        .ok_or_else(|| "fresh-tail snapshot returned no registry row".to_string())?;
    let read_optional_i64 = |column: &str| -> Result<Option<i64>, String> {
        match first.get(column) {
            Some(SqlValue::Integer(value)) => Ok(Some(*value)),
            Some(SqlValue::Null) | None => Ok(None),
            other => Err(format!("fresh-tail {column}: unexpected value {other:?}")),
        }
    };
    let own_watermark = read_optional_i64("own_watermark")?;
    let registry_min = read_optional_i64("registry_min")?;
    let live_count = match read_optional_i64("live_count")? {
        Some(value) => {
            Some(u64::try_from(value).map_err(|_| "negative fresh-tail live_count".to_string())?)
        }
        None => None,
    };

    type RawVector = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
    );
    let mut finals: Vec<(Uuid, bool, RawVector)> = Vec::new();
    let mut index_by_id: HashMap<Uuid, usize> = HashMap::new();
    for row in &rows {
        let Some(SqlValue::Integer(_)) = row.get("seq") else {
            continue;
        };
        let subject = match row.get("subject_id") {
            Some(SqlValue::Text(value)) => Uuid::parse_str(value)
                .map_err(|error| format!("fresh-tail subject_id {value}: {error}"))?,
            other => return Err(format!("fresh-tail subject_id: unexpected value {other:?}")),
        };
        let is_delete = match row.get("op") {
            Some(SqlValue::Text(value)) if value == "delete" => true,
            Some(SqlValue::Text(value)) if value == "upsert" => false,
            other => return Err(format!("fresh-tail op: unexpected value {other:?}")),
        };
        let raw_vector = (
            match row.get("vector_namespace") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("vector_model") {
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
        match index_by_id.get(&subject) {
            Some(&index) => finals[index] = (subject, is_delete, raw_vector),
            None => {
                index_by_id.insert(subject, finals.len());
                finals.push((subject, is_delete, raw_vector));
            }
        }
    }

    let mut ops = Vec::with_capacity(finals.len());
    for (subject, is_delete, (vector_ns, vector_model, vector_field, embedding)) in finals {
        if is_delete {
            ops.push((subject, None));
            continue;
        }
        let in_scope = vector_ns.as_deref() == Some(ns)
            && vector_model.as_deref() == Some(model)
            && vector_field.as_deref() == Some("knowledge.atom");
        if !in_scope {
            return Err(format!(
                "fresh-tail upsert {subject}: vector row outside consumer scope"
            ));
        }
        let Some(bytes) = embedding else {
            return Err(format!(
                "fresh-tail upsert {subject}: embedding is not a blob"
            ));
        };
        let embedding = khive_storage::decode_f32_native(&bytes).map_err(|_| {
            format!(
                "fresh-tail upsert {subject}: malformed embedding byte length {}",
                bytes.len()
            )
        })?;
        ops.push((subject, Some(embedding)));
    }
    Ok(FreshTailSnapshot {
        own_watermark,
        registry_min,
        live_count,
        ops,
    })
}

pub(super) fn exact_cosine(query: &[f32], embedding: &[f32]) -> f32 {
    if query.len() != embedding.len() || query.is_empty() {
        return 0.0;
    }
    let mut query = query.to_vec();
    let mut embedding = embedding.to_vec();
    l2_normalize(&mut query);
    l2_normalize(&mut embedding);
    query
        .iter()
        .zip(embedding.iter())
        .map(|(left, right)| left * right)
        .sum::<f32>()
        .max(0.0)
}

pub(crate) fn merge_fresh_tail(
    candidates: Vec<(Uuid, f32)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
) -> Vec<(Uuid, f32)> {
    khive_retrieval::ann::merge_fresh_tail(candidates, ops, |embedding| {
        Ok::<_, std::convert::Infallible>(exact_cosine(query, embedding))
    })
    .unwrap_or_else(|never| match never {})
}

pub(crate) async fn merge_fresh_tail_off_thread(
    candidates: Vec<(Uuid, f32)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
) -> Vec<(Uuid, f32)> {
    if ops.is_empty() {
        return candidates;
    }
    let query = query.to_vec();
    tokio::task::spawn_blocking(move || merge_fresh_tail(candidates, &query, ops))
        .await
        .expect("ANN fresh-tail merge task panicked")
}

pub(crate) enum FreshTailOutcome {
    /// Coalesced final tail operations that are valid against the candidate
    /// list the caller already captured from its serving bridge.
    Ops(Vec<(Uuid, Option<Vec<f32>>)>),
    /// A compaction mismatch forced current-query segment re-resolution. These
    /// candidates already come from one coherent replacement segment plus its
    /// own tail and must replace, never merge with, the caller's stale list.
    Replace {
        candidates: Vec<(Uuid, f32)>,
        /// Whether the replacement ANN source returned fewer than the
        /// requested `k` candidates before its own fresh tail was merged.
        source_exhausted: bool,
    },
    /// The exact leg could not run; retain the caller's existing candidates.
    Skipped,
}

pub(crate) async fn fresh_tail_leg(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    watermark: Option<u64>,
) -> FreshTailOutcome {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    if force_rebuild_required(ann, key) {
        // The first detector already evicted the untrusted bridge and retired
        // its Ready ownership.  Do not invalidate the authoritative warm now
        // in flight on every concurrent query; dropping this query's captured
        // candidates is sufficient until that warm replaces the cache.
        return FreshTailOutcome::Replace {
            candidates: Vec::new(),
            source_exhausted: true,
        };
    }
    if !rt.ann_fresh_tail_enabled() {
        return match read_own_watermark(rt, ns, model).await {
            Ok(Some(watermark)) if watermark >= 0 => FreshTailOutcome::Skipped,
            Ok(_) => force_cold_after_registry_loss(rt, ann, key).await,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    namespace = ns,
                    model,
                    "knowledge ANN registry read failed while fresh-tail was disabled"
                );
                FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                }
            }
        };
    }

    match watermark {
        Some(watermark) => fresh_tail_serving(rt, ann, key, query, k, watermark).await,
        None => fresh_tail_capped(rt, ann, key).await,
    }
}

pub(super) async fn force_cold_after_registry_loss(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> FreshTailOutcome {
    if rt.is_read_only() {
        // Registry loss normally publishes the cross-process `-1` rebuild
        // fence. A frozen snapshot cannot acquire that authority, so evict
        // only process-local candidates and remain on the FTS path.
        clear_namespace(ann, &key.namespace).await;
        return FreshTailOutcome::Replace {
            candidates: Vec::new(),
            source_exhausted: true,
        };
    }
    // Publish the cross-process fence before yielding or evicting local state.
    // A peer checkpoint therefore cannot compact/publish through the gap while
    // this process is transitioning its loaded bridge to Cold.
    if let Err(error) = prepare_authoritative_rebuild(rt, ann, key).await {
        tracing::warn!(
            error = %error,
            namespace = key.namespace,
            model = key.model,
            "knowledge ANN failed to publish registry-loss rebuild sentinel"
        );
    }
    clear_namespace(ann, &key.namespace).await;
    FreshTailOutcome::Replace {
        candidates: Vec::new(),
        source_exhausted: true,
    }
}

pub(super) fn ready_snapshot_watermark(snapshot: &FreshTailSnapshot) -> Option<u64> {
    snapshot
        .own_watermark
        .filter(|watermark| *watermark >= 0)
        .and_then(|watermark| u64::try_from(watermark).ok())
}

pub(super) fn nonnegative_registry_min(snapshot: &FreshTailSnapshot) -> u64 {
    snapshot
        .registry_min
        .filter(|watermark| *watermark >= 0)
        .and_then(|watermark| u64::try_from(watermark).ok())
        .unwrap_or(0)
}

pub(super) async fn fresh_tail_serving(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    watermark: u64,
) -> FreshTailOutcome {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    let snapshot = match fetch_fresh_tail_snapshot(rt, ns, model, watermark, None).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = ns,
                model,
                "knowledge fresh-tail snapshot failed; dropping stale vector leg"
            );
            return FreshTailOutcome::Replace {
                candidates: Vec::new(),
                source_exhausted: true,
            };
        }
    };
    let Some(_own_watermark) = ready_snapshot_watermark(&snapshot) else {
        return force_cold_after_registry_loss(rt, ann, key).await;
    };

    let registry_min = nonnegative_registry_min(&snapshot);
    if registry_min > watermark {
        // The log may already have been compacted through `registry_min`, so
        // candidates from the bridge at `watermark` cannot be paired with a
        // scan floored at that newer value. Prefer the currently published
        // segment, whose commit watermark must cover the registry minimum.
        let published_watermark = ann_segment_dir(rt, ns, model)
            .and_then(|dir| read_commit_info(&dir).ok().flatten())
            .and_then(|info| info.last_applied_seq)
            .filter(|published| *published >= registry_min);

        match published_watermark {
            Some(published_watermark) => {
                // Retire the stale cache entry now. The query below owns a
                // local replacement candidate set, while the next request's
                // normal warm path re-adopts the published segment.
                clear_namespace(ann, ns).await;
                return fresh_tail_reresolve(rt, ann, key, query, k, published_watermark).await;
            }
            None => {
                // Re-resolution is not possible in this query. Preserve the
                // same-snapshot coverage proof above the registry minimum, but
                // do not mix those operations with candidates from the older
                // bridge: return an exact-only replacement vector source.
                clear_namespace(ann, ns).await;
                return FreshTailOutcome::Replace {
                    candidates: merge_fresh_tail_off_thread(Vec::new(), query, snapshot.ops).await,
                    source_exhausted: true,
                };
            }
        }
    }
    FreshTailOutcome::Ops(snapshot.ops)
}

/// Bound re-resolution when peers publish checkpoints faster than one query can
/// load and validate them. The terminal branch keeps only the exact suffix above
/// the last same-snapshot registry floor, avoiding an unprovable mixture with
/// candidates from a segment behind that floor.
pub(super) const FRESH_TAIL_RERESOLVE_MAX_ROUNDS: u32 = 3;

/// Load the currently published segment, search it, then validate its watermark
/// against the registry minimum and fetch its own tail in one SQLite snapshot.
/// A peer may advance the minimum between the filesystem load and validation;
/// retry from the newly published segment in that case.
pub(super) async fn fresh_tail_reresolve(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    published_watermark: u64,
) -> FreshTailOutcome {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    let mut expected_watermark = published_watermark;
    for round in 1..=FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
        let Some(dir) = ann_segment_dir(rt, ns, model) else {
            tracing::warn!(
                key = ?key,
                namespace = ns,
                model,
                "knowledge fresh-tail published segment disappeared during re-resolution"
            );
            return FreshTailOutcome::Replace {
                candidates: Vec::new(),
                source_exhausted: true,
            };
        };
        let query_for_search = query.to_vec();
        let loaded = tokio::task::spawn_blocking(move || {
            let bridge = AnnBridge::load(&dir)?;
            let loaded_watermark = bridge.index.last_applied_seq();
            let candidates = bridge.search(&query_for_search, k);
            Ok::<_, String>((candidates, loaded_watermark))
        })
        .await;
        let (candidates, loaded_watermark) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    namespace = ns,
                    model,
                    "knowledge fresh-tail published segment load failed"
                );
                return FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                };
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    namespace = ns,
                    model,
                    "knowledge fresh-tail segment worker failed during re-resolution"
                );
                return FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                };
            }
        };
        let loaded_watermark = loaded_watermark.unwrap_or(expected_watermark);
        let source_exhausted = candidates.len() < k;

        let snapshot = match fetch_fresh_tail_snapshot(rt, ns, model, loaded_watermark, None).await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    namespace = ns,
                    model,
                    "knowledge fresh-tail re-resolution snapshot failed"
                );
                return FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                };
            }
        };
        let Some(_own_watermark) = ready_snapshot_watermark(&snapshot) else {
            return force_cold_after_registry_loss(rt, ann, key).await;
        };
        let registry_min = nonnegative_registry_min(&snapshot);

        if registry_min <= loaded_watermark {
            return FreshTailOutcome::Replace {
                candidates: merge_fresh_tail_off_thread(candidates, query, snapshot.ops).await,
                source_exhausted,
            };
        }

        if round == FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
            tracing::warn!(
                key = ?key,
                namespace = ns,
                model,
                rounds = round,
                floor = registry_min,
                "knowledge fresh-tail re-resolution did not converge; using exact-only floored suffix"
            );
            return FreshTailOutcome::Replace {
                candidates: merge_fresh_tail_off_thread(Vec::new(), query, snapshot.ops).await,
                source_exhausted: true,
            };
        }

        expected_watermark = registry_min;
    }
    unreachable!("fresh-tail re-resolution loop returns within its bounded rounds")
}

pub(super) async fn fresh_tail_capped(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> FreshTailOutcome {
    let snapshot = match fetch_fresh_tail_snapshot(
        rt,
        &key.namespace,
        &key.model,
        0,
        Some(ann_rebuild_threshold()),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = key.namespace,
                model = key.model,
                "knowledge capped fresh-tail fetch failed"
            );
            return FreshTailOutcome::Skipped;
        }
    };
    if ready_snapshot_watermark(&snapshot).is_none() {
        return force_cold_after_registry_loss(rt, ann, key).await;
    }
    debug_assert!(snapshot.live_count.is_some());
    FreshTailOutcome::Ops(snapshot.ops)
}

/// Reconcile a checkpoint after another publisher already advanced the durable
/// row. The loser must adopt the winner (when persisted) rather than overwrite
/// a newer segment or demote a recovered row back to `-1`.
/// `observed_watermark` is the winner's durable lower bound while the caller
/// still holds the bridge publication lock.
pub(super) async fn adopt_checkpoint_winner(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    generation: u64,
    observed_watermark: u64,
) -> bool {
    let installed = match ann_segment_dir(rt, &key.namespace, &key.model) {
        Some(dir) => match AnnBridge::load(&dir) {
            Ok(bridge) if bridge.index.last_applied_seq().unwrap_or(0) >= observed_watermark => {
                // Any incumbent predates registry-loss recovery. Remove it
                // before installing the winner; the normal generation fence
                // still rejects the winner if a local write raced its scan.
                ann.indexes.write().await.remove(key);
                install_replacing(ann, key, bridge.with_generation(generation)).await
            }
            Ok(bridge) => {
                tracing::warn!(
                    key = ?key,
                    bridge_watermark = bridge.index.last_applied_seq().unwrap_or(0),
                    observed_watermark,
                    "checkpoint winner segment trails its durable watermark"
                );
                ann.indexes.write().await.remove(key);
                false
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    "failed to adopt checkpoint race winner"
                );
                ann.indexes.write().await.remove(key);
                false
            }
        },
        // An in-memory runtime has no cross-process segment. A same-process
        // winner may already be installed in the shared AnnState; otherwise
        // the next warm retries as ordinary registered Cold.
        None => {
            let current = has_current_index_at_watermark(ann, key, observed_watermark).await;
            if !current {
                ann.indexes.write().await.remove(key);
            }
            current
        }
    };
    clear_force_rebuild(ann, key);
    installed
}
