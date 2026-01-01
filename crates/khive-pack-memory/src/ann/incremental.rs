//! Live bridge maintenance between durable segment publications.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct CheckpointPolicy {
    pub(super) max_dirty_ops: u64,
    pub(super) interval: std::time::Duration,
    pub(super) consolidate_tau: usize,
    pub(super) rebuild_fraction: f64,
}

impl CheckpointPolicy {
    pub(super) fn from_env() -> Self {
        fn number(name: &str, default: u64) -> u64 {
            std::env::var(name)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        }
        Self {
            max_dirty_ops: number("KHIVE_ANN_CHECKPOINT_OPS", 1_000).max(1),
            interval: std::time::Duration::from_secs(number("KHIVE_ANN_CHECKPOINT_SECS", 300)),
            consolidate_tau: usize::try_from(number("KHIVE_ANN_CONSOLIDATE_TAU", 40_000))
                .unwrap_or(40_000)
                .max(1),
            rebuild_fraction: ann_rebuild_threshold(),
        }
    }

    fn dirty_limit(self, live: usize) -> u64 {
        // Leave three quarters of the restart replay allowance as headroom.
        // Tiny corpora checkpoint at one operation (restart uses ceil).
        self.max_dirty_ops
            .min((self.rebuild_fraction * live as f64 / 4.0).floor() as u64)
            .max(1)
    }

    fn due(self, bridge: &AnnBridge) -> bool {
        bridge.dirty_ops > 0
            && (bridge.dirty_ops >= self.dirty_limit(bridge.index.live_count())
                || (!self.interval.is_zero() && bridge.last_checkpoint.elapsed() >= self.interval))
    }
}

pub(super) fn checkpoint_policy(ann: &SharedAnn) -> CheckpointPolicy {
    *ann.checkpoint_policy
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The replay-versus-rebuild cost boundary is independent of the cumulative
/// delta-chain compaction limit (ADR-079 Amendment 1, restart rule 7).
pub(super) fn replay_limit(live: u64, rebuild_fraction: f64) -> u64 {
    (rebuild_fraction * live as f64).ceil() as u64
}

pub(super) async fn checkpoint_due(ann: &SharedAnn, key: &AnnKey) -> bool {
    if !ann.builds_corpus_indexes {
        return false;
    }
    let policy = checkpoint_policy(ann);
    ann.indexes
        .read()
        .await
        .get(key)
        .is_some_and(|bridge| policy.due(bridge))
}

pub(super) fn load_segment(ann: &SharedAnn, dir: &std::path::Path) -> Result<AnnBridge, String> {
    #[cfg(test)]
    ann.segment_load_count.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    if ann.fail_next_segment_load.swap(false, Ordering::SeqCst) {
        return Err("injected segment re-adoption failure".into());
    }
    #[cfg(not(test))]
    let _ = ann;
    AnnBridge::load(dir)
}

pub(super) struct AnnWarmDetails {
    pub(super) path: &'static str,
    pub(super) ops_applied: u64,
}

impl Default for AnnWarmDetails {
    fn default() -> Self {
        Self {
            path: "already_fresh",
            ops_applied: 0,
        }
    }
}

impl AnnWarmDetails {
    pub(super) fn finish(&mut self, result: &Result<AnnEnsureStatus, RuntimeError>) {
        match result {
            Ok(AnnEnsureStatus::EmptyCorpus) => self.path = "empty",
            Ok(AnnEnsureStatus::DeclinedNotWarmHost) => self.path = "declined",
            Ok(AnnEnsureStatus::DiscardedStaleBuild) => self.path = "discarded",
            Err(_) => self.path = "failed",
            _ => {}
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct AnnWarmCompletedPayload {
    #[serde(flatten)]
    pub(super) phase: khive_storage::PhaseCompletedPayload,
    pub(super) path: &'static str,
    pub(super) ops_applied: u64,
}

pub(super) enum InstalledMaintenance {
    Complete,
    Rebuild,
    Absent,
}

pub(super) struct IncrementalTail {
    pub(super) ops: Vec<(Uuid, Option<Vec<f32>>)>,
    pub(super) applied: u64,
    pub(super) raw_count: u64,
}

/// Read the delta and its raw row count under the same registry-protected snapshot.
/// Coalescing repeatedly updated subjects must not hide the restart tail's size.
pub(super) async fn protected_tail(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
    applied: u64,
    max_delta: u64,
) -> Result<Option<IncrementalTail>, String> {
    #[cfg(not(test))]
    let _ = ann;
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let result = fetch_protected_tail_on(reader.as_mut(), model, applied, max_delta).await;
    #[cfg(test)]
    ann.pause_protected_tail_for_test().await;
    result.map(|(tail, _observed_rows)| {
        tail.map(|(ops, applied, raw_count)| IncrementalTail {
            ops,
            applied,
            raw_count,
        })
    })
}

pub(super) struct MaintenanceFence {
    incarnation: Arc<()>,
    applied: u64,
    epoch: u64,
    generation: u64,
    published_seq: u64,
    dirty_ops: u64,
    commit_digest: Option<[u8; 32]>,
    last_delta_nonce: Option<Uuid>,
}

impl MaintenanceFence {
    pub(super) fn capture(bridge: &AnnBridge) -> Self {
        Self {
            incarnation: Arc::clone(&bridge.incarnation),
            applied: bridge.index.last_applied_seq().unwrap_or(0),
            epoch: bridge.epoch_baseline,
            generation: bridge.generation,
            published_seq: bridge.published_seq,
            dirty_ops: bridge.dirty_ops,
            commit_digest: bridge.commit_digest,
            last_delta_nonce: bridge.last_delta_nonce,
        }
    }

    pub(super) fn matches(&self, bridge: &AnnBridge) -> bool {
        Arc::ptr_eq(&self.incarnation, &bridge.incarnation)
            && self.applied == bridge.index.last_applied_seq().unwrap_or(0)
            && self.epoch == bridge.epoch_baseline
            && self.generation == bridge.generation
            && self.published_seq == bridge.published_seq
            && self.dirty_ops == bridge.dirty_ops
            && self.commit_digest == bridge.commit_digest
            && self.last_delta_nonce == bridge.last_delta_nonce
    }
}

pub(super) async fn maintain_installed(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    generation: u64,
    epoch: u64,
    details: &mut AnnWarmDetails,
) -> Result<InstalledMaintenance, RuntimeError> {
    let Some((applied, live)) = ann.indexes.read().await.get(key).map(|b| {
        (
            b.index.last_applied_seq().unwrap_or(0),
            b.index.live_count(),
        )
    }) else {
        return Ok(InstalledMaintenance::Absent);
    };
    let policy = checkpoint_policy(ann);
    // Bound this *tail* by replay cost. Cumulative delta headroom only chooses
    // whether the accepted tail publishes another chunk or a full checkpoint.
    let max_delta = replay_limit(live as u64, policy.rebuild_fraction);
    let IncrementalTail {
        ops,
        applied: new_s,
        raw_count,
    } = match protected_tail(rt, ann, model, applied, max_delta).await {
        Ok(Some(tail)) => tail,
        Ok(None) => return Ok(InstalledMaintenance::Rebuild),
        Err(error) => {
            tracing::warn!(%error, model, "memory ANN installed tail unavailable; reclassifying segment");
            return Ok(InstalledMaintenance::Absent);
        }
    };
    if durable_epoch(rt).await != epoch {
        return Ok(InstalledMaintenance::Rebuild);
    }
    details.ops_applied = ops.len() as u64;
    let no_work = if raw_count == 0 {
        let indexes = ann.indexes.read().await;
        let Some(bridge) = indexes.get(key) else {
            return Ok(InstalledMaintenance::Absent);
        };
        if bridge.index.last_applied_seq().unwrap_or(0) != applied || bridge.epoch_baseline != epoch
        {
            return Ok(InstalledMaintenance::Absent);
        }
        let publish = policy.due(bridge);
        let consolidate = publish
            && bridge.needs_full_compaction()
            && (bridge.index.needs_consolidation()
                || bridge.index.ops_since_consolidation() >= policy.consolidate_tau);
        (!consolidate).then(|| (MaintenanceFence::capture(bridge), publish))
    } else {
        None
    };
    let (publish, publication_incarnation) = if let Some((fence, publish)) = no_work {
        let mut indexes = ann.indexes.write().await;
        let Some(bridge) = indexes.get_mut(key) else {
            return Ok(InstalledMaintenance::Absent);
        };
        if !fence.matches(bridge) {
            return Ok(InstalledMaintenance::Absent);
        }
        khive_storage::ensure_request_read_active("memory.ann.incremental")?;
        bridge.generation = generation;
        (publish, Arc::clone(&bridge.incarnation))
    } else {
        let shared = Arc::clone(ann);
        let maintenance_key = key.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let indexes = shared.indexes.blocking_read();
            let Some(incumbent) = indexes.get(&maintenance_key) else {
                return Ok(None);
            };
            if incumbent.index.last_applied_seq().unwrap_or(0) != applied
                || incumbent.epoch_baseline != epoch
            {
                return Ok(None);
            }
            let fence = MaintenanceFence::capture(incumbent);
            let mut bridge = incumbent.fork_for_maintenance();
            drop(indexes);
            if raw_count > 0 {
                let recorded_ops = ops.clone();
                bridge.apply_final_ops(ops, new_s)?;
                bridge.record_delta_batch(recorded_ops, new_s, raw_count);
            }
            bridge.generation = generation;
            bridge.dirty_ops = bridge.dirty_ops.saturating_add(raw_count);
            if raw_count > 0 {
                bridge.namespace_set.clear();
            }
            let publish = policy.due(&bridge) || (raw_count > 0 && bridge.needs_full_compaction());
            if publish && bridge.needs_full_compaction() {
                bridge.consolidate_if_needed(policy.consolidate_tau)?;
            }
            Ok::<_, String>(Some((fence, bridge, publish)))
        })
        .await
        .map_err(|error| RuntimeError::Internal(format!("memory ANN maintenance task: {error}")))?;
        let (fence, candidate, publish) = match prepared {
            Ok(Some(prepared)) => prepared,
            Ok(None) => return Ok(InstalledMaintenance::Absent),
            Err(error) => {
                tracing::warn!(%error, model, "memory ANN incremental apply failed; rebuilding");
                return Ok(InstalledMaintenance::Rebuild);
            }
        };
        khive_storage::ensure_request_read_active("memory.ann.incremental")?;
        if durable_epoch(rt).await != epoch {
            return Ok(InstalledMaintenance::Rebuild);
        }
        let publication_incarnation = Arc::clone(&candidate.incarnation);
        let retired = {
            let mut indexes = ann.indexes.write().await;
            let Some(incumbent) = indexes.get(key) else {
                return Ok(InstalledMaintenance::Absent);
            };
            if !fence.matches(incumbent) {
                return Ok(InstalledMaintenance::Absent);
            }
            khive_storage::ensure_request_read_active("memory.ann.incremental")?;
            indexes.insert(key.clone(), candidate)
        };
        drop(retired);
        (publish, publication_incarnation)
    };
    details.path = "incremental_in_place";
    if !publish {
        return Ok(InstalledMaintenance::Complete);
    }
    // Readers retain access to the incumbent during file-backed publication. No
    // index write lock spans that filesystem I/O; the caller owns the model warm lock.
    if let Some(dir) = ann_segment_dir(rt, model) {
        let indexes = ann.indexes.read().await;
        let Some(bridge) = indexes.get(key) else {
            return Ok(InstalledMaintenance::Absent);
        };
        if !Arc::ptr_eq(&bridge.incarnation, &publication_incarnation) {
            return Ok(InstalledMaintenance::Absent);
        }
        let publication_fence = MaintenanceFence::capture(bridge);
        let publication =
            persist_file_checkpoint(rt, ann, model, &dir, bridge, WatermarkAuthority::Active).await;
        drop(indexes);
        match publication {
            Ok(CheckpointResult::Full {
                reopened: Some(mut reopened),
                ..
            }) => {
                reopened.generation = generation;
                reopened.epoch_baseline = epoch;
                let retired = {
                    let mut indexes = ann.indexes.write().await;
                    if indexes
                        .get(key)
                        .is_some_and(|bridge| publication_fence.matches(bridge))
                    {
                        indexes.insert(key.clone(), *reopened)
                    } else {
                        return Ok(InstalledMaintenance::Absent);
                    }
                };
                drop(retired);
            }
            Ok(CheckpointResult::Full {
                reopened: None,
                base_digest,
            }) => {
                let mut indexes = ann.indexes.write().await;
                if let Some(bridge) = indexes
                    .get_mut(key)
                    .filter(|bridge| publication_fence.matches(bridge))
                {
                    bridge.mark_full_checkpoint_base(base_digest);
                    bridge.mark_checkpointed();
                }
            }
            Ok(CheckpointResult::Delta(publication)) => {
                let mut indexes = ann.indexes.write().await;
                if let Some(bridge) = indexes
                    .get_mut(key)
                    .filter(|bridge| publication_fence.matches(bridge))
                {
                    bridge.mark_delta_checkpoint(&publication);
                    bridge.mark_checkpointed();
                }
            }
            Err(unprotected) => {
                if unprotected {
                    evict_unprotected_index(ann, key).await;
                }
                return Err(RuntimeError::Internal(
                    "memory ANN incremental checkpoint was not published".into(),
                ));
            }
        }
    } else {
        #[cfg(test)]
        ann.pause_pathless_checkpoint_for_test().await;
        // Publish the registry protection before raising the bridge's
        // exact-tail floor. The intermediate old-floor/new-registry state is
        // resolved by fresh_tail_serving's pathless mismatch branch. Do not
        // hold the index write lock while waiting for the SQL writer: a
        // pathless fresh-tail snapshot can hold that connection while it
        // needs the index read lock. The mismatch path drops its snapshot
        // before waiting for this checkpoint's model lock.
        #[cfg(test)]
        ann.pathless_watermark_attempt_notify.notify_one();
        if let Err(error) =
            raise_watermark_with_authority(rt, model, new_s, WatermarkAuthority::Active).await
        {
            evict_unprotected_index(ann, key).await;
            return Err(RuntimeError::Internal(error));
        }
        #[cfg(test)]
        if ann
            .pathless_post_watermark_barrier
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            ann.pathless_post_watermark_notify.notify_one();
            ann.pathless_post_watermark_release.notified().await;
        }
        let mut indexes = ann.indexes.write().await;
        if let Some(bridge) = indexes.get_mut(key).filter(|bridge| {
            Arc::ptr_eq(&bridge.incarnation, &publication_incarnation)
                && bridge.index.last_applied_seq().unwrap_or(0) == new_s
                && bridge.epoch_baseline == epoch
                && bridge.generation == generation
        }) {
            // A pathless publication has no delta chain to retain or compact.
            bridge.delta_batches.clear();
            bridge.delta_raw_ops = 0;
            bridge.delta_chunks = 0;
            bridge.base_ops = bridge.index.num_vectors();
            bridge.mark_checkpointed();
        }
        drop(indexes);
        if let Err(error) = compact_log(rt, model).await {
            tracing::warn!(%error, model, "memory ANN log compaction failed after incremental checkpoint");
        }
    }
    details.path = "incremental_checkpoint";
    Ok(InstalledMaintenance::Complete)
}
