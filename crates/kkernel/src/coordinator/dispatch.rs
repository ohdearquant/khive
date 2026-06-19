//! SubstrateCoordinator — cross-backend dispatch (D2-D4).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinError;
use uuid::Uuid;

use khive_runtime::{BackendId, KhiveRuntime, NoteSearchHit, SearchHit};
use khive_score::DeterministicScore;
use khive_storage::EdgeRelation;
use khive_types::namespace::Namespace;

use super::locator::LocatorCache;
use super::registry::BackendRegistry;

/// Result of a single backend's entity-search contribution to a fan-out.
///
/// `hits` may be empty when the backend returned no results.
/// `error` carries the backend-specific failure message on error.
#[derive(Debug)]
pub struct BackendSearchResult {
    pub backend_id: BackendId,
    pub hits: Vec<SearchHit>,
    pub note_hits: Vec<NoteSearchHit>,
    pub error: Option<String>,
}

/// Cross-backend dispatch layer.
///
/// Owns node-to-backend location (D2), cross-backend link stamping (D3),
/// fan-out entity/note search with RRF (D4), traversal (D5, future),
/// and partition tolerance (D6, future).
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
    /// Namespace-agnostic per ADR-007 Rev 3: presence of the record on a backend
    /// is sufficient — the stored namespace is NOT compared to the caller namespace.
    /// The `namespace` parameter is used only for `runtime.authorize()` capability checks.
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
            // ADR-007 Rev 3: presence on this backend is sufficient.
            // Do NOT filter by stored record namespace.
            let entity_owned = match runtime.entities(&token) {
                Ok(store) => store.get_entity(id).await.ok().flatten().is_some(),
                Err(_) => false,
            };
            if entity_owned {
                self.locator.insert(id, backend_id.clone());
                return Some(backend_id.clone());
            }
            let note_owned = match runtime.notes(&token) {
                Ok(store) => store.get_note(id).await.ok().flatten().is_some(),
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
                // ADR-007 Rev 3: presence on this backend is sufficient.
                // Do NOT filter by stored record namespace.
                if let Ok(store) = runtime.entities(&token) {
                    if let Ok(Some(_)) = store.get_entity(id).await {
                        locator.insert(id, backend_id.clone());
                        return Some(backend_id);
                    }
                }
                if let Ok(store) = runtime.notes(&token) {
                    if let Ok(Some(_)) = store.get_note(id).await {
                        locator.insert(id, backend_id.clone());
                        return Some(backend_id);
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

    /// Prewarm the locator cache after a successful create.
    ///
    /// Called by the `SubstrateCoordinatorService` so that the first `locate()`
    /// for a newly-created record is a cache hit rather than a backend scan.
    pub fn record_created(&self, id: Uuid, backend_id: BackendId) {
        self.locator.insert(id, backend_id);
    }

    /// Invalidate the locator cache entry for `id`.
    pub fn invalidate(&self, id: Uuid) {
        self.locator.remove(id);
    }

    // ---- D3: Cross-backend link ----

    /// Create an edge whose endpoints may be on different backends (ADR-029 D3).
    ///
    /// Locates both `source_id` and `target_id`. When they are on different backends,
    /// the edge is written on the source backend with `target_backend` stamped to the
    /// target backend id. When both endpoints are on the same backend, delegates to
    /// the normal `link` path (no `target_backend` stamp).
    ///
    /// The coordinator validates endpoints via `validate_link_endpoints` on the source
    /// backend's runtime before writing the edge.
    pub async fn link_cross_backend(
        &self,
        namespace: &Namespace,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
    ) -> Result<khive_storage::Edge, String> {
        let src_backend = self
            .locate(source_id, namespace)
            .await
            .ok_or_else(|| format!("node {source_id} not found on any backend"))?;
        let tgt_backend = self
            .locate(target_id, namespace)
            .await
            .ok_or_else(|| format!("node {target_id} not found on any backend"))?;

        let src_runtime = self
            .registry
            .get(&src_backend)
            .map(|e| Arc::clone(&e.runtime))
            .ok_or_else(|| format!("backend {src_backend} not registered"))?;

        let token = src_runtime
            .authorize(namespace.clone())
            .map_err(|e: khive_runtime::RuntimeError| e.to_string())?;

        let cross_backend = src_backend.as_str() != tgt_backend.as_str();

        if !cross_backend {
            // Same-backend: full endpoint validation including existence and kind checks.
            src_runtime
                .validate_link_endpoints(&token, source_id, target_id, relation)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            // Cross-backend: the target entity lives on a different backend so the source
            // runtime cannot resolve it via its own DB. Fetch each endpoint from its
            // respective backend and validate the ADR-002 kind-pairing rules using the
            // pre-fetched records (no cross-backend DB lookup required).
            let tgt_runtime = self
                .registry
                .get(&tgt_backend)
                .map(|e| Arc::clone(&e.runtime))
                .ok_or_else(|| format!("backend {tgt_backend} not registered"))?;
            let tgt_token = tgt_runtime
                .authorize(namespace.clone())
                .map_err(|e: khive_runtime::RuntimeError| e.to_string())?;
            let src_resolved = src_runtime
                .resolve_primary(&token, source_id)
                .await
                .map_err(|e| e.to_string())?;
            let tgt_resolved = tgt_runtime
                .resolve_primary(&tgt_token, target_id)
                .await
                .map_err(|e| e.to_string())?;
            src_runtime
                .validate_link_endpoints_by_resolved(
                    source_id,
                    target_id,
                    relation,
                    src_resolved.as_ref(),
                    tgt_resolved.as_ref(),
                )
                .map_err(|e| e.to_string())?;
        }
        let target_backend_stamp = if cross_backend {
            Some(tgt_backend.as_str().to_string())
        } else {
            None
        };

        let edge = src_runtime
            .link_with_target_backend(
                &token,
                source_id,
                target_id,
                relation,
                weight,
                metadata,
                target_backend_stamp,
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok(edge)
    }

    // ---- D4: Fan-out search ----

    /// Broadcast `query` to all registered backends in parallel and merge results via RRF (k=60).
    ///
    /// `search_notes` controls which substrate to search:
    /// - `false` → entity fan-out via `hybrid_search`
    /// - `true`  → note fan-out via `search_notes`
    ///
    /// Per-backend errors are captured in [`BackendSearchResult::error`] — a single
    /// failing backend does NOT abort the fan-out.
    pub async fn fan_out_search(
        &self,
        query: &str,
        namespace: &Namespace,
        limit: u32,
        search_notes: bool,
    ) -> (Vec<SearchHit>, Vec<NoteSearchHit>, Vec<BackendSearchResult>) {
        let entries: Vec<(BackendId, Arc<KhiveRuntime>)> = self
            .registry
            .iter()
            .map(|e| (e.id.clone(), Arc::clone(&e.runtime)))
            .collect();

        if entries.is_empty() {
            return (vec![], vec![], vec![]);
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
                        note_hits: vec![],
                        error: Some(e.to_string()),
                    };
                    return (vec![], vec![], vec![backend_result]);
                }
            };
            if search_notes {
                match runtime
                    .search_notes(&token, query, None, limit, None, false)
                    .await
                {
                    Ok(note_hits) => {
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: note_hits.clone(),
                            error: None,
                        };
                        return (vec![], note_hits, vec![backend_result]);
                    }
                    Err(e) => {
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: vec![],
                            error: Some(e.to_string()),
                        };
                        return (vec![], vec![], vec![backend_result]);
                    }
                }
            } else {
                match runtime
                    .hybrid_search(&token, query, None, limit, None, None)
                    .await
                {
                    Ok(hits) => {
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: hits.clone(),
                            note_hits: vec![],
                            error: None,
                        };
                        return (hits, vec![], vec![backend_result]);
                    }
                    Err(e) => {
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: vec![],
                            error: Some(e.to_string()),
                        };
                        return (vec![], vec![], vec![backend_result]);
                    }
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
                        None::<Vec<NoteSearchHit>>,
                    );
                }
                let token = match runtime.authorize(ns) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "fan_out_search: authorization denied for namespace");
                        return (backend_id, Err(e), None);
                    }
                };
                if search_notes {
                    let result = runtime
                        .search_notes(&token, &q, None, limit, None, false)
                        .await;
                    match result {
                        Ok(note_hits) => (backend_id, Ok(vec![]), Some(note_hits)),
                        Err(e) => (backend_id, Err(e), None),
                    }
                } else {
                    let result = runtime
                        .hybrid_search(&token, &q, None, limit, None, None)
                        .await;
                    match result {
                        Ok(hits) => (backend_id, Ok(hits), None),
                        Err(e) => (backend_id, Err(e), None),
                    }
                }
            });
            handles.push(handle);
        }

        type BackendOutcome = (
            BackendId,
            Result<Vec<SearchHit>, khive_runtime::RuntimeError>,
            Option<Vec<NoteSearchHit>>,
        );
        let join_results: Vec<Result<BackendOutcome, JoinError>> =
            futures_util::future::join_all(handles).await;

        let mut per_backend: Vec<BackendSearchResult> = Vec::new();
        let mut entity_ranked_lists: Vec<Vec<SearchHit>> = Vec::new();
        let mut note_ranked_lists: Vec<Vec<NoteSearchHit>> = Vec::new();

        for join_result in join_results {
            match join_result {
                Ok((backend_id, Ok(hits), note_hits_opt)) => {
                    let note_hits = note_hits_opt.unwrap_or_default();
                    if !hits.is_empty() {
                        entity_ranked_lists.push(hits.clone());
                    }
                    if !note_hits.is_empty() {
                        note_ranked_lists.push(note_hits.clone());
                    }
                    per_backend.push(BackendSearchResult {
                        backend_id,
                        hits,
                        note_hits,
                        error: None,
                    });
                }
                Ok((backend_id, Err(e), _)) => {
                    per_backend.push(BackendSearchResult {
                        backend_id,
                        hits: vec![],
                        note_hits: vec![],
                        error: Some(e.to_string()),
                    });
                }
                Err(join_err) => {
                    tracing::warn!(error = %join_err, "backend search task failed");
                }
            }
        }

        let merged_entities = rrf_merge_entity_hits(entity_ranked_lists, limit as usize);
        let merged_notes = rrf_merge_note_hits(note_ranked_lists, limit as usize);
        (merged_entities, merged_notes, per_backend)
    }
}

// ---- RRF merge ----

/// Merge multiple ranked entity hit lists via Reciprocal Rank Fusion (k=60).
fn rrf_merge_entity_hits(lists: Vec<Vec<SearchHit>>, limit: usize) -> Vec<SearchHit> {
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

/// Merge multiple ranked note hit lists via Reciprocal Rank Fusion (k=60).
fn rrf_merge_note_hits(lists: Vec<Vec<NoteSearchHit>>, limit: usize) -> Vec<NoteSearchHit> {
    const K: f64 = 60.0;

    let mut scores: HashMap<Uuid, (f64, Option<String>, Option<String>)> = HashMap::new();

    for list in &lists {
        for (i, hit) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            let rrf = 1.0 / (K + rank);
            let entry = scores.entry(hit.note_id).or_insert((0.0, None, None));
            entry.0 += rrf;
            if entry.1.is_none() {
                entry.1 = hit.title.clone();
            }
            if entry.2.is_none() {
                entry.2 = hit.snippet.clone();
            }
        }
    }

    let mut merged: Vec<NoteSearchHit> = scores
        .into_iter()
        .map(|(id, (score, title, snippet))| {
            let det_score = DeterministicScore::from_f64(score);
            NoteSearchHit {
                note_id: id,
                score: det_score,
                title,
                snippet,
            }
        })
        .collect();

    merged.sort_by(|a, b| b.score.cmp(&a.score).then(a.note_id.cmp(&b.note_id)));
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
