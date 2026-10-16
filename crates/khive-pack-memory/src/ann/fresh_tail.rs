use super::*;

// ── ADR-118: fresh-tail exact leg (read-your-writes recall visibility) ─────────

/// Read the currently-installed bridge's fresh-tail watermark for `key`: the
/// `ann_write_log` seq its corpus state reflects, or `None` if no bridge is
/// installed. Test-only: production call sites need this paired atomically
/// with the search that produced their candidates (ADR-118) and get it from
/// [`search_loaded_with_seq`] directly instead of a second lock acquisition.
#[cfg(test)]
pub(crate) async fn bridge_applied_seq(ann: &SharedAnn, key: &AnnKey) -> Option<u64> {
    let guard = ann.indexes.read().await;
    guard
        .get(key)
        .map(|b| b.index.last_applied_seq().unwrap_or(0))
}

/// Open an explicit read transaction so a reader keeps one connection across
/// calls (ADR-118 §1). Load-bearing corpus hydration still uses one
/// statement (see [`fetch_final_tail_on`]) since a pool-backed reader may
/// reacquire a connection per call. `BEGIN DEFERRED` is valid read-only.
pub(super) async fn begin_read_snapshot(
    reader: &mut dyn khive_storage::SqlReader,
) -> Result<(), String> {
    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: Some("memory_ann_fresh_tail_snapshot_begin".into()),
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Close the snapshot opened by [`begin_read_snapshot`]. The transaction is
/// read-only (no DML issued inside it), so `COMMIT` and `ROLLBACK` are
/// equivalent — `COMMIT` is used for symmetry with a normal transaction end.
pub(super) async fn end_read_snapshot(reader: &mut dyn khive_storage::SqlReader) {
    if let Err(e) = reader
        .query_all(SqlStatement {
            sql: "COMMIT".into(),
            params: vec![],
            label: Some("memory_ann_fresh_tail_snapshot_end".into()),
        })
        .await
    {
        tracing::warn!(error = %e, "fresh-tail: snapshot COMMIT failed (read-only transaction; the connection is dropped regardless, so no lock is leaked)");
    }
}

/// Exact cosine similarity between a raw query vector and a raw stored
/// embedding, using the same L2-normalization convention as
/// [`AnnBridge::search`]: both vectors normalized, dot product, clamped to a
/// non-negative floor. An empty query or a length mismatch scores 0.0.
pub(crate) fn exact_cosine(query: &[f32], embedding: &[f32]) -> f32 {
    if query.len() != embedding.len() || query.is_empty() {
        return 0.0;
    }
    exact_cosine_unclamped(query, embedding).max(0.0)
}

pub(super) fn exact_cosine_unclamped(query: &[f32], embedding: &[f32]) -> f32 {
    let mut q = query.to_vec();
    l2_normalize(&mut q);
    let mut e = embedding.to_vec();
    l2_normalize(&mut e);
    q.iter().zip(e.iter()).map(|(a, b)| a * b).sum::<f32>()
}

pub(crate) fn merge_fresh_tail_for_route(
    best_raw: Vec<(Uuid, f64)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
    route: AnnScoreRoute,
) -> Result<Vec<(Uuid, f64)>, RuntimeError> {
    khive_retrieval::ann::merge_fresh_tail(best_raw, ops, |embedding| {
        route.tail_score(query, embedding)
    })
}

/// Keep the cheap wait probe and candidate-producing snapshot on the same
/// original-log-or-published-watermark fence predicate.
pub(super) fn session_fence_predicate(
    model_param: usize,
    seq_param: usize,
    namespace_param: usize,
    consumer_param: usize,
    wildcard_param: usize,
) -> String {
    format!(
        "EXISTS(SELECT 1 FROM ann_write_log \
                  WHERE seq = ?{seq_param} AND namespace = ?{namespace_param} \
                    AND embedding_model = ?{model_param} \
                    AND kind = 'note' AND field = 'note.content' \
                    AND op = 'upsert') \
         OR EXISTS(SELECT 1 FROM ann_consumer_watermark \
                    WHERE consumer = ?{consumer_param} AND namespace = ?{wildcard_param} \
                      AND embedding_model = ?{model_param} \
                      AND watermark >= ?{seq_param} AND watermark >= 0)"
    )
}

/// Poll only the fence while a session recall waits. This is a scheduling hint,
/// not the candidate proof: `session_exact_candidates` repeats the same
/// predicate inside its candidate-producing SQLite snapshot.
pub(crate) async fn session_unmet_fences(
    rt: &KhiveRuntime,
    fence: &crate::visibility::VisibilityFence,
) -> Vec<String> {
    let mut reader = match rt.sql().reader().await {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!(%error, "session fence probe could not open a reader");
            return fence
                .fences
                .iter()
                .map(|entry| entry.model.clone())
                .collect();
        }
    };
    let sql = format!(
        "SELECT ({}) AS has_fence",
        session_fence_predicate(1, 2, 3, 4, 5)
    );
    let mut unmet = Vec::new();
    for entry in &fence.fences {
        let Ok(seq) = i64::try_from(entry.ann_write_log_seq) else {
            unmet.push(entry.model.clone());
            continue;
        };
        #[cfg(test)]
        let _ = SESSION_FENCE_PROBE_COUNT.try_with(|count| count.set(count.get() + 1));
        let result = reader
            .query_scalar(SqlStatement {
                sql: sql.clone(),
                params: vec![
                    SqlValue::Text(entry.model.clone()),
                    SqlValue::Integer(seq),
                    SqlValue::Text(fence.namespace.clone()),
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(ANN_WILDCARD_NS.into()),
                ],
                label: Some("memory_session_fence_probe".into()),
            })
            .await;
        if !matches!(&result, Ok(Some(SqlValue::Integer(1)))) {
            if let Err(error) = result {
                tracing::warn!(model = %entry.model, %error, "session fence probe failed");
            }
            unmet.push(entry.model.clone());
        }
    }
    unmet
}

/// Merge a fresh-tail's coalesced final ops into an ANN candidate list
/// (ADR-118 §2): deduplicated by `subject_id` with the tail winning, then
/// re-sorted by score, with equal scores in ascending id order (ADR-079
/// Amendment 4, item 6). A `None` op (delete) drops the subject even if it was
/// in `best_raw` — the tail is authoritative for every subject it names.
pub(crate) fn merge_fresh_tail(
    best_raw: Vec<(Uuid, f32)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
) -> Vec<(Uuid, f32)> {
    khive_retrieval::ann::merge_fresh_tail(best_raw, ops, |embedding| {
        Ok::<_, std::convert::Infallible>(exact_cosine(query, embedding))
    })
    .unwrap_or_else(|never| match never {})
}

/// Fold a [`FreshTailOutcome`] into the candidates a recall handler serves
/// plus the degradation disclosure it owes the caller. Every recall path
/// goes through this single mapping so no exceptional class is silently
/// treated as healthy; see `docs/ann.md` for the per-variant disclosure rule.
pub(crate) fn outcome_into_candidates(
    outcome: FreshTailOutcome,
    prior: Vec<(Uuid, f32)>,
    query: &[f32],
) -> (Vec<(Uuid, f32)>, Option<String>) {
    match outcome {
        FreshTailOutcome::Ops(ops) => (merge_fresh_tail(prior, query, ops), None),
        FreshTailOutcome::Replace(candidates, reason) => (
            candidates
                .into_iter()
                .map(|(id, score)| (id, score as f32))
                .collect(),
            reason.map(str::to_string),
        ),
        FreshTailOutcome::Skipped(reason) => (prior, Some(reason.to_string())),
    }
}

/// Cap on the error text a [`SkipReason`] carries beside its label, counted
/// in characters and inclusive of [`SKIP_DETAIL_TRUNCATION_MARKER`]. A
/// degraded recall stamps the rendered reason onto every hit it serves, so an
/// error that quotes a whole statement must not be able to bloat the response.
pub(super) const SKIP_DETAIL_MAX_CHARS: usize = 200;

/// Written in place of the characters a cut removed, so a reader can tell a
/// bounded rendering from a complete one.
pub(super) const SKIP_DETAIL_TRUNCATION_MARKER: &str = "...";

/// Bound `detail` to [`SKIP_DETAIL_MAX_CHARS`] characters, marking a cut with
/// [`SKIP_DETAIL_TRUNCATION_MARKER`]. The bound is character-wise, not
/// byte-wise, so a multi-byte error message cannot be split mid-character.
pub(super) fn bound_skip_detail(detail: &str) -> String {
    if detail.char_indices().nth(SKIP_DETAIL_MAX_CHARS).is_none() {
        return detail.to_owned();
    }
    let keep = SKIP_DETAIL_MAX_CHARS - SKIP_DETAIL_TRUNCATION_MARKER.chars().count();
    let cut = match detail.char_indices().nth(keep) {
        Some((byte_index, _)) => byte_index,
        None => detail.len(),
    };
    let mut bounded = String::with_capacity(cut + SKIP_DETAIL_TRUNCATION_MARKER.len());
    bounded.push_str(&detail[..cut]);
    bounded.push_str(SKIP_DETAIL_TRUNCATION_MARKER);
    bounded
}

/// Why the fresh-tail leg sat out a query: the failure-site label, plus the
/// error that caused the skip whenever the site was holding one.
///
/// One label covers causes that differ in what the caller should do next — a
/// reader pool exhausted for a moment clears on the next query, a database
/// file the process cannot open does not, and both arrive as "reader open
/// failed" — so the error travels out with the label instead of stopping at a
/// log line the caller cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkipReason {
    pub(super) label: &'static str,
    pub(super) detail: Option<String>,
}

impl SkipReason {
    /// A skip whose site holds no error: the bare label, unchanged.
    pub(super) fn bare(label: &'static str) -> Self {
        Self {
            label,
            detail: None,
        }
    }

    /// A skip whose site holds the error that caused it. The error's own
    /// message is bounded by [`bound_skip_detail`] before it is carried.
    pub(super) fn with_error(label: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            label,
            detail: Some(bound_skip_detail(&error.to_string())),
        }
    }

    /// The failure-site label on its own, without any error text.
    #[cfg(test)]
    pub(super) fn label(&self) -> &'static str {
        self.label
    }

    /// The bounded error text, or `None` when the site held no error.
    #[cfg(test)]
    pub(super) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}: {detail}", self.label),
            None => f.write_str(self.label),
        }
    }
}

/// Outcome of [`fresh_tail_leg`]. Full semantics and the disclosure contract
/// for each variant: `docs/ann.md`.
pub(crate) enum FreshTailOutcome {
    /// Coalesced final tail ops, valid against the caller's existing
    /// candidates (`best_raw` unchanged) — merge via [`merge_fresh_tail`].
    /// The common case.
    Ops(Vec<(Uuid, Option<Vec<f32>>)>),
    /// A compaction mismatch forced re-resolution: these candidates REPLACE
    /// `best_raw` outright (never merge them with the stale set). `Some(reason)`
    /// discloses when read-your-writes visibility was lost after
    /// re-resolution; `None` means the full pair was assembled.
    Replace(Vec<(Uuid, f64)>, Option<&'static str>),
    /// The leg sat out this query entirely; the caller's candidates are
    /// unaffected. The payload renders to a non-empty failure-site
    /// diagnostic: the label, followed by the error that caused the skip
    /// whenever the site was holding one. Callers may depend on its
    /// presence, not on its exact wording.
    Skipped(SkipReason),
}

pub(super) fn replace_with_merged_tail(
    candidates: Vec<(Uuid, f64)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
    route: AnnScoreRoute,
) -> FreshTailOutcome {
    if matches!(route, AnnScoreRoute::Memory) {
        let candidates = candidates
            .into_iter()
            .map(|(id, score)| (id, score as f32))
            .collect();
        return FreshTailOutcome::Replace(
            merge_fresh_tail(candidates, query, ops)
                .into_iter()
                .map(|(id, score)| (id, f64::from(score)))
                .collect(),
            None,
        );
    }
    match merge_fresh_tail_for_route(candidates, query, ops, route) {
        Ok(merged) => FreshTailOutcome::Replace(merged, None),
        Err(error) => FreshTailOutcome::Skipped(SkipReason::with_error(
            "fresh-tail: canonical score conversion failed",
            error,
        )),
    }
}

/// The ADR-118 fresh-tail exact leg, giving read-your-writes visibility.
/// `s = Some(watermark)` is the primary tier: merges every committed write
/// above the serving bridge's watermark. `s = None` is the no-index tier
/// (§3): caps the scan at a corpus-relative newest suffix instead of the
/// full scope. `query`/`k` serve only the primary tier's mismatch
/// re-resolution path. See `docs/ann.md` for both tiers in full.
pub(crate) async fn fresh_tail_leg(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    query: &[f32],
    k: usize,
    s: Option<u64>,
) -> FreshTailOutcome {
    // Registration precondition (ADR-118 §1): a serving bridge is trusted only
    // while its consumer is active (`S >= 0`). Pending/recovery/absence means a
    // peer may have retired the old protection; drop the already-captured ANN
    // candidates so even a disabled exact leg cannot leak stale state.
    let mut registry_state = read_own_watermark(rt, model).await;
    if s.is_some()
        && ann_segment_dir(rt, model).is_none()
        && matches!(registry_state, Ok(Some(PENDING_WATERMARK)))
    {
        // Wait out the pathless install-before-activation window (see
        // `docs/ann.md`) and revalidate, or the closed-state guard below
        // could evict a just-built, about-to-activate bridge.
        let lock = model_warm_lock(ann, key).await;
        #[cfg(test)]
        ann.pathless_pending_publication_wait.notify_one();
        let _publication_guard = lock.lock().await;
        registry_state = read_own_watermark(rt, model).await;
    }
    match registry_state {
        Ok(Some(watermark)) if watermark >= 0 => {}
        Ok(Some(watermark)) => {
            evict_unprotected_index(ann, key).await;
            tracing::warn!(
                model,
                watermark,
                "fresh-tail: closed consumer registration; dropping stale ANN candidates"
            );
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: closed consumer registration; dropped unprotected candidates"),
            );
        }
        Ok(None) => {
            evict_unprotected_index(ann, key).await;
            if let Err(e) = register_consumer(rt, model).await {
                tracing::warn!(error = %e, model, "fresh-tail: consumer re-registration failed");
            }
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: consumer registration absent; dropped unprotected candidates and re-registered"),
            );
        }
        Err(e) => {
            evict_unprotected_index(ann, key).await;
            tracing::warn!(error = %e, model, "fresh-tail: registry read failed; dropping stale ANN candidates");
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: registry read failed; dropped unprotected candidates"),
            );
        }
    }

    if !rt.ann_fresh_tail_enabled() {
        return FreshTailOutcome::Skipped(SkipReason::bare(
            "fresh-tail leg disabled by runtime policy (KHIVE_ANN_FRESH_TAIL is sampled at construction)",
        ));
    }

    match s {
        Some(s) => {
            fresh_tail_serving(
                rt,
                ann,
                key,
                model,
                FreshTailSearch::new(query, k, AnnScoreRoute::Memory),
                s,
                None,
            )
            .await
        }
        None => fresh_tail_capped(rt, model).await,
    }
}

/// Tier 1: a serving bridge exists at watermark `s`. The registry-minimum
/// guard and tail statement share one read transaction (ADR-118 §1
/// "Compaction linearization"; see `docs/ann.md`).
pub(crate) async fn fresh_tail_serving(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    search: FreshTailSearch<'_>,
    s: u64,
    consumer: Option<&str>,
) -> FreshTailOutcome {
    let mut reader = match rt.sql().reader().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: reader open failed; skipping exact leg");
            return FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: reader open failed",
                e,
            ));
        }
    };
    if let Err(e) = begin_read_snapshot(reader.as_mut()).await {
        tracing::warn!(error = %e, model, "fresh-tail: snapshot begin failed; skipping exact leg");
        return FreshTailOutcome::Skipped(SkipReason::with_error(
            "fresh-tail: snapshot begin failed",
            e,
        ));
    }
    if let Some(consumer) = consumer {
        let own = ann_registry::read_watermark_on(
            reader.as_mut(),
            "note_search_ann_consumer_snapshot",
            consumer,
            ANN_WILDCARD_NS,
            model,
        )
        .await
        .map_err(|error| error.to_string());
        if !matches!(own, Ok(Some(watermark)) if watermark >= 0) {
            end_read_snapshot(reader.as_mut()).await;
            return FreshTailOutcome::Skipped(SkipReason::bare(
                "note-search ANN consumer is not active in tail snapshot",
            ));
        }
    }

    let registry_min = match ann_registry::min_watermark_on(
        reader.as_mut(),
        "memory_",
        ANN_WILDCARD_NS,
        model,
    )
    .await
    .map_err(|e| e.to_string())
    {
        Ok(v) => v,
        Err(e) => {
            end_read_snapshot(reader.as_mut()).await;
            tracing::warn!(error = %e, model, "fresh-tail: registry-min read failed; skipping exact leg");
            return FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: registry-min read failed",
                e,
            ));
        }
    };

    // Negative (closed-state) minima must never be lossily cast to a huge
    // u64 floor — they cannot indicate that compaction outran `s`.
    if let Some(m) = registry_min.and_then(|value| u64::try_from(value).ok()) {
        if m > s {
            if ann_segment_dir(rt, model).is_none() {
                // A checkpoint may have raised the registry floor but not yet
                // marked its dirty bridge published. Never wait for that
                // checkpoint's model lock while pinning the pathless SQL
                // connection it needs to finish.
                end_read_snapshot(reader.as_mut()).await;
                drop(reader);
                return fresh_tail_pathless_reresolve(rt, ann, key, model, search, consumer).await;
            }

            // Mismatch (ADR-118 §1): the log may no longer retain every row
            // above `s`. A filesystem commit-record read (no DB access)
            // decides whether re-resolution is possible before this
            // snapshot's floor fallback is needed.
            let persisted = match ann_segment_dir(rt, model) {
                None => Ok(None),
                Some(dir) => match read_commit_info(&dir) {
                    Err(e) => Err(e.to_string()),
                    Ok(info) => match info.and_then(|info| info.last_applied_seq) {
                        None => Ok(None),
                        Some(base_seq) => {
                            effective_persisted_state(&dir, base_seq).map(|(seq, _)| Some(seq))
                        }
                    },
                },
            };
            let resolved_new_s = match persisted {
                Ok(seq) => seq.filter(|new_s| *new_s >= m),
                Err(e) => {
                    // An unreadable commit record or delta HEAD does not show
                    // that no newer segment exists. Compaction through `m`
                    // means a published state covered `m`, so a write it
                    // removed is in neither the stale candidates nor the log
                    // above `m`: no floored tail can complete them.
                    end_read_snapshot(reader.as_mut()).await;
                    tracing::warn!(error = %e, model, "fresh-tail: persisted segment state read failed; dropping stale ANN candidates");
                    bump_generation(ann, key).await;
                    return FreshTailOutcome::Replace(
                        Vec::new(),
                        Some("fresh-tail: persisted segment state read failed; dropped stale candidates"),
                    );
                }
            };
            return match resolved_new_s {
                Some(new_s) => {
                    end_read_snapshot(reader.as_mut()).await;
                    drop(reader);
                    fresh_tail_reresolve(rt, ann, key, model, search, new_s, consumer).await
                }
                None => {
                    // Missing publication metadata or a base below `m` cannot
                    // account for the compacted interval `(s, m]`. A tail
                    // above `m` cannot make the captured candidates complete.
                    end_read_snapshot(reader.as_mut()).await;
                    bump_generation(ann, key).await;
                    FreshTailOutcome::Replace(
                        Vec::new(),
                        Some("fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates"),
                    )
                }
            };
        }
    }

    let outcome = fetch_final_tail_on(reader.as_mut(), model, s, None).await;
    end_read_snapshot(reader.as_mut()).await;
    match outcome {
        Ok((ops, _new_s)) => FreshTailOutcome::Ops(ops),
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: tail fetch failed; skipping exact leg");
            FreshTailOutcome::Skipped(SkipReason::with_error("fresh-tail: tail fetch failed", e))
        }
    }
}

/// Resolve a pathless checkpoint mismatch without holding a SQL read snapshot
/// while waiting for the checkpoint's per-model lock. Once that checkpoint
/// finishes, the bridge search and a new registry/tail snapshot form one
/// coherent pair; this checkpoint's compaction cannot race the model lock.
pub(super) async fn fresh_tail_pathless_reresolve(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    search: FreshTailSearch<'_>,
    consumer: Option<&str>,
) -> FreshTailOutcome {
    let FreshTailSearch { query, k, route } = search;
    let lock = model_warm_lock(ann, key).await;
    #[cfg(test)]
    ann.pathless_reresolve_wait_notify.notify_one();
    let _publication_guard = lock.lock().await;
    let (candidates, resolved_s) = match search_loaded_with_seq_route(ann, key, query, k, route)
        .await
    {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            bump_generation(ann, key).await;
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless mismatch has no installed bridge; dropped stale candidates"),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolution search failed; dropping stale ANN candidates");
            bump_generation(ann, key).await;
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless re-resolution search failed; dropped stale candidates"),
            );
        }
    };

    let mut reader = match rt.sql().reader().await {
        Ok(reader) => reader,
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved reader open failed");
            return FreshTailOutcome::Replace(
                candidates,
                Some("fresh-tail: pathless re-resolved reader open failed; served re-resolved candidates without fresh-tail merge"),
            );
        }
    };
    if let Err(e) = begin_read_snapshot(reader.as_mut()).await {
        tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved snapshot begin failed");
        return FreshTailOutcome::Replace(
            candidates,
            Some("fresh-tail: pathless re-resolved snapshot begin failed; served re-resolved candidates without fresh-tail merge"),
        );
    }
    if let Some(consumer) = consumer {
        let own = ann_registry::read_watermark_on(
            reader.as_mut(),
            "note_search_ann_consumer_snapshot",
            consumer,
            ANN_WILDCARD_NS,
            model,
        )
        .await
        .map_err(|error| error.to_string());
        if !matches!(own, Ok(Some(watermark)) if watermark >= 0) {
            end_read_snapshot(reader.as_mut()).await;
            return FreshTailOutcome::Skipped(SkipReason::bare(
                "note-search ANN consumer is not active after pathless re-resolution",
            ));
        }
    }
    let floor = ann_registry::min_watermark_on(reader.as_mut(), "memory_", ANN_WILDCARD_NS, model)
        .await
        .map_err(|e| e.to_string());
    let outcome = match floor {
        Ok(floor)
            if floor
                .and_then(|value| u64::try_from(value).ok())
                .is_none_or(|floor| floor <= resolved_s) =>
        {
            match fetch_final_tail_on(reader.as_mut(), model, resolved_s, None).await {
                Ok((ops, _)) => replace_with_merged_tail(candidates, query, ops, route),
                Err(e) => {
                    tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved tail fetch failed; serving re-resolved candidates");
                    FreshTailOutcome::Replace(
                        candidates,
                        Some("fresh-tail: pathless re-resolved tail fetch failed; served re-resolved candidates without fresh-tail merge"),
                    )
                }
            }
        }
        Ok(floor) => {
            tracing::warn!(
                ?floor,
                resolved_s,
                model,
                "fresh-tail: pathless re-resolved bridge is below the registry floor"
            );
            bump_generation(ann, key).await;
            FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless mismatch has no bridge at the registry floor; dropped stale candidates"),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved registry read failed");
            FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless re-resolved registry read failed; dropped stale candidates"),
            )
        }
    };
    end_read_snapshot(reader.as_mut()).await;
    outcome
}

/// Bound on the re-resolution convergence loop below; three back-to-back peer
/// checkpoints inside one query's read window would be pathological. See
/// `docs/ann.md` for the convergence argument.
pub(super) const FRESH_TAIL_RERESOLVE_MAX_ROUNDS: u32 = 3;

/// Real mismatch re-resolution (ADR-118 §1): load the currently published
/// segment, search it, and merge in its own tail above its own watermark — a
/// self-consistent pair that never borrows a newer watermark while serving
/// stale-bridge candidates. Reloads on a further mismatch instead of
/// immediately flooring, up to [`FRESH_TAIL_RERESOLVE_MAX_ROUNDS`]; see
/// `docs/ann.md` for why that reload converges and why flooring immediately
/// would silently drop committed writes.
pub(super) async fn fresh_tail_reresolve(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    search: FreshTailSearch<'_>,
    new_s: u64,
    consumer: Option<&str>,
) -> FreshTailOutcome {
    let FreshTailSearch { query, k, route } = search;
    let mut expected_s = new_s;
    for round in 1..=FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
        // Every failure below that cannot produce re-resolved candidates drops
        // the stale ones: the caller chose re-resolution because compaction
        // passed their watermark, so a write it removed is in neither the
        // stale candidates nor the log above the registry minimum.
        let Some(dir) = ann_segment_dir(rt, model) else {
            bump_generation(ann, key).await;
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: re-resolved segment directory unavailable; dropped stale candidates"),
            );
        };
        let bridge = match AnnBridge::load(&dir) {
            Ok(b) => b,
            Err(e) => {
                // The caller chose re-resolution from the delta HEAD alone. A
                // load that rejects the segment (a missing or corrupt chunk on
                // the chain HEAD names included) cannot deliver the watermark
                // HEAD promised. A write compacted into that chain is in
                // neither the stale candidates nor the log above the registry
                // minimum, so no tail can complete them: drop them.
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved segment load failed; dropping stale ANN candidates");
                bump_generation(ann, key).await;
                return FreshTailOutcome::Replace(
                    Vec::new(),
                    Some("fresh-tail: re-resolved segment load failed; dropped stale candidates"),
                );
            }
        };
        let s_loaded = bridge.index.last_applied_seq().unwrap_or(expected_s);
        let candidates = match bridge.search_with_route(query, k, route) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved segment search failed; dropping stale ANN candidates");
                bump_generation(ann, key).await;
                return FreshTailOutcome::Replace(
                    Vec::new(),
                    Some("fresh-tail: re-resolved segment search failed; dropped stale candidates"),
                );
            }
        };
        // This load served only the current query; force re-adoption so the
        // background warm path installs it for future ones too.
        bump_generation(ann, key).await;

        #[cfg(test)]
        {
            if ann
                .reresolve_race_barrier
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                ann.reresolve_race_notify.notify_one();
                ann.reresolve_race_release.notified().await;
            }
        }

        // Re-validate the registry minimum and run the tail scan inside one
        // snapshot so a concurrent checkpoint can't compact past `s_loaded`
        // between the load above and this fetch (docs/ann.md).
        let mut reader = match rt.sql().reader().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved reader open failed; serving re-resolved candidates without further tail");
                return FreshTailOutcome::Replace(
                    candidates,
                    Some("fresh-tail: re-resolved reader open failed"),
                );
            }
        };
        if let Err(e) = begin_read_snapshot(reader.as_mut()).await {
            tracing::warn!(error = %e, model, "fresh-tail: re-resolved snapshot begin failed; serving re-resolved candidates without further tail");
            return FreshTailOutcome::Replace(
                candidates,
                Some("fresh-tail: re-resolved snapshot begin failed"),
            );
        }
        if let Some(consumer) = consumer {
            let own = ann_registry::read_watermark_on(
                reader.as_mut(),
                "note_search_ann_consumer_snapshot",
                consumer,
                ANN_WILDCARD_NS,
                model,
            )
            .await
            .map_err(|error| error.to_string());
            if !matches!(own, Ok(Some(watermark)) if watermark >= 0) {
                end_read_snapshot(reader.as_mut()).await;
                return FreshTailOutcome::Skipped(SkipReason::bare(
                    "note-search ANN consumer is not active after segment re-resolution",
                ));
            }
        }

        let registry_min = match ann_registry::min_watermark_on(
            reader.as_mut(),
            "memory_",
            ANN_WILDCARD_NS,
            model,
        )
        .await
        .map_err(|e| e.to_string())
        {
            Ok(v) => v.and_then(|value| u64::try_from(value).ok()),
            Err(e) => {
                end_read_snapshot(reader.as_mut()).await;
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved registry-min read failed; serving re-resolved candidates without further tail");
                return FreshTailOutcome::Replace(
                    candidates,
                    Some("fresh-tail: re-resolved registry-min read failed"),
                );
            }
        };

        let coherent = match registry_min {
            Some(m) => m <= s_loaded,
            None => true,
        };
        if coherent {
            // Coherent (these candidates, this watermark) pair: the log
            // still retains every row above `s_loaded`, so the tail scan
            // needs no floor beyond it.
            let outcome = fetch_final_tail_on(reader.as_mut(), model, s_loaded, None).await;
            end_read_snapshot(reader.as_mut()).await;
            return match outcome {
                Ok((ops, _)) => replace_with_merged_tail(candidates, query, ops, route),
                Err(e) => {
                    tracing::warn!(error = %e, model, "fresh-tail: re-resolved tail fetch failed; serving re-resolved candidates without further tail");
                    FreshTailOutcome::Replace(
                        candidates,
                        Some("fresh-tail: re-resolved tail fetch failed"),
                    )
                }
            };
        }
        let m = registry_min.expect("coherent=false implies registry_min is Some");

        if round == FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
            // Terminal round (ADR-118 §1): serve the last loaded candidates
            // floored at the last observed minimum — coherent, at the cost
            // of the (s_loaded, m] window not being provably retained.
            tracing::warn!(
                model,
                rounds = round,
                floor = m,
                "fresh-tail: re-resolution did not converge within {round} rounds; \
                 serving the ADR-118 floored fallback"
            );
            let outcome = fetch_final_tail_on(reader.as_mut(), model, m, None).await;
            end_read_snapshot(reader.as_mut()).await;
            return match outcome {
                Ok((ops, _)) => replace_with_merged_tail(candidates, query, ops, route),
                Err(e) => {
                    tracing::warn!(error = %e, model, "fresh-tail: floored-fallback tail fetch failed; serving re-resolved candidates without further tail");
                    FreshTailOutcome::Replace(
                        candidates,
                        Some("fresh-tail: floored-fallback tail fetch failed"),
                    )
                }
            };
        }

        end_read_snapshot(reader.as_mut()).await;
        // Reload the currently published segment next round — by the
        // compaction invariant above it is now at or past `m`.
        expected_s = m;
    }
    unreachable!("the loop always returns on or before its terminal round")
}

/// Tier 2 (§3): no serving index at all. A cheap, log-only existence probe
/// (no corpus join) fast-paths the common empty-tail case; a non-empty tail
/// is capped at ceil(`ann_rebuild_threshold() * live corpus`) rows. After the
/// probe, one statement/snapshot computes that live count, selects the newest
/// suffix, and hydrates its current vectors and note liveness.
pub(super) async fn fresh_tail_capped(rt: &KhiveRuntime, model: &str) -> FreshTailOutcome {
    fresh_tail_capped_at_threshold(rt, model, ann_rebuild_threshold()).await
}

pub(super) async fn fresh_tail_capped_at_threshold(
    rt: &KhiveRuntime,
    model: &str,
    live_threshold: f64,
) -> FreshTailOutcome {
    match tail_exists(rt, model, 0).await {
        Ok(false) => return FreshTailOutcome::Ops(Vec::new()),
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: tail-existence read failed; skipping capped exact leg");
            return FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: tail-existence read failed",
                e,
            ));
        }
    }
    match fetch_final_tail(rt, model, 0, Some(live_threshold)).await {
        Ok((ops, _new_s)) => FreshTailOutcome::Ops(ops),
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: capped tail fetch failed; skipping exact leg");
            FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: capped tail fetch failed",
                e,
            ))
        }
    }
}
