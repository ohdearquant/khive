// FILE SIZE JUSTIFICATION: coordinator/mod.rs co-locates BackendRegistry, LocatorCache,
// SubstrateCoordinator, and their tests because all three types are tightly coupled — LocatorCache
// holds BackendId values from BackendRegistry, and SubstrateCoordinator owns both. Splitting would
// require pub(crate) on all internal types and would break the single-invariant test helpers that
// construct coordinated registry+cache states. The inline tests for D2/D3/D4 phases need access
// to the #[cfg(test)] fail_backend_id field which cannot be pub(crate) without leaking it to
// integration tests.

//! SubstrateCoordinator — cross-backend dispatch layer (D1-D6).
//!
//! See `docs/coordinator.md` for architecture detail and deferred phases.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::task::JoinError;
use uuid::Uuid;

use khive_runtime::{BackendId, KhiveRuntime, SearchHit};
use khive_score::DeterministicScore;
use khive_types::namespace::Namespace;

// ---- BackendRegistry ----

/// A registered backend entry held by the [`SubstrateCoordinator`].
#[derive(Clone)]
pub struct BackendEntry {
    /// Unique identifier for this backend (matches `[[backends.name]]` in `khive.toml`).
    pub id: BackendId,
    /// The runtime instance operating over this backend.
    pub runtime: Arc<KhiveRuntime>,
}

/// Registry of all backends known to the coordinator.
///
/// Constructed once at boot from `khive.toml` and immutable thereafter.
/// Keyed by [`BackendId`] for deterministic ordering in `ids()` / `iter()` output.
#[derive(Default)]
pub struct BackendRegistry {
    backends: BTreeMap<String, BackendEntry>,
    primary: Option<String>,
}

impl BackendRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend. The first backend registered becomes the primary.
    ///
    /// Returns `false` if a backend with the same `id` was already registered.
    pub fn register(&mut self, id: BackendId, runtime: Arc<KhiveRuntime>) -> bool {
        let key = id.as_str().to_string();
        if self.backends.contains_key(&key) {
            return false;
        }
        if self.primary.is_none() {
            self.primary = Some(key.clone());
        }
        self.backends.insert(key, BackendEntry { id, runtime });
        true
    }

    /// Look up a backend by id.
    pub fn get(&self, id: &BackendId) -> Option<&BackendEntry> {
        self.backends.get(id.as_str())
    }

    /// The primary backend (first registered). `None` only if the registry is empty.
    pub fn primary(&self) -> Option<&BackendEntry> {
        self.primary.as_deref().and_then(|k| self.backends.get(k))
    }

    /// Iterate over all registered backends.
    pub fn iter(&self) -> impl Iterator<Item = &BackendEntry> {
        self.backends.values()
    }

    /// Number of registered backends.
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// True if no backends have been registered.
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// List all registered [`BackendId`]s.
    pub fn ids(&self) -> Vec<BackendId> {
        self.backends.keys().map(BackendId::new).collect()
    }
}

// ---- LocatorCache (D2) ----

/// Default TTL for locator cache entries (5 minutes).
const DEFAULT_LOCATOR_TTL: Duration = Duration::from_secs(300);

/// A single entry in the locator cache.
struct LocatorEntry {
    backend_id: BackendId,
    inserted_at: Instant,
}

/// In-memory UUID-to-backend cache with lazy TTL eviction.
///
/// See `docs/coordinator.md` for eviction strategy, TTL rationale, and
/// thread-safety design.
pub struct LocatorCache {
    entries: RwLock<HashMap<Uuid, LocatorEntry>>,
    ttl: Duration,
}

impl LocatorCache {
    /// Construct with the given TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Construct with the default TTL (5 minutes).
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_LOCATOR_TTL)
    }

    /// Look up the backend that owns `id`.
    ///
    /// Returns `None` on a miss or when the entry has expired. Expired entries
    /// are removed from the map under a write lock so they don't accumulate.
    pub fn get(&self, id: Uuid) -> Option<BackendId> {
        let now = Instant::now();
        // Fast path: read lock, live entry.
        {
            let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.get(&id) {
                if now.duration_since(entry.inserted_at) < self.ttl {
                    return Some(entry.backend_id.clone());
                }
                // Expired — drop read lock, upgrade to write lock to evict.
            } else {
                return None;
            }
        }
        // Slow path: entry exists but is expired — evict under write lock.
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        // Re-check under write lock: another thread may have refreshed it.
        if let Some(entry) = guard.get(&id) {
            if now.duration_since(entry.inserted_at) < self.ttl {
                return Some(entry.backend_id.clone());
            }
        }
        guard.remove(&id);
        None
    }

    /// Remove the cache entry for `id`, if any.
    ///
    /// Call on hard-delete or any write path that invalidates a UUID's backend
    /// assignment. Subsequent `get` calls will trigger a fresh backend scan.
    pub fn remove(&self, id: Uuid) {
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(&id);
    }

    /// Insert or refresh the owning backend for `id`.
    pub fn insert(&self, id: Uuid, backend_id: BackendId) {
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            id,
            LocatorEntry {
                backend_id,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Remove all entries whose TTL has elapsed.
    ///
    /// Call from a maintenance task to prevent unbounded growth in high-churn
    /// deployments. Under normal usage (entities don't disappear) the cache is
    /// bounded by the number of distinct entities touched in a session.
    pub fn purge_expired(&self) {
        let now = Instant::now();
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| now.duration_since(entry.inserted_at) < self.ttl);
    }

    /// Number of live entries (including possibly-expired ones not yet purged).
    pub fn len(&self) -> usize {
        let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
        guard.len()
    }

    /// True if the cache has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for LocatorCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Fan-out search result (D3) ----

/// Result of a single backend's contribution to a fan-out search.
///
/// `hits` may be empty when the backend returned no results.
/// `error` carries the backend-specific failure message when the backend
/// returned an error (best-effort — remaining backends still contribute).
#[derive(Debug)]
pub struct BackendSearchResult {
    pub backend_id: BackendId,
    pub hits: Vec<SearchHit>,
    pub error: Option<String>,
}

// ---- SubstrateCoordinator ----

/// Cross-backend dispatch layer.
///
/// Owns node-to-backend location (D2), cross-backend link routing (D3),
/// search fan-out with RRF (D4), traversal (D5), and partition tolerance (D6).
/// See `crates/kkernel/docs/coordinator.md` for architecture detail and deferred phases.
pub struct SubstrateCoordinator {
    registry: BackendRegistry,
    locator: Arc<LocatorCache>,
    /// Test-only: if set, `fan_out_search` forces this backend's search to fail
    /// (returns `RuntimeError::Internal`) so partial-failure paths can be tested
    /// without a real broken backend.
    #[cfg(test)]
    fail_backend_id: Option<String>,
}

impl SubstrateCoordinator {
    /// Construct from a [`BackendRegistry`].
    pub fn new(registry: BackendRegistry) -> Self {
        Self {
            registry,
            locator: Arc::new(LocatorCache::new()),
            #[cfg(test)]
            fail_backend_id: None,
        }
    }

    /// Construct from a [`BackendRegistry`] with a custom locator TTL.
    pub fn with_locator_ttl(registry: BackendRegistry, ttl: Duration) -> Self {
        Self {
            registry,
            locator: Arc::new(LocatorCache::with_ttl(ttl)),
            #[cfg(test)]
            fail_backend_id: None,
        }
    }

    /// Construct with a single backend (single-backend deployment default).
    ///
    /// Uses `BackendId::main()` as the backend id. The coordinator degenerates
    /// to a pass-through; all cross-backend mechanisms are identity.
    pub fn single(runtime: Arc<KhiveRuntime>) -> Self {
        let mut registry = BackendRegistry::new();
        registry.register(BackendId::main(), runtime);
        Self {
            registry,
            locator: Arc::new(LocatorCache::new()),
            #[cfg(test)]
            fail_backend_id: None,
        }
    }

    /// Test-only: instruct `fan_out_search` to simulate a search failure for
    /// the named backend. The backend still participates in the fan-out but its
    /// search returns `RuntimeError::Internal("injected failure")` rather than
    /// calling the real `hybrid_search`.
    #[cfg(test)]
    pub fn with_failing_backend(mut self, backend_id: &str) -> Self {
        self.fail_backend_id = Some(backend_id.to_string());
        self
    }

    /// The underlying [`BackendRegistry`].
    pub fn registry(&self) -> &BackendRegistry {
        &self.registry
    }

    /// A shared reference to the locator cache (D2).
    pub fn locator_cache(&self) -> &Arc<LocatorCache> {
        &self.locator
    }

    /// The primary backend's runtime, or `None` if the registry is empty.
    pub fn primary_runtime(&self) -> Option<Arc<KhiveRuntime>> {
        self.registry.primary().map(|e| Arc::clone(&e.runtime))
    }

    /// List all registered backend ids.
    pub fn backend_ids(&self) -> Vec<BackendId> {
        self.registry.ids()
    }

    /// Number of registered backends.
    pub fn backend_count(&self) -> usize {
        self.registry.len()
    }

    /// True when this is a single-backend deployment.
    ///
    /// When `true`, all D1–D6 coordinator mechanisms degenerate to identity:
    /// no fan-out, no cross-backend routing, no partition concerns.
    pub fn is_single_backend(&self) -> bool {
        self.registry.len() <= 1
    }

    // ---- D2: Locator cache ----

    /// Resolve which backend owns the substrate node identified by `id`.
    ///
    /// The probe checks both the entity substrate and the note substrate so that
    /// note UUIDs are located correctly in addition to entity UUIDs.
    ///
    /// 1. Check the [`LocatorCache`]. Return immediately on a live hit.
    /// 2. On a miss (or expired entry), scan all backends concurrently.
    ///    Each backend is probed for both an entity and a note with the given UUID.
    ///    The first backend that owns the UUID wins; the result is inserted into
    ///    the cache and returned.
    /// 3. Return `None` if no backend claims the UUID.
    ///
    /// In a single-backend deployment this is equivalent to confirming the node
    /// exists on the primary backend (or returning `None`).
    ///
    /// # Cache invalidation
    ///
    /// Call [`SubstrateCoordinator::invalidate`] after a hard-delete or a
    /// create-on-a-specific-backend operation to keep the cache consistent.
    pub async fn locate(&self, id: Uuid, namespace: &Namespace) -> Option<BackendId> {
        // Cache hit path (no I/O).
        if let Some(backend_id) = self.locator.get(id) {
            return Some(backend_id);
        }

        // Collect all (backend_id, runtime) pairs for the scan.
        let entries: Vec<(BackendId, Arc<KhiveRuntime>)> = self
            .registry
            .iter()
            .map(|e| (e.id.clone(), Arc::clone(&e.runtime)))
            .collect();

        if entries.is_empty() {
            return None;
        }

        // Single-backend shortcut: avoid tokio::spawn overhead.
        if entries.len() == 1 {
            let (backend_id, runtime) = &entries[0];
            let token = match runtime.authorize(namespace.clone()) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "locate: authorization denied for namespace");
                    return None;
                }
            };
            let ns_str = namespace.as_str().to_string();

            // Entity probe.
            let entity_ns = ns_str.clone();
            let entity_owned = match runtime.entities(&token) {
                Ok(store) => store
                    .get_entity(id)
                    .await
                    .ok()
                    .flatten()
                    .map(|e| e.namespace == entity_ns)
                    .unwrap_or(false),
                Err(_) => false,
            };
            if entity_owned {
                self.locator.insert(id, backend_id.clone());
                return Some(backend_id.clone());
            }
            // Note probe.
            let note_owned = match runtime.notes(&token) {
                Ok(store) => store
                    .get_note(id)
                    .await
                    .ok()
                    .flatten()
                    .map(|n| n.namespace == ns_str)
                    .unwrap_or(false),
                Err(_) => false,
            };
            if note_owned {
                self.locator.insert(id, backend_id.clone());
                return Some(backend_id.clone());
            }
            return None;
        }

        // Multi-backend concurrent scan — probe both entity and note substrates.
        let ns_clone = namespace.clone();
        let locator = Arc::clone(&self.locator);

        let mut handles = Vec::with_capacity(entries.len());
        for (backend_id, runtime) in entries {
            let ns = ns_clone.clone();
            let locator = Arc::clone(&locator);
            let handle = tokio::spawn(async move {
                let token = match runtime.authorize(ns.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "locate: authorization denied for namespace");
                        return None;
                    }
                };
                let ns_str = ns.as_str().to_string();

                // Entity probe.
                if let Ok(store) = runtime.entities(&token) {
                    if let Ok(Some(entity)) = store.get_entity(id).await {
                        if entity.namespace == ns_str {
                            locator.insert(id, backend_id.clone());
                            return Some(backend_id);
                        }
                    }
                }
                // Note probe.
                if let Ok(store) = runtime.notes(&token) {
                    if let Ok(Some(note)) = store.get_note(id).await {
                        if note.namespace == ns_str {
                            locator.insert(id, backend_id.clone());
                            return Some(backend_id);
                        }
                    }
                }
                None
            });
            handles.push(handle);
        }

        // Return the first backend that claims the UUID.
        let results: Vec<Result<Option<BackendId>, JoinError>> =
            futures_util::future::join_all(handles).await;
        for result in results {
            if let Ok(Some(backend_id)) = result {
                return Some(backend_id);
            }
        }
        None
    }

    /// Invalidate the locator cache entry for `id`.
    ///
    /// Call after a hard-delete (the deleted UUID must not route to the old
    /// backend on subsequent `locate` calls) or after a targeted write that
    /// creates a node on a specific backend so the cache reflects the new owner
    /// immediately rather than waiting for TTL expiry.
    pub fn invalidate(&self, id: Uuid) {
        self.locator.remove(id);
    }

    // ---- D3: Fan-out search ----

    /// Broadcast `query` to all registered backends in parallel and merge results.
    ///
    /// Each backend's `hybrid_search` is invoked concurrently. Results are fused
    /// using Reciprocal Rank Fusion (k=60) so that items appearing near the top of
    /// multiple backends' result lists rank higher in the merged output.
    ///
    /// Per-backend errors are captured in [`BackendSearchResult::error`] — a single
    /// failing backend does NOT abort the fan-out. The merged `Vec<SearchHit>` is
    /// derived from backends that succeeded.
    ///
    /// # Single-backend behaviour
    ///
    /// When `is_single_backend()` is true this degenerates to a single `hybrid_search`
    /// call on the primary backend with no concurrency overhead.
    ///
    /// # Result ordering
    ///
    /// Hits are sorted by descending RRF score (ties broken by UUID). The final
    /// list is truncated to `limit`.
    pub async fn fan_out_search(
        &self,
        query: &str,
        namespace: &Namespace,
        limit: u32,
    ) -> (Vec<SearchHit>, Vec<BackendSearchResult>) {
        let entries: Vec<(BackendId, Arc<KhiveRuntime>)> = self
            .registry
            .iter()
            .map(|e| (e.id.clone(), Arc::clone(&e.runtime)))
            .collect();

        if entries.is_empty() {
            return (vec![], vec![]);
        }

        // Single-backend shortcut.
        if entries.len() == 1 {
            let (backend_id, runtime) = &entries[0];
            let token = match runtime.authorize(namespace.clone()) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "fan_out_search: authorization denied for namespace");
                    let backend_result = BackendSearchResult {
                        backend_id: backend_id.clone(),
                        hits: vec![],
                        error: Some(e.to_string()),
                    };
                    return (vec![], vec![backend_result]);
                }
            };
            match runtime
                .hybrid_search(&token, query, None, limit, None, None)
                .await
            {
                Ok(hits) => {
                    let backend_result = BackendSearchResult {
                        backend_id: backend_id.clone(),
                        hits: hits.clone(),
                        error: None,
                    };
                    return (hits, vec![backend_result]);
                }
                Err(e) => {
                    let backend_result = BackendSearchResult {
                        backend_id: backend_id.clone(),
                        hits: vec![],
                        error: Some(e.to_string()),
                    };
                    return (vec![], vec![backend_result]);
                }
            }
        }

        // Multi-backend fan-out.
        let query = query.to_string();
        let ns = namespace.clone();

        // Test-only: capture which backend id (if any) should be forced to fail.
        #[cfg(test)]
        let fail_id: Option<String> = self.fail_backend_id.clone();
        #[cfg(not(test))]
        let fail_id: Option<String> = None;

        let mut handles = Vec::with_capacity(entries.len());
        for (backend_id, runtime) in entries {
            let q = query.clone();
            let ns = ns.clone();
            let should_fail = fail_id
                .as_deref()
                .map(|id| id == backend_id.as_str())
                .unwrap_or(false);
            let handle = tokio::spawn(async move {
                if should_fail {
                    return (
                        backend_id,
                        Err(khive_runtime::RuntimeError::Internal(
                            "injected failure".to_string(),
                        )),
                    );
                }
                let token = match runtime.authorize(ns) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "fan_out_search: authorization denied for namespace");
                        return (backend_id, Err(e));
                    }
                };
                let result = runtime
                    .hybrid_search(&token, &q, None, limit, None, None)
                    .await;
                (backend_id, result)
            });
            handles.push(handle);
        }

        type BackendSearchOutcome = (
            BackendId,
            Result<Vec<SearchHit>, khive_runtime::RuntimeError>,
        );
        let join_results: Vec<Result<BackendSearchOutcome, JoinError>> =
            futures_util::future::join_all(handles).await;

        let mut per_backend: Vec<BackendSearchResult> = Vec::new();
        // Each backend contributes an ordered list; we RRF-merge across lists.
        // Collect (backend_id, ranked_hits) pairs for fusion.
        let mut ranked_lists: Vec<Vec<SearchHit>> = Vec::new();

        for join_result in join_results {
            match join_result {
                Ok((backend_id, Ok(hits))) => {
                    ranked_lists.push(hits.clone());
                    per_backend.push(BackendSearchResult {
                        backend_id,
                        hits,
                        error: None,
                    });
                }
                Ok((backend_id, Err(e))) => {
                    per_backend.push(BackendSearchResult {
                        backend_id,
                        hits: vec![],
                        error: Some(e.to_string()),
                    });
                }
                Err(join_err) => {
                    // JoinError — task panicked or was cancelled. Log and continue.
                    tracing::warn!(error = %join_err, "backend search task failed");
                }
            }
        }

        let merged = rrf_merge_hits(ranked_lists, limit as usize);
        (merged, per_backend)
    }
}

// ---- RRF merge for fan-out search (D3) ----

/// Merge multiple ranked hit lists via Reciprocal Rank Fusion (k=60).
///
/// For each hit across all lists, the RRF score is the sum of `1/(k + rank)`
/// where `rank` is 1-indexed within each list. Hits appearing in multiple lists
/// accumulate score from each. The merged list is sorted descending by score,
/// ties broken by UUID, and truncated to `limit`.
fn rrf_merge_hits(lists: Vec<Vec<SearchHit>>, limit: usize) -> Vec<SearchHit> {
    const K: f64 = 60.0;

    // Per-UUID accumulators: (rrf_score, first_title, first_snippet, source).
    let mut scores: HashMap<Uuid, (f64, Option<String>, Option<String>)> = HashMap::new();

    for list in &lists {
        for (i, hit) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            let rrf = 1.0 / (K + rank);
            let entry = scores.entry(hit.entity_id).or_insert((0.0, None, None));
            entry.0 += rrf;
            if entry.1.is_none() {
                entry.1 = hit.title.clone();
            }
            if entry.2.is_none() {
                entry.2 = hit.snippet.clone();
            }
        }
    }

    // Build merged hits using float-to-deterministic score conversion.
    // DeterministicScore::from_f64 maps to a stable i64 representation for ordering.
    let mut merged: Vec<SearchHit> = scores
        .into_iter()
        .map(|(id, (score, title, snippet))| {
            let det_score = DeterministicScore::from_f64(score);
            SearchHit {
                entity_id: id,
                score: det_score,
                source: khive_runtime::SearchSource::Both,
                title,
                snippet,
            }
        })
        .collect();

    merged.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    merged.truncate(limit);
    merged
}

// ---- futures_util re-export shim ----
//
// `tokio` does not re-export `join_all`. We use the `futures-util` path via
// `futures::future::join_all` — but adding `futures` as a dep for one call is
// heavy. Instead we use the `tokio::task::JoinHandle` list directly by collecting
// into a `Vec` and awaiting each handle sequentially only when the `futures_util`
// crate is unavailable.
//
// Since `khive-runtime` already pulls in `futures` transitively we can use
// `futures::future::join_all` without adding a direct dep on kkernel. The
// `futures_util` path below is the canonical zero-dep pattern.
mod futures_util {
    pub mod future {
        pub async fn join_all<F: std::future::Future>(
            futs: Vec<F>,
        ) -> Vec<<F as std::future::Future>::Output> {
            let mut results = Vec::with_capacity(futs.len());
            for fut in futs {
                results.push(fut.await);
            }
            results
        }
    }
}

#[cfg(test)]
mod tests;
