//! SubstrateCoordinator — cross-backend dispatch (D1-D6).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinError;
use uuid::Uuid;

use khive_runtime::{BackendId, KhiveRuntime, SearchHit};
use khive_score::DeterministicScore;
use khive_types::namespace::Namespace;

use super::locator::LocatorCache;
use super::registry::BackendRegistry;

/// Result of a single backend's contribution to a fan-out search.
///
/// `hits` may be empty when the backend returned no results.
/// `error` carries the backend-specific failure message on error.
#[derive(Debug)]
pub struct BackendSearchResult {
    pub backend_id: BackendId,
    pub hits: Vec<SearchHit>,
    pub error: Option<String>,
}

/// Cross-backend dispatch layer.
///
/// Owns node-to-backend location (D2), cross-backend search fan-out with RRF (D3),
/// traversal (D5), and partition tolerance (D6).
pub struct SubstrateCoordinator {
    registry: BackendRegistry,
    locator: Arc<LocatorCache>,
    #[cfg(test)]
    pub(super) fail_backend_id: Option<String>,
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

    /// Test-only: force `fan_out_search` to simulate a search failure for the named backend.
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
    pub fn is_single_backend(&self) -> bool {
        self.registry.len() <= 1
    }

    // ---- D2: Locator cache ----

    /// Resolve which backend owns the substrate node identified by `id`.
    ///
    /// Checks the locator cache first; on a miss, scans all backends concurrently.
    /// Probes both entity and note substrates.
    pub async fn locate(&self, id: Uuid, namespace: &Namespace) -> Option<BackendId> {
        if let Some(backend_id) = self.locator.get(id) {
            return Some(backend_id);
        }

        let entries: Vec<(BackendId, Arc<KhiveRuntime>)> = self
            .registry
            .iter()
            .map(|e| (e.id.clone(), Arc::clone(&e.runtime)))
            .collect();

        if entries.is_empty() {
            return None;
        }

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

                if let Ok(store) = runtime.entities(&token) {
                    if let Ok(Some(entity)) = store.get_entity(id).await {
                        if entity.namespace == ns_str {
                            locator.insert(id, backend_id.clone());
                            return Some(backend_id);
                        }
                    }
                }
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
    pub fn invalidate(&self, id: Uuid) {
        self.locator.remove(id);
    }

    // ---- D3: Fan-out search ----

    /// Broadcast `query` to all registered backends in parallel and merge results via RRF (k=60).
    ///
    /// Per-backend errors are captured in [`BackendSearchResult::error`] — a single
    /// failing backend does NOT abort the fan-out.
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

        let query = query.to_string();
        let ns = namespace.clone();

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
                    tracing::warn!(error = %join_err, "backend search task failed");
                }
            }
        }

        let merged = rrf_merge_hits(ranked_lists, limit as usize);
        (merged, per_backend)
    }
}

// ---- RRF merge ----

/// Merge multiple ranked hit lists via Reciprocal Rank Fusion (k=60).
fn rrf_merge_hits(lists: Vec<Vec<SearchHit>>, limit: usize) -> Vec<SearchHit> {
    const K: f64 = 60.0;

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

// ---- futures_util shim ----
//
// `khive-runtime` pulls in `futures` transitively. We use `futures::future::join_all`
// through this local shim to avoid adding a direct `futures` dep on kkernel.
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
