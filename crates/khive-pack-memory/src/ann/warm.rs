use super::*;

/// Fire-once per-model background warm. Returns `true` if a new task was started.
pub(crate) async fn ensure_ann_background(
    rt: &KhiveRuntime,
    _token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> bool {
    // Request-time recall and mutation hooks share this entry point with
    // daemon warm. A snapshot may use an already-loaded bridge or the exact
    // sqlite-vec fallback, but it must never enqueue registration,
    // checkpoint, or compaction work against its read-only backend.
    if rt.is_read_only() || model.is_empty() {
        return false;
    }
    let key = AnnKey::from_token(model);

    // Capture the generation before the fast path so the caller observes prior writes.
    let target_generation = current_generation(ann, &key).await;

    // Presence is insufficient: the installed generation must cover the caller's floor.
    // An installed memory bridge may predate this consumer's registration.
    // Its graph is useful to recall, but note search cannot trust its tail
    // until a full checkpoint activates the new watermark.
    let note_search_pending = note_search_requires_full_rebuild(rt, ann, model).await;
    if !note_search_pending
        && installed_is_fresh(ann, &key, target_generation).await
        && !checkpoint_due(ann, &key).await
    {
        return false;
    }

    if !try_take_warming_guard(ann, &key) {
        return false;
    }

    // Tracking lets daemon shutdown drain the task; callers still pay only for enqueue.
    spawn_rebuild_task(rt.clone(), ann.clone(), model.to_owned(), key);
    true
}

/// Synchronously claim a warming key for initial or chained task spawning.
pub(super) fn try_take_warming_guard(ann: &SharedAnn, key: &AnnKey) -> bool {
    let mut warming = lock_warming(ann);
    if warming.contains(key) {
        return false;
    }
    warming.insert(key.clone());
    true
}

/// Spawn a tracked rebuild chain; the caller MUST already hold the warming key.
///
/// This stays synchronous so chained re-enqueue creates an independent `Send` future.
pub(super) fn spawn_rebuild_task(rt: KhiveRuntime, ann: SharedAnn, model: String, key: AnnKey) {
    spawn_rebuild_task_inner(rt, ann, model, key, false);
}

/// Spawn one rebuild task; only post-release chained tasks pay the debounce delay.
pub(super) fn spawn_rebuild_task_inner(
    rt: KhiveRuntime,
    ann: SharedAnn,
    model: String,
    key: AnnKey,
    chained: bool,
) {
    // RAII ties release to every tracked-task exit path.
    let warming_guard = WarmingGuard {
        ann: ann.clone(),
        key: key.clone(),
    };
    let tracking = TrackedJob::register("memory_ann_rebuild");
    let rt = rt.background_view();
    drop(tokio::spawn(async move {
        if chained {
            tokio::time::sleep(rebuild_chain_debounce()).await;
        }
        // Recheck after each build because writes that found this guard occupied were not queued.
        // Bound attempts so continuous writes cannot retain the guard indefinitely; daemon drain
        // supplies the shutdown bound for any remaining debounced chain.
        const ATTEMPT_BOUND: u32 = 3;
        let mut attempt_floor = current_generation(&ann, &key).await;
        let mut attempts: u32 = 0;
        loop {
            let Ok(token) = rt.authorize(Namespace::local()) else {
                break;
            };
            attempts += 1;
            #[cfg(test)]
            {
                ann.attempt_floor_notify.notify_one();
                // Armed tests pause here so their generation bump precedes the build.
                if attempts == 1
                    && ann
                        .attempt_floor_barrier
                        .load(std::sync::atomic::Ordering::SeqCst)
                {
                    ann.attempt_floor_release.notified().await;
                }
            }
            match submit_ann_attempt(&rt, &ann, &model, token, &tracking).await {
                JobOutcome::Completed(Ok(status)) => {
                    tracing::debug!(?status, model = %model, "memory ANN background warm complete");
                }
                JobOutcome::Completed(Err(e)) if is_benign_shutdown_cancellation(&e) => {
                    // Runtime teardown cancellation is expected, not a backend failure.
                    tracing::debug!(error = %e, model = %model, "memory ANN background warm cancelled at shutdown");
                    return;
                }
                JobOutcome::Completed(Err(e)) => {
                    tracing::warn!(error = %e, model = %model, "memory ANN background build failed");
                    break;
                }
                JobOutcome::Cancelled
                | JobOutcome::FinalizationTimedOut
                | JobOutcome::Refused(_) => return,
            }
            let now_generation = current_generation(&ann, &key).await;
            if now_generation <= attempt_floor {
                // No newer generation exists, so another attempt cannot make progress.
                break;
            }
            if attempts >= ATTEMPT_BOUND {
                tracing::debug!(
                    model = %model,
                    attempts,
                    "memory ANN background warm hit its rebuild-attempt bound; \
                     deferring the remainder to the next recall or write"
                );
                break;
            }
            attempt_floor = now_generation;
        }
        // Release BEFORE the final generation check so a raced write can claim a new task.
        drop(warming_guard);
        if !rt.background_domain().control().is_closed()
            && current_generation(&ann, &key).await > attempt_floor
            && try_take_warming_guard(&ann, &key)
        {
            // Chained re-enqueue is debounced rather than immediately respawned.
            spawn_rebuild_task_inner(rt, ann, model, key, true);
        }
    }));
}

/// Warm the global per-model ANN indexes at startup — skips already-loaded keys.
pub(crate) async fn warm_existing_memory_indexes(rt: &KhiveRuntime, ann: &SharedAnn) {
    if rt.is_read_only() {
        return;
    }
    let models = rt.registered_embedding_model_names();
    for model in &models {
        let token = match rt.authorize(Namespace::local()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let tracking = TrackedJob::register("memory_ann_startup");
        match submit_ann_attempt(rt, ann, model, token, &tracking).await {
            JobOutcome::Completed(Ok(status)) => {
                tracing::debug!(?status, model = %model, "memory ANN warm complete");
            }
            JobOutcome::Completed(Err(e)) => {
                tracing::warn!(error = %e, model = %model, "memory ANN warm failed");
            }
            JobOutcome::Cancelled | JobOutcome::FinalizationTimedOut | JobOutcome::Refused(_) => {
                return
            }
        }
    }
}

/// Whether `start_rotation_watcher` has claimed its one-shot guard for `ann`.
#[cfg(test)]
pub(crate) fn rotation_watch_started_for_test(ann: &SharedAnn) -> bool {
    ann.rotation_watch_started.load(Ordering::Acquire)
}

/// Start one pack-lifetime watcher that releases mmap file generations after
/// a peer checkpoint rotates them. The task holds only a weak ANN reference
/// between ticks, so dropping the pack ends it even in a non-daemon stdio
/// process; daemon shutdown is an immediate second exit path.
pub(crate) fn start_rotation_watcher(rt: &KhiveRuntime, ann: &SharedAnn) {
    let shutdown = khive_runtime::daemon_shutdown_token();
    let watcher = start_rotation_watcher_with_shutdown(rt, ann, shutdown);
    #[cfg(test)]
    rotation_watch_test_support::retain_rotation_watch_handle(ann, watcher);
    #[cfg(not(test))]
    drop(watcher);
}

pub(super) fn start_rotation_watcher_with_shutdown(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    shutdown: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let ann_root = rt.backend_ann_root()?;
    let watch = khive_retrieval::ann::rotation_watch_future(
        ann,
        &ann.rotation_watch_started,
        ann_root,
        ROTATION_WATCH_INTERVAL,
        shutdown,
        |ann, ann_root| async move {
            refresh_rotated_segments_in_root(&ann_root, &ann).await;
        },
    )?;
    Some(khive_runtime::spawn_named_tracked_task(
        "memory_ann_rotation_watch",
        watch,
    ))
}

/// Poll every installed mmap bridge once and replace any generation published
/// by another process. This performs no corpus scan on the unchanged path.
#[cfg(test)]
pub(super) async fn refresh_rotated_segments_once(rt: &KhiveRuntime, ann: &SharedAnn) {
    let Some(ann_root) = rt.backend_ann_root() else {
        return;
    };
    refresh_rotated_segments_in_root(&ann_root, ann).await;
}

pub(super) async fn refresh_rotated_segments_in_root(ann_root: &std::path::Path, ann: &SharedAnn) {
    let installed: Vec<(AnnKey, [u8; 32])> = ann
        .indexes
        .read()
        .await
        .iter()
        .filter_map(|(key, bridge)| bridge.commit_digest.map(|digest| (key.clone(), digest)))
        .collect();

    for (key, expected) in installed {
        let dir = ann_segment_dir_from_root(ann_root, &key.model);
        match delta::publication_digest(&dir) {
            Ok(Some(observed)) if observed != expected => {
                refresh_rotated_segment(ann, &key, expected, dir).await;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    model = %key.model,
                    "memory ANN rotation check failed; retaining the installed generation"
                );
            }
        }
    }
}

/// Reload one changed publication under the same local and cross-process
/// locks used by checkpoint writers. A committed-but-invalid replacement
/// evicts the predecessor: its segment files have already been unlinked, and
/// retaining it would recreate the disk-space leak this watcher prevents.
pub(super) async fn refresh_rotated_segment(
    ann: &SharedAnn,
    key: &AnnKey,
    expected: [u8; 32],
    dir: std::path::PathBuf,
) {
    let local_lock = model_warm_lock(ann, key).await;
    let _local_guard = local_lock.lock().await;

    let incumbent = ann.indexes.read().await.get(key).map(|bridge| {
        (
            bridge.commit_digest,
            bridge.generation,
            bridge.epoch_baseline,
            bridge.index.last_applied_seq().unwrap_or(0),
            bridge.published_seq,
            bridge.dirty_ops > 0,
        )
    });
    let Some((
        Some(incumbent_digest),
        generation,
        epoch_baseline,
        incumbent_seq,
        published_seq,
        dirty,
    )) = incumbent
    else {
        return;
    };
    if incumbent_digest != expected {
        return;
    }

    let _publication_guard = match khive_retrieval::ann::acquire_checkpoint_lock_async(
        dir.clone(),
        "memory ANN",
    )
    .await
    {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                error = %error,
                model = %key.model,
                "memory ANN rotation reload could not acquire the publication lock"
            );
            return;
        }
    };

    let observed = match delta::publication_digest(&dir) {
        Ok(Some(digest)) if digest != incumbent_digest => digest,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(
                error = %error,
                model = %key.model,
                "memory ANN rotated commit identity could not be read"
            );
            return;
        }
    };

    let replacement = load_segment(ann, &dir).and_then(|bridge| {
        if bridge.commit_digest != Some(observed) {
            return Err("commit identity changed during locked rotation reload".to_string());
        }
        let replacement_seq = bridge.index.last_applied_seq().unwrap_or(0);
        let protected_seq = if dirty { published_seq } else { incumbent_seq };
        if replacement_seq < protected_seq {
            return Err(format!(
                "rotated segment watermark {replacement_seq} regressed below installed {incumbent_seq}"
            ));
        }
        Ok(bridge
            .with_generation(generation)
            .with_epoch_baseline(epoch_baseline))
    });

    match replacement {
        Ok(bridge) => {
            // Do not carry the incumbent's namespace_set onto the rotated
            // bridge: a peer's checkpoint can cover namespaces this process
            // never observed. Leave the conservative empty set `load` set,
            // so recall keeps over-fetching until it repopulates.
            let must_replay = dirty && bridge.index.last_applied_seq().unwrap_or(0) < incumbent_seq;
            if install_replacing(ann, key, bridge).await {
                if must_replay {
                    // Release the old files, but retain freshness pressure.
                    // Recall merges the retained tail above the peer checkpoint.
                    bump_generation(ann, key).await;
                }
                tracing::debug!(
                    model = %key.model,
                    "memory ANN adopted rotated mmap generation and released its predecessor"
                );
            }
        }
        Err(error) => {
            let mut indexes = ann.indexes.write().await;
            if indexes
                .get(key)
                .is_some_and(|bridge| bridge.commit_digest == Some(incumbent_digest))
            {
                indexes.remove(key);
            }
            tracing::warn!(
                error = %error,
                model = %key.model,
                "memory ANN rotated generation failed validation; released predecessor and will rebuild on demand"
            );
        }
    }
}

/// Restore or rebuild one model graph under a shared single-flight lock.
///
/// The actual attempt emits one best-effort `ann_warm` phase span. See
/// `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
pub(crate) async fn ensure_ann_for_model(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> Result<AnnEnsureStatus, RuntimeError> {
    let (key, target_generation, force_full_rebuild, _single_flight_guard) =
        match prepare_ann_for_model(rt, ann, model).await? {
            AnnPreparation::Finished(status) => return Ok(status),
            AnnPreparation::Attempt {
                key,
                target_generation,
                force_full_rebuild,
                _guard,
            } => (key, target_generation, force_full_rebuild, _guard),
        };

    let phase_start = std::time::Instant::now();
    // Process CPU is cumulative, so phase attribution requires entry and exit snapshots.
    let cpu_start = khive_runtime::process_resource_usage();
    // RAII keeps `ann_warm` visible to health reporting on every exit path.
    let _phase_guard = khive_runtime::register_active_phase("ann_warm");
    // No corpus count here: the Hot adoption path performs zero corpus I/O
    // (ADR-079 Amendment 1), and an O(N) diagnostic COUNT on every warm would
    // defeat that. Vector counts are attributed at build completion instead.
    let corpus_size = None;
    emit_ann_warm_phase_event(
        rt,
        token,
        model,
        khive_types::EventKind::PhaseStarted,
        khive_storage::PhaseStartedPayload {
            work_class: "warm".into(),
            phase: "ann_warm".into(),
            corpus_size,
        },
    )
    .await;

    let mut details = AnnWarmDetails::default();
    let result = ensure_ann_for_model_inner(
        rt,
        token,
        ann,
        model,
        target_generation,
        force_full_rebuild,
        &mut details,
    )
    .await;

    let wall_us = phase_start.elapsed().as_micros() as i64;
    let cpu_us = khive_runtime::cpu_delta_us(cpu_start, khive_runtime::process_resource_usage());
    emit_ann_warm_terminal_phase(rt, token, model, &result, wall_us, cpu_us, details).await;
    if result.is_ok() {
        checkpoint_timer::schedule_checkpoint(rt, ann, &key).await;
    }
    result
}

pub(super) async fn emit_ann_warm_terminal_phase(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    result: &Result<AnnEnsureStatus, RuntimeError>,
    wall_us: i64,
    cpu_us: Option<i64>,
    mut details: AnnWarmDetails,
) {
    details.finish(result);
    match result {
        Err(e) if is_benign_shutdown_cancellation(e) => {
            emit_ann_warm_phase_event(
                rt,
                token,
                model,
                khive_types::EventKind::PhaseCancelled,
                khive_storage::PhaseCancelledPayload {
                    work_class: "warm".into(),
                    phase: "ann_warm".into(),
                    wall_us,
                    cpu_us,
                },
            )
            .await;
        }
        _ => {
            emit_ann_warm_phase_event(
                rt,
                token,
                model,
                khive_types::EventKind::PhaseCompleted,
                AnnWarmCompletedPayload {
                    phase: khive_storage::PhaseCompletedPayload {
                        work_class: "warm".into(),
                        phase: "ann_warm".into(),
                        wall_us,
                        cpu_us,
                    },
                    path: details.path,
                    ops_applied: details.ops_applied,
                },
            )
            .await;
        }
    }
}

/// Append a best-effort ANN warm phase event without changing the warm result.
pub(super) async fn emit_ann_warm_phase_event<P: serde::Serialize>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    kind: khive_types::EventKind,
    payload: P,
) {
    // Snapshot inspection returns before store acquisition or payload serialization.
    if rt.is_read_only() {
        return;
    }
    // A missing event store means auditing is unconfigured, not that warming failed.
    let result = crate::store_access::acquire_store("memory.ann.event_store", {
        let runtime = rt.clone();
        let token = token.clone();
        move || runtime.events(&token)
    })
    .await;
    let Ok(Ok(store)) = result else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_kind = %kind.name(),
                model,
                "failed to serialize ann_warm phase event payload"
            );
            return;
        }
    };
    let event = khive_storage::Event::new(
        token.namespace().as_str(),
        "memory.ann_warm",
        kind,
        khive_types::SubstrateKind::Event,
        "daemon:ann_warm",
    )
    .with_payload(payload_value);
    let append = store.append_event(event);
    #[cfg(test)]
    let append = super::finalization_timeout_tests::observe_append(append);
    if let Err(err) = append.await {
        tracing::warn!(
            error = %err,
            event_kind = %kind.name(),
            model,
            "ann_warm phase event append failed"
        );
    }
}

/// Restore or rebuild after the caller has established its generation floor.
pub(super) async fn ensure_ann_for_model_inner(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    target_generation: u64,
    force_full_rebuild: bool,
    details: &mut AnnWarmDetails,
) -> Result<AnnEnsureStatus, RuntimeError> {
    ensure_ann_for_model_inner_with_context(
        rt,
        token,
        ann,
        model,
        target_generation,
        force_full_rebuild,
        details,
        InteractiveAnn,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn ensure_ann_for_model_inner_with_context<C: AnnBuildContext>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    target_generation: u64,
    mut force_full_rebuild: bool,
    details: &mut AnnWarmDetails,
    context: C,
) -> Result<AnnEnsureStatus, C::Error> {
    let ns = "global";
    let key = AnnKey::new(model);

    if !force_full_rebuild
        && installed_is_fresh(ann, &key, target_generation).await
        && !checkpoint_due(ann, &key).await
    {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    // Stamp the epoch observed before this attempt; only a later reindex invalidates it.
    let target_epoch = durable_epoch(rt).await;
    let epoch_changed = ann
        .indexes
        .read()
        .await
        .get(&key)
        .is_some_and(|bridge| bridge.epoch_baseline != target_epoch);
    force_full_rebuild |= epoch_changed;
    if !force_full_rebuild && ann.builds_corpus_indexes {
        match maintain_installed(
            rt,
            ann,
            &key,
            model,
            target_generation,
            target_epoch,
            details,
        )
        .await?
        {
            InstalledMaintenance::Complete => return Ok(AnnEnsureStatus::AlreadyLoaded),
            InstalledMaintenance::Rebuild => force_full_rebuild = true,
            InstalledMaintenance::Absent => {}
        }
    }

    // v2 segment classifier (ADR-079 Amendment 1, global-scope addendum): the
    // 8-rule first-match decision table over the persisted commit record, this
    // consumer's wildcard registry row, and one same-snapshot (live, tail)
    // read. Replaces the retired JSON-snapshot content-hash gate.
    if !force_full_rebuild {
        if let Some(seg_dir) = ann_segment_dir(rt, model) {
            match classify_and_adopt_segment(
                rt,
                ann,
                &key,
                model,
                &seg_dir,
                target_generation,
                target_epoch,
                details,
            )
            .await
            {
                SegmentOutcome::Installed(status) => return Ok(status),
                SegmentOutcome::Empty => {
                    tracing::debug!(namespace = %ns, model = %model, "memory ANN: zero live corpus");
                    return Ok(AnnEnsureStatus::EmptyCorpus);
                }
                SegmentOutcome::Cold => {}
            }
        }
    }

    if !ann.builds_corpus_indexes {
        tracing::info!(namespace = %ns, model = %model,
            "no adoptable memory ANN segment and this process does not build corpus \
             indexes; serving degraded and leaving the build to the daemon");
        return Ok(AnnEnsureStatus::DeclinedNotWarmHost);
    }

    // The fingerprint sandwich bounds scan races; generation ordering closes the
    // later persistence/install window and prevents an older build from winning.
    details.path = "full_build";
    let empty_corpus_fence = ann
        .indexes
        .read()
        .await
        .get(&key)
        .map(MaintenanceFence::capture);
    let fp_before = compute_memory_fingerprint(rt, token, model).await;
    let built = if C::IS_INTERACTIVE {
        load_and_build_from_vector_store(rt, token, model)
            .await
            .map_err(C::Error::from)
    } else {
        load_and_build_from_vector_store_with_context(rt, token, model, context).await
    };
    match built {
        Ok(Some(bridge)) => {
            let fp_after = compute_memory_fingerprint(rt, token, model).await;
            // If fingerprint changed during the scan, the corpus raced; discard.
            if fp_before != fp_after {
                tracing::debug!(
                    namespace = %ns,
                    model = %model,
                    "memory ANN corpus mutated during build; discarding"
                );
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            let vector_count = bridge.id_map.len();
            let installed = checkpoint_raise_compact_readopt(
                rt,
                ann,
                &key,
                model,
                bridge,
                CheckpointPublication {
                    generation: target_generation,
                    epoch: target_epoch,
                    authority: WatermarkAuthority::PendingOrActive,
                },
            )
            .await;
            if !installed {
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            tracing::debug!(namespace = %ns, model = %model, vectors = vector_count, "memory ANN index built");
            Ok(AnnEnsureStatus::Built {
                vectors: vector_count,
            })
        }
        Ok(None) => {
            // An empty rebuild has no replacement to retire an incumbent retained
            // after private replay failed. Evict only that unchanged incarnation.
            khive_storage::ensure_request_read_active("memory.ann.empty_corpus")?;
            if empty_corpus_fence.is_some()
                && fp_before
                    .as_ref()
                    .is_none_or(|fingerprint| fingerprint.vector_count != 0)
            {
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            if durable_epoch(rt).await != target_epoch {
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            let retired = {
                let mut indexes = ann.indexes.write().await;
                let unchanged = match &empty_corpus_fence {
                    Some(fence) => indexes
                        .get(&key)
                        .is_some_and(|bridge| fence.matches(bridge)),
                    None => !indexes.contains_key(&key),
                };
                if !unchanged {
                    return Ok(AnnEnsureStatus::DiscardedStaleBuild);
                }
                khive_storage::ensure_request_read_active("memory.ann.empty_corpus")?;
                indexes.remove(&key)
            };
            drop(retired);
            tracing::debug!(namespace = %ns, model = %model, "memory ANN: no note vectors to build");
            Ok(AnnEnsureStatus::EmptyCorpus)
        }
        Err(error) => Err(error),
    }
}

// ── corpus loading ────────────────────────────────────────────────────────────

/// Compute a fingerprint for all non-deleted memory note vectors for `model` (all namespaces).
pub(super) async fn compute_memory_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let result = crate::store_access::acquire_store("memory.ann.fingerprint_store", {
        let runtime = rt.clone();
        let token = token.clone();
        let model = model.to_owned();
        move || runtime.vectors_for_model(&token, &model)
    })
    .await;
    let store = result.ok()?.ok()?;
    let info = store.info().await.ok()?;
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(memory_corpus().fingerprint(&table_name, model, "memory_ann_fingerprint"))
        .await
        .ok()?;
    let vector_count = match rows.first()?.get("n")? {
        SqlValue::Integer(n) if *n >= 0 => *n as u64,
        _ => return None,
    };
    Some(CorpusFingerprint {
        vector_count,
        dimensions: info.dimensions as u32,
    })
}

/// Build a graph from live model vectors, capturing the corpus watermark in
/// the same statement as the scan (ADR-079 Amendment 1 linearization; see
/// `docs/api/ann-lifecycle.md`). Global-scope, join-filtered: every namespace,
/// live notes only.
pub(super) async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
    let result =
        load_and_build_from_vector_store_with_context(rt, token, model, InteractiveAnn).await;
    match &result {
        Err(error) if is_benign_shutdown_cancellation(error) => {
            tracing::debug!(%error, model, "memory ANN build cancelled at shutdown")
        }
        Err(error) => tracing::warn!(%error, model, "memory ANN build failed"),
        _ => {}
    }
    result
}

pub(super) async fn load_and_build_from_vector_store_with_context<C: AnnBuildContext>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    context: C,
) -> Result<Option<AnnBridge>, C::Error> {
    let result = crate::store_access::acquire_store("memory.ann.vector_store", {
        let runtime = rt.clone();
        let token = token.clone();
        let model = model.to_owned();
        move || runtime.vectors_for_model(&token, &model)
    })
    .await?;
    let store = match result {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    // Preserve typed storage/join errors so shutdown cancellation remains classifiable.
    let info = store.info().await?;
    if info.dimensions == 0 {
        return Ok(None);
    }
    let dims = info.dimensions;

    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");

    let sql = rt.sql();
    let mut reader = sql.reader().await?;

    let rows = reader
        .query_all(memory_corpus().corpus_scan(&table_name, model, "memory_ann_corpus_scan"))
        .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let scan_watermark = rows
        .first()
        .and_then(|row| match row.get("log_s") {
            Some(SqlValue::Integer(n)) => u64::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0);

    let mut id_map: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut flat: Vec<f32> = Vec::with_capacity(rows.len() * dims);
    let mut namespace_set: HashSet<String> = HashSet::new();

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
        if let Some(SqlValue::Text(ns)) = row.get("namespace") {
            namespace_set.insert(ns.clone());
        }
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }

    if id_map.is_empty() {
        return Ok(None);
    }

    // Same admitted lease/tracker lives in the actual controlled CPU closure.
    let mut built = context.build(flat, dims, id_map, namespace_set).await?;
    built.set_applied_seq(scan_watermark);
    Ok(Some(built))
}

// ── ADR-079 Amendment 1: segments, registry, classifier (global scope) ────────

/// Global-scope v2 segment directory for `model`: `<db-file>.ann/<hex(snapshot_key)>`
/// (see `docs/api/ann-lifecycle.md` for the segment-root layout). `None` for
/// in-memory backends.
pub(super) fn ann_segment_dir(rt: &KhiveRuntime, model: &str) -> Option<std::path::PathBuf> {
    let ann_root = rt.backend_ann_root()?;
    Some(ann_segment_dir_from_root(&ann_root, model))
}

pub(super) fn ann_segment_dir_from_root(
    ann_root: &std::path::Path,
    model: &str,
) -> std::path::PathBuf {
    let key = snapshot_key("global", model);
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    ann_root.join(hex)
}

/// Install `candidate`, replacing an equal-or-newer-generation incumbent but
/// never a strictly newer one (see `docs/api/ann-lifecycle.md` for why equal
/// generations still replace). Returns whether `candidate` was installed.
pub(super) async fn install_replacing(ann: &SharedAnn, key: &AnnKey, candidate: AnnBridge) -> bool {
    match ann.indexes.write().await.entry(key.clone()) {
        std::collections::hash_map::Entry::Occupied(mut e)
            if e.get().generation <= candidate.generation =>
        {
            e.insert(candidate);
            true
        }
        std::collections::hash_map::Entry::Occupied(_) => {
            tracing::debug!(
                model = %key.model,
                "memory ANN replace skipped: cached entry is newer than this build"
            );
            false
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(candidate);
            true
        }
    }
}
