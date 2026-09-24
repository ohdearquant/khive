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

struct IncrementalTail {
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
    applied: u64,
    raw_count: u64,
}

/// Read the delta and its raw row count under the same registry-protected snapshot.
/// Coalescing repeatedly updated subjects must not hide the restart tail's size.
async fn protected_tail(
    rt: &KhiveRuntime,
    model: &str,
    applied: u64,
    max_delta: u64,
) -> Result<Option<IncrementalTail>, String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    begin_read_snapshot(reader.as_mut()).await?;
    let result = async {
        let minimum = registry_min_watermark_on(reader.as_mut(), model).await?;
        if minimum
            .and_then(|s| u64::try_from(s).ok())
            .is_some_and(|s| s > applied)
        {
            return Err("installed watermark is behind compacted history".into());
        }
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT COUNT(*) AS count FROM ann_write_log \
                  WHERE embedding_model = ?1 AND kind = 'note' AND field = 'note.content' \
                    AND seq > ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(applied as i64),
                ],
                label: Some("memory_ann_incremental_tail_count".into()),
            })
            .await
            .map_err(|e| e.to_string())?;
        let count = match rows.first().and_then(|row| row.get("count")) {
            Some(SqlValue::Integer(n)) if *n >= 0 => *n as u64,
            _ => return Err("incremental tail count is invalid".into()),
        };
        // Decide before hydrating a large tail. Both reads use this snapshot.
        if count > max_delta {
            return Ok(None);
        }
        let (ops, end) = fetch_final_tail_on(reader.as_mut(), model, applied, None).await?;
        Ok(Some(IncrementalTail {
            ops,
            applied: end,
            raw_count: count,
        }))
    }
    .await;
    end_read_snapshot(reader.as_mut()).await;
    result
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
    let max_delta = (policy.rebuild_fraction * live as f64).ceil() as u64;
    let IncrementalTail {
        ops,
        applied: new_s,
        raw_count,
    } = match protected_tail(rt, model, applied, max_delta).await {
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
    let publish = {
        let mut indexes = ann.indexes.write().await;
        let Some(bridge) = indexes.get_mut(key) else {
            return Ok(InstalledMaintenance::Absent);
        };
        if bridge.index.last_applied_seq().unwrap_or(0) != applied || bridge.epoch_baseline != epoch
        {
            return Ok(InstalledMaintenance::Absent);
        }
        if raw_count > 0 {
            if let Err(error) = bridge.apply_final_ops(ops, new_s) {
                tracing::warn!(%error, model, "memory ANN incremental apply failed; rebuilding");
                indexes.remove(key);
                return Ok(InstalledMaintenance::Rebuild);
            }
        }
        bridge.generation = generation;
        bridge.dirty_ops = bridge.dirty_ops.saturating_add(raw_count);
        if raw_count > 0 {
            // Deltas span all namespaces. Empty is the conservative over-fetch policy.
            bridge.namespace_set.clear();
        }
        let publish = policy.due(bridge);
        if publish {
            bridge
                .consolidate_if_needed(policy.consolidate_tau)
                .map_err(RuntimeError::Internal)?;
        }
        publish
    };
    details.path = "incremental_in_place";
    if !publish {
        return Ok(InstalledMaintenance::Complete);
    }
    // Readers retain access to the incumbent during publication. No index write
    // lock spans filesystem or database I/O; the caller owns the model warm lock.
    if let Some(dir) = ann_segment_dir(rt, model) {
        let indexes = ann.indexes.read().await;
        let Some(bridge) = indexes.get(key) else {
            return Ok(InstalledMaintenance::Absent);
        };
        let publication =
            persist_file_checkpoint(rt, ann, model, &dir, bridge, WatermarkAuthority::Active).await;
        drop(indexes);
        match publication {
            Ok(Some(mut reopened)) => {
                reopened.generation = generation;
                reopened.epoch_baseline = epoch;
                install_replacing(ann, key, reopened).await;
            }
            Ok(None) => {
                if let Some(bridge) = ann.indexes.write().await.get_mut(key) {
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
        if let Some(bridge) = ann.indexes.write().await.get_mut(key) {
            bridge.mark_checkpointed();
        }
        if let Err(error) =
            raise_watermark_with_authority(rt, model, new_s, WatermarkAuthority::Active).await
        {
            evict_unprotected_index(ann, key).await;
            return Err(RuntimeError::Internal(error));
        }
        if let Err(error) = compact_log(rt, model).await {
            tracing::warn!(%error, model, "memory ANN log compaction failed after incremental checkpoint");
        }
    }
    details.path = "incremental_checkpoint";
    Ok(InstalledMaintenance::Complete)
}
