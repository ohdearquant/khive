use super::*;

/// Identity key for the global memory Vamana index for a model; hex-encoded to
/// name its v2 segment directory. Distinct from knowledge's
/// `{ns}::vamana::{model}` to prevent corpus identity collisions.
pub(crate) fn snapshot_key(_namespace: &str, model: &str) -> String {
    format!("global::memory_vamana::{model}")
}

/// Status returned by `ensure_ann_for_model` so callers can log/act on the
/// build outcome without parsing log lines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnEnsureStatus {
    AlreadyLoaded,
    LoadedSnapshot,
    Built {
        vectors: usize,
    },
    EmptyCorpus,
    DiscardedStaleBuild,
    /// Nothing on disk was adoptable and this process does not build corpus
    /// indexes. The caller serves its exact/lexical path for this request; the
    /// daemon builds and publishes, and the next attempt adopts that segment.
    DeclinedNotWarmHost,
}

// ── state operations ──────────────────────────────────────────────────────────

/// Search the already-loaded index for `key`. Returns `Ok(None)` on cache miss,
/// `Ok(Some(hits))` on success, `Err` on ANN search failure (caller falls back).
/// Test-only: production call sites need the watermark paired atomically with
/// the search (ADR-118) and use [`search_loaded_with_seq`] directly.
#[cfg(test)]
pub(crate) async fn search_loaded(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Result<Option<Vec<(Uuid, f32)>>, RuntimeError> {
    search_loaded_with_seq(ann, key, query, k)
        .await
        .map(|opt| opt.map(|(hits, _seq)| hits))
}

/// Same as [`search_loaded`], but also returns the searched bridge's
/// fresh-tail watermark (ADR-118), captured from the SAME read-lock guard as
/// the search itself — one lock acquisition, so a concurrent bridge swap
/// (a checkpoint installing a replacement) cannot pair these candidates with
/// a different bridge's watermark.
pub(crate) async fn search_loaded_with_seq(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Result<Option<(Vec<(Uuid, f32)>, u64)>, RuntimeError> {
    search_loaded_with_seq_route(ann, key, query, k, AnnScoreRoute::Memory)
        .await
        .map(|result| {
            result.map(|(hits, seq)| {
                (
                    hits.into_iter()
                        .map(|(id, score)| (id, score as f32))
                        .collect(),
                    seq,
                )
            })
        })
}

pub(crate) async fn search_loaded_with_seq_route(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    route: AnnScoreRoute,
) -> Result<Option<(Vec<(Uuid, f64)>, u64)>, RuntimeError> {
    let guard = ann.indexes.read().await;
    match guard.get(key) {
        None => Ok(None),
        Some(bridge) => {
            #[cfg(test)]
            ann.warm_route_count.fetch_add(1, Ordering::SeqCst);
            let hits = bridge.search_with_route(query, k, route)?;
            // Keep the exact leg over unpublished deltas: incremental graph
            // insertion alone does not guarantee immediate reachability.
            let seq = if bridge.dirty_ops > 0 {
                bridge.published_seq
            } else {
                bridge.index.last_applied_seq().unwrap_or(0)
            };
            Ok(Some((hits, seq)))
        }
    }
}

/// Return the installed graph's namespaces; an empty set requires conservative retry.
pub(crate) async fn index_namespace_set(ann: &SharedAnn, key: &AnnKey) -> Option<HashSet<String>> {
    let guard = ann.indexes.read().await;
    guard.get(key).map(|b| b.namespace_set.clone())
}

/// Evict an unusable graph after ANN search failure; ordinary writes never call this.
pub(crate) async fn clear_key(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
    lock_warming(ann).remove(key);
}

/// Evict only the serving bridge after durable registry protection is lost.
/// The current warm owner keeps its fire-once guard until normal RAII release;
/// dropping that guard here would allow duplicate rebuild tasks to overlap.
pub(super) async fn evict_unprotected_index(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
}

/// Lock the synchronous warming set, recovering so one panic cannot disable future warms.
pub(super) fn lock_warming(ann: &SharedAnn) -> std::sync::MutexGuard<'_, HashSet<AnnKey>> {
    ann.warming
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Releases the fire-once warming key on success, error, or panic.
pub(super) struct WarmingGuard {
    pub(super) ann: SharedAnn,
    pub(super) key: AnnKey,
}

impl Drop for WarmingGuard {
    fn drop(&mut self) {
        lock_warming(&self.ann).remove(&self.key);
        #[cfg(test)]
        self.ann.warming_idle.notify_waiters();
    }
}

/// Wait until a model's background rebuild chain is idle.
///
/// Registers notification before testing the predicate so releases cannot be missed.
#[cfg(test)]
pub(crate) async fn wait_until_warm_idle(ann: &SharedAnn, key: &AnnKey) {
    loop {
        let notified = ann.warming_idle.notified();
        if !lock_warming(ann).contains(key) {
            return;
        }
        notified.await;
    }
}
