use super::*;

/// Run one already-owned warm attempt to completion.
pub(super) async fn run_warm_attempt(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    permit: AnnWarmPermit,
) {
    let outcome = ensure_ann_for_model(rt, token, ann, model).await;
    finish_warm(permit, outcome).await;
}

/// Await one single-flight warm for the explicit model. Used by both v1 and
/// v2 startup discovery so they share the same lifecycle while retaining the
/// preload path's existing await-before-next-key timing.
pub(super) async fn warm_ann_for_model_once(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) {
    if model.is_empty() {
        return;
    }
    let key = AnnKey::new(token.namespace().as_str(), model);
    let Some(permit) = begin_warm(ann, key) else {
        return;
    };
    run_warm_attempt(rt, token, ann, model, permit).await;
}

/// Pre-load Vamana snapshots for all `{ns}::vamana::{model}` keys found in
/// `retrieval_snapshots`.  Called from `KnowledgePack::warm()` before the first
/// search request so in-memory indexes are ready without a first-query spike.
///
/// Each unique namespace+model pair gets its own keyed slot; all snapshots are
/// loaded, not just the first one.
pub(crate) async fn warm_known_snapshots(rt: &KhiveRuntime, ann: &SharedAnn) {
    // v1 legacy pass: warm namespaces recorded in retrieval_snapshots, if that
    // table exists. On a v2-only database it will not, so a query error must fall
    // through to the v2 segment enumeration below rather than abort the warm pass.
    let rows = {
        let sql = rt.sql();
        match sql.reader().await {
            Ok(mut reader) => reader
                .query_all(SqlStatement {
                    sql: ::khive_runtime::sql!("knowledge_legacy_snapshot_namespaces").into(),
                    params: vec![SqlValue::Text("%::vamana::%".into())],
                    label: None,
                })
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };

    for row in &rows {
        let ns_key = match row.get("namespace") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let Some((ns_str, model)) = ns_key.split_once("::vamana::") else {
            continue;
        };
        if ns_str.is_empty() || model.is_empty() {
            continue;
        }
        let ns = match Namespace::parse(ns_str) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let token = match rt.authorize(ns) {
            Ok(t) => t,
            Err(_) => continue,
        };
        warm_ann_for_model_once(rt, &token, ann, model).await;
    }

    // Enumerate v2 segment directories under this database's own ANN root and
    // warm any keys not already loaded by the v1 DB pass above.
    let ann_root = match rt.backend_ann_root() {
        Some(d) => d,
        None => return,
    };
    let read_dir = match std::fs::read_dir(&ann_root) {
        Ok(rd) => rd,
        Err(_) => return, // no ann/ dir yet — nothing to warm
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let hex = name.to_string_lossy();
        let Some((ns_str, model)) = decode_ann_dir_name(hex.as_ref()) else {
            continue;
        };
        let ns = match Namespace::parse(&ns_str) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let token = match rt.authorize(ns) {
            Ok(t) => t,
            Err(_) => continue,
        };
        warm_ann_for_model_once(rt, &token, ann, &model).await;
    }
}

/// Whether `start_rotation_watcher` has claimed its one-shot guard for `ann`.
#[cfg(test)]
pub(crate) fn rotation_watch_started_for_test(ann: &SharedAnn) -> bool {
    ann.rotation_watch_started.load(Ordering::Acquire)
}

/// Start one pack-lifetime watcher that releases mmap generations after peer
/// checkpoints rotate their files. The task retains only a weak ANN reference
/// between ticks and also exits immediately on daemon shutdown.
pub(crate) fn start_rotation_watcher(rt: &KhiveRuntime, ann: &SharedAnn) {
    drop(start_rotation_watcher_with_shutdown(
        rt,
        ann,
        khive_runtime::daemon_shutdown_token(),
    ));
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
        "knowledge_ann_rotation_watch",
        watch,
    ))
}

/// Poll the commit identities of currently installed mmap bridges once.
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
        let dir = ann_segment_dir_from_root(ann_root, &key.namespace, &key.model);
        match segment_commit_digest(&dir) {
            Ok(Some(observed)) if observed != expected => {
                refresh_rotated_segment(ann, &key, expected, dir).await;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    namespace = %key.namespace,
                    model = %key.model,
                    "knowledge ANN rotation check failed; retaining the installed generation"
                );
            }
        }
    }
}

/// Evict `key`'s bridge if it is still the one identified by
/// `incumbent_digest`, and clear a `Ready` warm state at the same
/// `generation`, under one `indexes` write-lock critical section.
///
/// Keeping both mutations inside one critical section closes the split-lock
/// race with `finish_warm`'s Ready publish (issue #2340): `finish_warm`
/// decides Ready and publishes it to `warm_states` while still holding the
/// `indexes` read guard it used for the decision, so whichever side's
/// critical section runs second observes the other's completed effect —
/// never a half-applied state where `warm_states` says `Ready` and no
/// bridge is installed. Keying the state cleanup on the evicted bridge's own
/// generation (not "any Ready") leaves an unrelated build's state alone.
pub(super) async fn evict_bridge_and_ready_state(
    ann: &SharedAnn,
    key: &AnnKey,
    incumbent_digest: [u8; 32],
    generation: u64,
) -> bool {
    let mut indexes = ann.indexes.write().await;
    if indexes
        .get(key)
        .is_none_or(|bridge| bridge.commit_digest != Some(incumbent_digest))
    {
        return false;
    }
    indexes.remove(key);
    // Only a Ready state at the evicted bridge's own generation describes it.
    // A Warming state belongs to an in-flight warm whose permit still owns
    // the single-flight slot; removing it would let a second warm start
    // alongside the first, and a Ready state at a different generation
    // belongs to a later build this eviction has nothing to say about.
    let mut states = warm_states_guard(&ann.warm_states);
    if matches!(
        states.get(key),
        Some(AnnWarmState::Ready { generation: state_generation }) if *state_generation == generation
    ) {
        states.remove(key);
    }
    true
}

/// Adopt one changed publication under the same lock order as checkpoint
/// writers: process-local key lock, then cross-process bridge lock. A changed
/// but invalid publication evicts the predecessor because its files have
/// already been unlinked; the next search may retry the ordinary warm path.
pub(super) async fn refresh_rotated_segment(
    ann: &SharedAnn,
    key: &AnnKey,
    expected: [u8; 32],
    dir: std::path::PathBuf,
) {
    let local_lock = checkpoint_lock(ann, key);
    let _local_guard = local_lock.lock().await;

    let incumbent = ann.indexes.read().await.get(key).map(|bridge| {
        (
            bridge.commit_digest,
            bridge.generation,
            bridge.index.last_applied_seq().unwrap_or(0),
        )
    });
    let Some((Some(incumbent_digest), generation, incumbent_seq)) = incumbent else {
        return;
    };
    if incumbent_digest != expected {
        return;
    }

    let _publication_guard = match khive_retrieval::ann::acquire_checkpoint_lock_async(
        dir.clone(),
        BRIDGE_LOCK_MESSAGE_PREFIX,
    )
    .await
    {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = %key.namespace,
                model = %key.model,
                "knowledge ANN rotation reload could not acquire the publication lock"
            );
            return;
        }
    };
    let observed = match segment_commit_digest(&dir) {
        Ok(Some(digest)) if digest != incumbent_digest => digest,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = %key.namespace,
                model = %key.model,
                "knowledge ANN rotated commit identity could not be read"
            );
            return;
        }
    };

    let replacement = AnnBridge::load(&dir).and_then(|bridge| {
        if bridge.commit_digest != Some(observed) {
            return Err("commit identity changed during locked rotation reload".to_string());
        }
        let replacement_seq = bridge.index.last_applied_seq().unwrap_or(0);
        if replacement_seq < incumbent_seq {
            return Err(format!(
                "rotated segment watermark {replacement_seq} regressed below installed {incumbent_seq}"
            ));
        }
        Ok(bridge.with_generation(generation))
    });

    match replacement {
        Ok(bridge) => {
            if install_replacing(ann, key, bridge).await {
                tracing::debug!(
                    namespace = %key.namespace,
                    model = %key.model,
                    "knowledge ANN adopted rotated mmap generation and released its predecessor"
                );
            }
        }
        Err(error) => {
            evict_bridge_and_ready_state(ann, key, incumbent_digest, generation).await;
            tracing::warn!(
                error = %error,
                namespace = %key.namespace,
                model = %key.model,
                "knowledge ANN rotated generation failed validation; released predecessor and will rebuild on demand"
            );
        }
    }
}

/// Spawn one per-key background warm and return immediately. Current-generation
/// `Warming`/`Ready` states suppress duplicates; `Failed` remains retryable by
/// the next search.
pub(crate) fn ensure_ann_background(rt: &KhiveRuntime, token: &NamespaceToken, ann: &SharedAnn) {
    // Searches against a frozen snapshot may use FTS, an already-loaded
    // bridge, and the load-only fresh-tail leg, but must not turn a cache miss
    // into consumer registration or checkpoint publication.
    if rt.is_read_only() {
        return;
    }
    let model = rt.default_embedder_name().to_string();
    if model.is_empty() {
        return;
    }
    let ns = token.namespace().as_str().to_owned();
    let key = AnnKey::new(&ns, &model);
    let Some(permit) = begin_warm(ann, key) else {
        return;
    };

    let rt = rt.clone();
    let ann = ann.clone();
    // Preserve the request-minted actor/visibility context (ADR-096). Reauthorizing
    // from the namespace here would silently replace it with runtime defaults.
    let token = token.clone();
    // Deliberately detached cache maintenance: this warm attempt is shared
    // across later requests and must not inherit one caller's cancellation or
    // deadline. Request-owned ANN/search fan-out is scoped at its spawn sites.
    tokio::spawn(async move {
        run_warm_attempt(&rt, &token, &ann, &model, permit).await;
    });
}

/// Outcome of the v2-segment decision table for one `(namespace, model)` scope.
pub(super) enum SegmentOutcome {
    /// An index was installed (Hot, Stale-tail, or a served Stale-rebuild
    /// segment whose replacement rebuild the caller must still run — those
    /// return Cold instead so the rebuild path fires).
    Installed,
    /// Live corpus is zero: no ANN candidate may be served or replayed
    /// (decision rule 5). Caller records the terminal unavailable marker.
    Empty,
    /// No trustworthy segment: fall through to the v1 / rebuild paths.
    Cold,
    /// Registry loss or its durable marker requires a full corpus scan.  The
    /// caller must bypass both v2 adoption and the legacy-v1 fallback.
    ForceRebuild,
}

/// ADR-079 Amendment 1 restart classifier (the 8-rule first-match decision
/// table), evaluated for one consumer scope, followed by the matching
/// adoption action. Replaces the retired full-corpus content-hash gate.
#[allow(clippy::too_many_arguments)]
pub(super) async fn classify_and_adopt_segment(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    key: &AnnKey,
    ns: &str,
    model: &str,
    seg_dir: &std::path::Path,
    target_generation: u64,
) -> SegmentOutcome {
    if force_rebuild_required(ann, key) {
        return SegmentOutcome::ForceRebuild;
    }

    // Rule 1: commit record absent, corrupt, or invalid length → Cold.
    let info = match read_commit_info(seg_dir) {
        Ok(Some(info)) => info,
        Ok(None) => return SegmentOutcome::Cold,
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "error reading v2 commit record; Cold");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 2: readable but pre-amendment (no watermark) → Cold. Compaction
    // stays blocked naturally: this consumer's pending (`-2`) or recovery
    // (`-1`) row holds the pair MIN below every log sequence.
    let Some(s) = info.last_applied_seq else {
        tracing::info!(namespace = %ns, model = %model,
            "pre-amendment v2 segment (no watermark); Cold rebuild");
        return SegmentOutcome::Cold;
    };

    // Rule 3: configured embedder dimensions ≠ segment dimensions → Cold.
    // Resolved from the embedder registry — no storage access. The corpus
    // is normally touched by one bounded statement after the tail probe:
    // `classification_scope_counts` below (with an exact-query fallback).
    match rt.embedder_dimensions(model) {
        Some(dims) if dims as u64 == info.dimensions => {}
        Some(dims) => {
            tracing::info!(namespace = %ns, model = %model,
                segment_dims = info.dimensions, live_dims = dims,
                "v2 segment dimension mismatch; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        None => return SegmentOutcome::Cold,
    }

    // Rule 4: an absent row or the durable -1 sentinel requires an
    // authoritative full-corpus rebuild.  The sentinel is cross-process and
    // keeps pair compaction blocked until that rebuild publishes.
    match read_own_watermark(rt, ns, model).await {
        Ok(Some(watermark)) if watermark >= 0 => {}
        Ok(Some(_)) => {
            mark_force_rebuild(ann, key);
            return SegmentOutcome::ForceRebuild;
        }
        Ok(None) => {
            tracing::info!(namespace = %ns, model = %model,
                "ann consumer registry row absent; fencing for authoritative rebuild");
            if let Err(error) = prepare_authoritative_rebuild(rt, ann, key).await {
                tracing::warn!(error = %error, "ann consumer re-registration failed");
            }
            return SegmentOutcome::ForceRebuild;
        }
        Err(e) => {
            tracing::warn!(error = %e, "ann registry read failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // Rule 6, tested first per the amendment's evaluation-order note: the
    // tail predicate is a log-table-only index probe, so the Hot path never
    // touches the vec0 corpus at all. With an empty tail the committed
    // segment already reflects every logged op at or below S, so a zero-live
    // scope implies an empty segment and adoption serves exactly what Empty
    // serves.
    match tail_exists(rt, ns, model, s).await {
        Ok(false) => {
            return match AnnBridge::load(seg_dir) {
                Ok(bridge) => {
                    install_if_fresher(ann, key, bridge.with_generation(target_generation)).await;
                    SegmentOutcome::Installed
                }
                Err(e) => {
                    tracing::warn!(error = %e, dir = %seg_dir.display(),
                        "Hot segment load failed; Cold rebuild");
                    SegmentOutcome::Cold
                }
            };
        }
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, "ann tail probe failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // A tail exists. Rules 5, 7, and 8 use one snapshot; a short tail
    // usually needs only a bounded scope count, not a full vec0 scan.
    let rebuild_fraction = ann_rebuild_threshold();
    let counts = match classification_scope_counts(rt, ns, model, s, rebuild_fraction).await {
        Ok(counts) => counts,
        Err(error) => {
            tracing::warn!(error = %error, "bounded ANN scope count failed; retrying exact count");
            match scope_counts(rt, ns, model, s).await {
                Ok((live, tail)) => ClassificationScopeCounts {
                    live_lower_bound: live,
                    tail,
                    live_count_exact: true,
                },
                Err(e) => {
                    tracing::warn!(error = %e, "ann scope-count read failed; Cold");
                    return SegmentOutcome::Cold;
                }
            }
        }
    };
    let mut live = counts.live_lower_bound;
    let mut tail = counts.tail;

    let mut threshold = (rebuild_fraction * live as f64).ceil() as u64;
    // A saturated cap must prove Stale-tail. This fallback keeps the exact
    // decision if floating-point or SQLite arithmetic ever violates that
    // conservative bound; its fresh exact query is internally one snapshot.
    if !counts.live_count_exact && tail > threshold {
        match scope_counts(rt, ns, model, s).await {
            Ok((exact_live, exact_tail)) => {
                live = exact_live;
                tail = exact_tail;
                threshold = (rebuild_fraction * live as f64).ceil() as u64;
            }
            Err(e) => {
                tracing::warn!(error = %e, "ANN exact scope-count fallback failed; Cold");
                return SegmentOutcome::Cold;
            }
        }
    }

    // Rule 5: zero live corpus → Empty, regardless of tail contents.
    if live == 0 {
        tracing::info!(namespace = %ns, model = %model,
            "zero live corpus for scope; Empty (FTS/degraded path)");
        return SegmentOutcome::Empty;
    }

    // Rule 7: tail within threshold → Stale-tail: mmap load + final-state
    // replay, then checkpoint so the next restart's tail starts empty and the
    // served bridge returns to mmap backing.
    if tail <= threshold {
        let mut bridge = match AnnBridge::load(seg_dir) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, dir = %seg_dir.display(),
                    "Stale-tail segment load failed; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        let (finals, new_s) = match fetch_final_states(rt, ns, model, s).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "tail replay contradiction; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        if let Err(e) = replay_final_states(rt, token, &mut bridge, ns, model, &finals).await {
            tracing::warn!(error = %e, "tail replay failed; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        bridge.set_applied_seq(new_s);
        // Replay is cheap and in memory; the checkpoint that follows it is a full
        // segment publication. A process that is not the warm index host serves the
        // replayed bridge and publishes nothing — same answers, no rewrite for every
        // other reader on the root to absorb.
        if !ann.builds_corpus_indexes {
            install_if_fresher(ann, key, bridge.with_generation(target_generation)).await;
            return SegmentOutcome::Installed;
        }
        let checkpointed = checkpoint_raise_compact_readopt(
            rt,
            ann,
            key,
            bridge,
            target_generation,
            CheckpointAuthority::Incremental,
        )
        .await;
        if !checkpointed && force_rebuild_required(ann, key) {
            return SegmentOutcome::ForceRebuild;
        }
        return SegmentOutcome::Installed;
    }

    // Rule 8: tail above threshold → Stale-rebuild: serve the checksum-valid
    // segment while the caller's rebuild path replaces it (`install_replacing`
    // on completion). Cost decision, never a demotion to Cold/FTS-only.
    match AnnBridge::load(seg_dir) {
        Ok(bridge) => {
            tracing::info!(namespace = %ns, model = %model, tail, live,
                "tail above rebuild threshold; serving stale segment during rebuild");
            install_if_fresher(ann, key, bridge.with_generation(target_generation)).await;
            // Rule 8 serves stale and lets the caller rebuild. Only the warm index
            // host has a rebuild to fall through to; for anyone else the stale
            // segment IS the answer for this request.
            if !ann.builds_corpus_indexes {
                return SegmentOutcome::Installed;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "Stale-rebuild segment load failed; rebuilding without serve-stale");
        }
    }
    SegmentOutcome::Cold
}

/// Lazy warm-load for a specific `model`. Load order (first hit wins): (1)
/// in-memory cache fast path, (2) v2 segment directory (ADR-079 Amendment 1
/// write-log restart classifier — see `classify_and_adopt_segment`), (3)
/// legacy v1 JSON snapshot, (4) full corpus rebuild, atomically persisted
/// as v2 for next restart. See
/// crates/khive-pack-knowledge/docs/api/vamana.md#ensure_ann_for_model-load-order
/// for the per-step detail. The explicit outcome lets the lifecycle retain a
/// served stale fallback while keeping a failed replacement retryable.
pub(crate) async fn ensure_ann_for_model(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> AnnWarmOutcome {
    if model.is_empty() {
        return AnnWarmOutcome::Empty;
    }
    let ns = token.namespace().as_str().to_owned();
    let key = AnnKey::new(&ns, model);

    // A corpus-scale build is minutes of CPU and a segment rewrite every other
    // reader on the root must then take. It pays for itself only in a process
    // that outlives the request, so only the warm daemon does it; everyone else
    // serves what is already persisted, or serves degraded and lets the daemon
    // build. The same answer gates the registry writes below: publishing a
    // rebuild sentinel is an authority act, and a process that will not do the
    // rebuild must not claim it.
    let warm_host = ann.builds_corpus_indexes;

    // Registration precedes every scan or legacy serve. Local absence of a
    // bridge cannot prove global first use: a peer may still hold stale v1 or
    // Owned state after this row was administratively removed. Every absent
    // knowledge row therefore publishes the durable force-rebuild sentinel
    // before any further serving, even in a fresh process.
    let mut force_rebuild = force_rebuild_required(ann, &key);
    match read_own_watermark(rt, &ns, model).await {
        Ok(Some(watermark)) if watermark < 0 => {
            if !warm_host {
                tracing::debug!(namespace = %ns, model = %model,
                    "ANN rebuild is pending and this process is not the warm index host; \
                     declining");
                return AnnWarmOutcome::Declined;
            }
            mark_force_rebuild(ann, &key);
            force_rebuild = true;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            if !warm_host {
                tracing::debug!(namespace = %ns, model = %model,
                    "ANN registry row absent and this process is not the warm index host; \
                     declining rather than claiming the rebuild");
                return AnnWarmOutcome::Declined;
            }
            if let Err(error) = prepare_authoritative_rebuild(rt, ann, &key).await {
                tracing::warn!(error = %error, "failed to fence ANN registry loss");
                return AnnWarmOutcome::Failed;
            }
            force_rebuild = true;
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to read ANN registration before scan");
            return AnnWarmOutcome::Failed;
        }
    }
    if force_rebuild && !matches!(read_own_watermark(rt, &ns, model).await, Ok(Some(-1))) {
        if let Err(error) = prepare_authoritative_rebuild(rt, ann, &key).await {
            tracing::warn!(error = %error, "failed to establish ANN rebuild sentinel");
            return AnnWarmOutcome::Failed;
        }
    }

    // Capture the namespace's write-generation BEFORE anything else (issue
    // #770) — including before the fast path below and before the corpus
    // scan — so a write that lands after this point is guaranteed to be
    // reflected as a higher generation than anything this build can install.
    let target_generation = current_generation(ann, &ns);

    // 1. Fast path: already loaded AND at least as fresh as this namespace's
    // current generation (PR #815). A present entry with a
    // stale generation is not a hit — mere presence let a pre-invalidation
    // build served from an emptied-then-refilled slot serve indefinitely.
    // Falling through here re-enters the same rebuild path a genuine cache
    // miss would take.
    if !force_rebuild {
        if let Some(loaded_generation) = ann
            .indexes
            .read()
            .await
            .get(&key)
            .map(|bridge| bridge.generation)
        {
            if loaded_generation >= target_generation {
                return AnnWarmOutcome::Ready;
            }
            tracing::debug!(
                namespace = %ns,
                model = %model,
                loaded_generation,
                target_generation,
                "knowledge ANN fast path skipped: cached entry generation stale; rebuilding"
            );
        }
    }

    // 2. v2 segment path — ADR-079 Amendment 1 watermark classifier. Total,
    // first-match decision table over the persisted commit record, this
    // consumer's registry row, and one same-snapshot (live, tail) read.
    if !force_rebuild {
        if let Some(seg_dir) = ann_segment_dir(rt, &ns, model) {
            match classify_and_adopt_segment(
                rt,
                token,
                ann,
                &key,
                &ns,
                model,
                &seg_dir,
                target_generation,
            )
            .await
            {
                SegmentOutcome::Installed => return AnnWarmOutcome::Ready,
                SegmentOutcome::Empty => {
                    mark_unavailable(ann, &key, target_generation);
                    return AnnWarmOutcome::Empty;
                }
                SegmentOutcome::Cold => {} // fall through to v1 / rebuild
                SegmentOutcome::ForceRebuild => force_rebuild = true,
            }
        }
    }

    // 3. v1 JSON snapshot path (backwards-compat transition).
    if !force_rebuild {
        if let Some(snapshot) = try_load_snapshot(rt, &ns, model).await {
            let current_fp = compute_fingerprint(rt, token, model).await;
            if let Some(fp) = current_fp {
                if snapshot.fingerprint == fp {
                    match AnnBridge::from_vamana_snapshot(snapshot) {
                        Ok(bridge) => {
                            install_if_fresher(
                                ann,
                                &key,
                                bridge.with_generation(target_generation),
                            )
                            .await;
                            return AnnWarmOutcome::Ready;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "corrupt Vamana v1 snapshot; rebuilding");
                        }
                    }
                } else {
                    tracing::info!(
                        namespace = %ns,
                        model = %model,
                        "stale Vamana v1 snapshot (fingerprint mismatch); rebuilding"
                    );
                }
            }
        }
    }

    // 4. Rebuild fallthrough — build from vector store, persist and re-adopt
    // the v2 segment, then raise the registry watermark and compact the log.
    if !warm_host {
        tracing::info!(namespace = %ns, model = %model,
            "no adoptable ANN segment and this process is not the warm index host; \
             serving degraded and leaving the corpus build to the daemon");
        return AnnWarmOutcome::Declined;
    }
    let scan_authority = match prepare_full_corpus_scan(rt, ann, &key).await {
        Ok(authority) => authority,
        Err(error) => {
            tracing::warn!(error = %error, "failed to establish ANN full-scan authority");
            return AnnWarmOutcome::Failed;
        }
    };
    match load_and_build_from_vector_store(rt, token, model).await {
        Ok(Some(bridge)) => {
            let checkpointed = checkpoint_raise_compact_readopt(
                rt,
                ann,
                &key,
                bridge,
                target_generation,
                scan_authority,
            )
            .await;
            if checkpointed {
                AnnWarmOutcome::Ready
            } else if force_rebuild_required(ann, &key) {
                AnnWarmOutcome::Failed
            } else if has_current_index(ann, &key).await {
                // A normal cold build may still install an Owned bridge when
                // persistence fails, and a lost FullSentinel race may adopt
                // the winner while reporting a fenced local publication.
                AnnWarmOutcome::Ready
            } else {
                AnnWarmOutcome::Failed
            }
        }
        Ok(None) => {
            // Empty corpus: this scan (at target_generation) proves nothing is
            // buildable right now. Mark it so wait_ready can short-circuit
            // instead of polling out the full warm-wait timeout (issue #1026).
            mark_unavailable(ann, &key, target_generation);
            // An authoritative Empty scan keeps the durable -1 sentinel: no
            // segment exists whose publication could advance the row safely.
            // The first subsequent vector write invalidates this terminal
            // marker and the next full scan checkpoints normally.
            AnnWarmOutcome::Empty
        }
        Err(e) => {
            // Operational failure (store open, SQL reader, corpus query) —
            // not proof the corpus is unbuildable. Do NOT mark unavailable:
            // the caller transitions the warm state to retryable `Failed`, and
            // a marker here would make wait_ready short-circuit false while
            // that retry is in flight.
            tracing::warn!(error = %e, "failed to rebuild Vamana ANN index");
            AnnWarmOutcome::Failed
        }
    }
}

/// Simulate an in-flight warm without populating the index. Call this in tests
/// to construct the state that triggers the cold-start guard in
/// `suggest`/`search`.
#[cfg(test)]
pub(crate) fn simulate_warming_in_flight(ann: &SharedAnn, key: AnnKey) {
    begin_warm(ann, key)
        .expect("fresh test ANN state must accept a warm")
        .leave_in_flight_for_test();
}
