//! SubstrateCoordinator — cross-backend dispatch (D2-D4).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::task::JoinError;
use uuid::Uuid;

use khive_pack_kg::handlers::{SearchSubstrate, ValidatedSearchRequest};
use khive_runtime::{
    BackendId, EdgeEndpointKind, KhiveRuntime, NoteSearchHit, Resolved, SearchHit, SearchSource,
};
use khive_score::DeterministicScore;
use khive_storage::EdgeRelation;
use khive_types::{namespace::Namespace, SubstrateKind};

use super::locator::LocatorCache;
use super::registry::BackendRegistry;

/// Keep arbitrary configured backend identifiers safe and bounded before
/// they reach persistent coordinator diagnostics.  The raw identifier stays
/// on [`BackendSearchResult`] for internal routing; the MCP boundary applies
/// the same canonical secret masker before exposing it on the wire.
fn bounded_backend_id_for_log(backend_id: &str) -> String {
    const MAX_INPUT_CHARS: usize = 4_096;
    const MAX_OUTPUT_CHARS: usize = 256;

    let backend_id_chars = backend_id.chars().count();
    // Mask the FULL id before any truncation: a detector's terminating span
    // (e.g. the `@` closing a `scheme://user:pass@host` credential) can sit
    // past any fixed input window, and a masker that only sees a truncated
    // prefix cannot recognize a match it cannot see the end of.
    let masked = khive_runtime::secret_gate::mask_for_redaction_surface(
        khive_runtime::secret_gate::RedactionSurface::McpDiagnostic,
        backend_id,
    );
    let was_masked = masked.as_ref() != backend_id || masked.trim().is_empty();
    let masked_input: String = masked.chars().take(MAX_INPUT_CHARS).collect();
    let sanitized = if masked_input.trim().is_empty() {
        "masked-backend"
    } else {
        masked_input.as_str()
    };
    if !was_masked && backend_id_chars <= MAX_OUTPUT_CHARS {
        return sanitized.to_string();
    }

    // Fingerprint the original value so independently configured secrets or
    // long identifiers do not collapse onto one diagnostic key after
    // masking/truncation.  The digest reveals no credential material.
    let fingerprint = format!("{:x}", Sha256::digest(backend_id.as_bytes()));
    let suffix = format!("…#{fingerprint}");
    let prefix_chars = MAX_OUTPUT_CHARS - suffix.chars().count();
    let prefix: String = sanitized.chars().take(prefix_chars).collect();
    format!("{prefix}{suffix}")
}

/// Bound and mask a backend failure cause before it reaches a warning.  This
/// mirrors the MCP wire boundary so the earlier coordinator diagnostic cannot
/// leak a credential that the response would later redact.
pub(super) fn bounded_backend_cause_for_log(message: &str) -> String {
    const MAX_INPUT_CHARS: usize = 4_096;
    const MAX_OUTPUT_CHARS: usize = 1_024;
    const MISSING_CAUSE: &str = "backend search failed without diagnostic detail";

    // Mask the FULL message before any truncation: a detector's terminating
    // span can sit past any fixed input window, and a masker that only sees
    // a truncated prefix cannot recognize a match it cannot see the end of.
    let masked = khive_runtime::secret_gate::mask_for_redaction_surface(
        khive_runtime::secret_gate::RedactionSurface::McpDiagnostic,
        message,
    );
    if masked.trim().is_empty() {
        return MISSING_CAUSE.to_string();
    }
    let masked_input_truncated = masked.chars().nth(MAX_INPUT_CHARS).is_some();
    let bounded_masked: String = masked.chars().take(MAX_INPUT_CHARS).collect();
    let mut masked_chars = bounded_masked.chars();
    let mut bounded: String = masked_chars.by_ref().take(MAX_OUTPUT_CHARS).collect();
    if masked_chars.next().is_some() || masked_input_truncated {
        bounded.push('…');
    }
    bounded
}

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

/// A located edge endpoint: which backend owns it, and its substrate kind.
///
/// `kind` lets cross-backend `annotates` validation accept edge-substrate
/// targets (ADR-002 rule 1) without a second DB round-trip once located.
#[derive(Clone, Debug)]
struct LocatedEndpoint {
    backend_id: BackendId,
    kind: EdgeEndpointKind,
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
    #[cfg(test)]
    pub(super) panic_backend_id: Option<String>,
    /// Test-only: named backend's fan-out search task never resolves,
    /// simulating a hung backend (MAJ-2 timeout regression coverage).
    #[cfg(test)]
    pub(super) hang_backend_ids: Vec<String>,
    /// Test-only: delay a backend result past the shared absolute deadline.
    #[cfg(test)]
    pub(super) delay_backend: Option<(String, Duration)>,
    /// Test-only: when set, a backend's entity fan-out task returns this
    /// list verbatim instead of calling `hybrid_search`, so RRF-merge
    /// regression tests can pin exact ranks/UUIDs (MAJ-4).
    #[cfg(test)]
    pub(super) entity_hits_override: Option<HashMap<String, Vec<SearchHit>>>,
    /// Test-only: same as `entity_hits_override`, for the note substrate.
    #[cfg(test)]
    pub(super) note_hits_override: Option<HashMap<String, Vec<NoteSearchHit>>>,
}

impl SubstrateCoordinator {
    /// Construct from a [`BackendRegistry`].
    pub fn new(registry: BackendRegistry) -> Self {
        Self {
            registry,
            locator: Arc::new(LocatorCache::new()),
            #[cfg(test)]
            fail_backend_id: None,
            #[cfg(test)]
            panic_backend_id: None,
            #[cfg(test)]
            hang_backend_ids: Vec::new(),
            #[cfg(test)]
            delay_backend: None,
            #[cfg(test)]
            entity_hits_override: None,
            #[cfg(test)]
            note_hits_override: None,
        }
    }

    /// Construct from a [`BackendRegistry`] with a custom locator TTL.
    pub fn with_locator_ttl(registry: BackendRegistry, ttl: Duration) -> Self {
        Self {
            registry,
            locator: Arc::new(LocatorCache::with_ttl(ttl)),
            #[cfg(test)]
            fail_backend_id: None,
            #[cfg(test)]
            panic_backend_id: None,
            #[cfg(test)]
            hang_backend_ids: Vec::new(),
            #[cfg(test)]
            delay_backend: None,
            #[cfg(test)]
            entity_hits_override: None,
            #[cfg(test)]
            note_hits_override: None,
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
            #[cfg(test)]
            panic_backend_id: None,
            #[cfg(test)]
            hang_backend_ids: Vec::new(),
            #[cfg(test)]
            delay_backend: None,
            #[cfg(test)]
            entity_hits_override: None,
            #[cfg(test)]
            note_hits_override: None,
        }
    }

    /// Test-only: force `fan_out_search` to simulate a search failure for the named backend.
    #[cfg(test)]
    pub fn with_failing_backend(mut self, backend_id: &str) -> Self {
        self.fail_backend_id = Some(backend_id.to_string());
        self
    }

    /// Test-only: force a named backend's fan-out task to panic.
    #[cfg(test)]
    pub fn with_panicking_backend(mut self, backend_id: &str) -> Self {
        self.panic_backend_id = Some(backend_id.to_string());
        self
    }

    /// Test-only: force a named backend's fan-out search task to hang forever
    /// (never resolves), simulating an unresponsive backend.
    #[cfg(test)]
    pub fn with_hanging_backend(mut self, backend_id: &str) -> Self {
        self.hang_backend_ids = vec![backend_id.to_string()];
        self
    }

    /// Test-only: hang several concurrently-started backend tasks.
    #[cfg(test)]
    pub fn with_hanging_backends<I, S>(mut self, backend_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hang_backend_ids = backend_ids.into_iter().map(Into::into).collect();
        self
    }

    /// Test-only: delay one backend's result by `delay`.
    #[cfg(test)]
    pub fn with_delayed_backend(mut self, backend_id: &str, delay: Duration) -> Self {
        self.delay_backend = Some((backend_id.to_string(), delay));
        self
    }

    /// Test-only: pin a named backend's entity fan-out contribution to an
    /// exact, caller-supplied ranked list instead of querying storage.
    #[cfg(test)]
    pub fn with_entity_hits_override(mut self, overrides: HashMap<String, Vec<SearchHit>>) -> Self {
        self.entity_hits_override = Some(overrides);
        self
    }

    /// Test-only: pin a named backend's note fan-out contribution to an
    /// exact, caller-supplied ranked list instead of querying storage.
    #[cfg(test)]
    pub fn with_note_hits_override(
        mut self,
        overrides: HashMap<String, Vec<NoteSearchHit>>,
    ) -> Self {
        self.note_hits_override = Some(overrides);
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
    /// Delegates to the private `locate_endpoint`, which resolves in the same
    /// substrate order as `get` (entity/note/event, then edge — #674), so a
    /// full-UUID `link` endpoint locates exactly what `get` resolves for the
    /// same UUID.
    pub async fn locate(&self, id: Uuid, namespace: &Namespace) -> Option<BackendId> {
        self.locate_endpoint(id, namespace)
            .await
            .map(|e| e.backend_id)
    }

    /// Resolve which backend owns the substrate node identified by `id`,
    /// together with its endpoint kind (entity, note, event, or edge).
    ///
    /// Namespace-agnostic per ADR-007 Rev 3, same contract as [`Self::locate`].
    /// Checks the locator cache first; on a miss, scans all backends concurrently.
    /// Resolves in the same substrate order as `get` (ADR-002 rule 1 parity,
    /// #674): entity/note/event via `resolve_edge_endpoint`, then edge via
    /// `get_edge`.
    async fn locate_endpoint(&self, id: Uuid, namespace: &Namespace) -> Option<LocatedEndpoint> {
        if let Some(backend_id) = self.locator.get(id) {
            let runtime = self
                .registry
                .get(&backend_id)
                .map(|e| Arc::clone(&e.runtime))?;
            let kind = Self::probe_endpoint_kind(&runtime, namespace, id).await?;
            return Some(LocatedEndpoint { backend_id, kind });
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
            let kind = Self::probe_endpoint_kind(runtime, namespace, id).await?;
            self.locator.insert(id, backend_id.clone());
            return Some(LocatedEndpoint {
                backend_id: backend_id.clone(),
                kind,
            });
        }

        let ns_clone = namespace.clone();
        let locator = Arc::clone(&self.locator);

        let mut handles = Vec::with_capacity(entries.len());
        for (backend_id, runtime) in entries {
            let ns = ns_clone.clone();
            let locator = Arc::clone(&locator);
            let handle = tokio::spawn(khive_storage::inherit_request_read_context(async move {
                let kind = Self::probe_endpoint_kind(&runtime, &ns, id).await?;
                locator.insert(id, backend_id.clone());
                Some(LocatedEndpoint { backend_id, kind })
            }));
            handles.push(handle);
        }

        let results: Vec<Result<Option<LocatedEndpoint>, JoinError>> =
            futures_util::future::join_all(handles).await;
        for result in results {
            if let Ok(Some(located)) = result {
                return Some(located);
            }
        }
        None
    }

    /// Probe a single backend for `id`'s substrate kind, authorizing for
    /// `namespace` first.
    ///
    /// ADR-007 Rev 3: presence on this backend is sufficient — the stored
    /// record namespace is NOT compared to the caller namespace. Mirrors the
    /// by-ID resolution order `get` uses: entity/note/event first
    /// (`resolve_edge_endpoint`), then edge (`get_edge`).
    async fn probe_endpoint_kind(
        runtime: &Arc<KhiveRuntime>,
        namespace: &Namespace,
        id: Uuid,
    ) -> Option<EdgeEndpointKind> {
        let token = match runtime.authorize(namespace.clone()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "locate_endpoint: authorization denied for namespace");
                return None;
            }
        };
        match runtime.resolve_edge_endpoint(&token, id).await {
            Ok(Some(Resolved::Entity(_))) => return Some(EdgeEndpointKind::Entity),
            Ok(Some(Resolved::Note(_))) => return Some(EdgeEndpointKind::Note),
            Ok(Some(Resolved::Event(_))) => return Some(EdgeEndpointKind::Event),
            Ok(Some(Resolved::PackRecord { .. })) | Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "locate_endpoint: resolve_edge_endpoint failed");
                return None;
            }
        }
        match runtime.get_edge(&token, id).await {
            Ok(Some(_)) => Some(EdgeEndpointKind::Edge),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "locate_endpoint: get_edge failed");
                None
            }
        }
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
        let src_located = self
            .locate_endpoint(source_id, namespace)
            .await
            .ok_or_else(|| format!("node {source_id} not found on any backend"))?;
        let tgt_located = self
            .locate_endpoint(target_id, namespace)
            .await
            .ok_or_else(|| format!("node {target_id} not found on any backend"))?;

        let src_backend = src_located.backend_id.clone();
        let tgt_backend = tgt_located.backend_id.clone();

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
        } else if relation == EdgeRelation::Annotates {
            // Cross-backend annotates: `locate_endpoint` already resolved each
            // endpoint's substrate kind using the same by-ID order `get` uses
            // (ADR-002 rule 1 parity, #674), including edge-substrate targets
            // that `resolve_primary`/`Resolved` cannot express — no extra
            // cross-backend DB lookup is needed.
            src_runtime
                .validate_annotates_endpoint_kinds(
                    source_id,
                    target_id,
                    Some(src_located.kind),
                    Some(tgt_located.kind),
                )
                .map_err(|e| e.to_string())?;
        } else {
            // Cross-backend, non-annotates: the target entity lives on a different backend so the source
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

    /// Broadcast a validated KG search request to all registered backends in
    /// parallel and merge results via RRF (k=60). Every filter in the request
    /// reaches the matching runtime search method on every backend.
    ///
    /// Per-backend errors are captured in [`BackendSearchResult::error`] — a single
    /// failing backend does NOT abort the fan-out.
    ///
    /// Authorizes each backend token against `namespace` alone (visible set
    /// `[namespace]`) — see [`Self::fan_out_search_with_visibility`] for the
    /// caller-widened-visibility variant the MCP coordinator boundary uses.
    pub async fn fan_out_search(
        &self,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
    ) -> (Vec<SearchHit>, Vec<NoteSearchHit>, Vec<BackendSearchResult>) {
        self.fan_out_search_with_visibility(request, namespace, &[])
            .await
    }

    /// Same as [`Self::fan_out_search`], but authorizes each backend token
    /// with an explicit extra read-visibility set (MAJ-3 fix).
    ///
    /// `extra_visible` mirrors the normal registry dispatch path's
    /// `['local'] ∪ visible_namespaces` widening
    /// (`khive_runtime::pack::VerbRegistry::dispatch_with_identity`) — pass
    /// `&[]` (equivalent to [`Self::fan_out_search`]) to authorize against
    /// `namespace` alone, or the caller's resolved extra-visible set to widen
    /// read visibility the same way the single-backend registry path does.
    /// An explicit `namespace=` request parameter intentionally narrows
    /// visibility and must be passed as `&[]` by the caller — this method
    /// does not itself distinguish explicit from default namespace
    /// resolution.
    pub async fn fan_out_search_with_visibility(
        &self,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
        extra_visible: &[Namespace],
    ) -> (Vec<SearchHit>, Vec<NoteSearchHit>, Vec<BackendSearchResult>) {
        let search_notes = request.substrate() == SearchSubstrate::Note;
        let requested_substrate = if search_notes {
            SubstrateKind::Note
        } else {
            SubstrateKind::Entity
        };
        let search_limit = rrf_fanout_search_limit(request);
        let limit = request.limit();
        let props_filter_owned = request.properties().cloned();
        let tags_owned = request.tags().to_vec();
        let kind_filter_owned = request.kind_filter().map(str::to_string);
        let entity_type_owned = request.entity_type().map(str::to_string);
        let include_superseded = request.include_superseded();

        let entries: Vec<(BackendId, Arc<KhiveRuntime>)> = self
            .registry
            .iter()
            .filter(|entry| entry.serves(requested_substrate))
            .map(|e| (e.id.clone(), Arc::clone(&e.runtime)))
            .collect();

        if entries.is_empty() {
            return (vec![], vec![], vec![]);
        }

        let timeout_ms = backend_search_timeout_ms();
        let timeout_dur = Duration::from_millis(timeout_ms);
        let request_deadline = khive_storage::effective_request_read_deadline(
            khive_storage::RequestReadDeadline::after(timeout_dur),
        );

        if entries.len() == 1 {
            let (backend_id, runtime) = &entries[0];
            let token = match runtime
                .authorize_with_visibility(namespace.clone(), extra_visible.to_vec())
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        error = %bounded_backend_cause_for_log(&e.to_string()),
                        "fan_out_search: authorization denied for namespace"
                    );
                    let backend_result = BackendSearchResult {
                        backend_id: backend_id.clone(),
                        hits: vec![],
                        note_hits: vec![],
                        error: Some(e.to_string()),
                    };
                    return (vec![], vec![], vec![backend_result]);
                }
            };
            // MAJ-2 (r2 follow-up): the single-backend early return has no
            // spawned task to bound with the fan-out timeout loop below, so
            // both awaits are wrapped directly in the same
            // `backend_search_timeout_ms()` budget — an unbounded await here
            // would leave a hung single-backend deployment's search
            // unbounded even though the multi-backend path is bounded.
            #[cfg(test)]
            let should_hang = self
                .hang_backend_ids
                .iter()
                .any(|id| id == backend_id.as_str());
            #[cfg(not(test))]
            let should_hang = false;
            if search_notes {
                let search_fut = async {
                    if should_hang {
                        std::future::pending::<()>().await;
                        unreachable!("a pending future never resolves");
                    }
                    runtime
                        .search_notes(
                            &token,
                            request.query(),
                            None,
                            search_limit,
                            request.kind_filter(),
                            include_superseded,
                            &tags_owned,
                            props_filter_owned.as_ref(),
                        )
                        .await
                };
                let search_fut =
                    khive_storage::scope_request_read_deadline_at(request_deadline, search_fut);
                tokio::pin!(search_fut);
                match tokio::time::timeout_at(request_deadline.async_at(), &mut search_fut).await {
                    Ok(Ok(note_hits)) => {
                        let filtered_note_hits: Vec<NoteSearchHit> = note_hits
                            .iter()
                            .filter(|hit| {
                                request
                                    .source()
                                    .is_none_or(|expected| hit.source == expected)
                            })
                            .take(limit as usize)
                            .cloned()
                            .collect();
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits,
                            error: None,
                        };
                        return (vec![], filtered_note_hits, vec![backend_result]);
                    }
                    Ok(Err(e)) => {
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: vec![],
                            error: Some(e.to_string()),
                        };
                        return (vec![], vec![], vec![backend_result]);
                    }
                    Err(_elapsed) => {
                        let _ = tokio::time::timeout(
                            khive_db::sqlite_interrupt_grace_from_env(),
                            &mut search_fut,
                        )
                        .await;
                        tracing::warn!(
                            backend = %bounded_backend_id_for_log(backend_id.as_str()),
                            timeout_ms,
                            "backend search task timed out"
                        );
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: vec![],
                            error: Some(format!("backend search timed out after {timeout_ms}ms")),
                        };
                        return (vec![], vec![], vec![backend_result]);
                    }
                }
            } else {
                let search_fut = async {
                    if should_hang {
                        std::future::pending::<()>().await;
                        unreachable!("a pending future never resolves");
                    }
                    runtime
                        .hybrid_search(
                            &token,
                            request.query(),
                            None,
                            search_limit,
                            request.kind_filter(),
                            request.entity_type(),
                            &tags_owned,
                            props_filter_owned.as_ref(),
                        )
                        .await
                };
                let search_fut =
                    khive_storage::scope_request_read_deadline_at(request_deadline, search_fut);
                tokio::pin!(search_fut);
                match tokio::time::timeout_at(request_deadline.async_at(), &mut search_fut).await {
                    Ok(Ok(hits)) => {
                        let filtered_hits: Vec<SearchHit> = hits
                            .iter()
                            .filter(|hit| {
                                request
                                    .source()
                                    .is_none_or(|expected| hit.source == expected)
                            })
                            .take(limit as usize)
                            .cloned()
                            .collect();
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits,
                            note_hits: vec![],
                            error: None,
                        };
                        return (filtered_hits, vec![], vec![backend_result]);
                    }
                    Ok(Err(e)) => {
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: vec![],
                            error: Some(e.to_string()),
                        };
                        return (vec![], vec![], vec![backend_result]);
                    }
                    Err(_elapsed) => {
                        let _ = tokio::time::timeout(
                            khive_db::sqlite_interrupt_grace_from_env(),
                            &mut search_fut,
                        )
                        .await;
                        tracing::warn!(
                            backend = %bounded_backend_id_for_log(backend_id.as_str()),
                            timeout_ms,
                            "backend search task timed out"
                        );
                        let backend_result = BackendSearchResult {
                            backend_id: backend_id.clone(),
                            hits: vec![],
                            note_hits: vec![],
                            error: Some(format!("backend search timed out after {timeout_ms}ms")),
                        };
                        return (vec![], vec![], vec![backend_result]);
                    }
                }
            }
        }

        let query = request.query().to_string();
        let ns = namespace.clone();
        let extra_visible_owned = extra_visible.to_vec();

        #[cfg(test)]
        let fail_id: Option<String> = self.fail_backend_id.clone();
        #[cfg(not(test))]
        let fail_id: Option<String> = None;
        #[cfg(test)]
        let panic_id: Option<String> = self.panic_backend_id.clone();
        #[cfg(not(test))]
        let panic_id: Option<String> = None;
        #[cfg(test)]
        let hang_ids: Vec<String> = self.hang_backend_ids.clone();
        #[cfg(not(test))]
        let hang_ids: Vec<String> = Vec::new();
        #[cfg(test)]
        let delay_backend = self.delay_backend.clone();
        #[cfg(not(test))]
        let delay_backend: Option<(String, Duration)> = None;

        let mut handles = Vec::with_capacity(entries.len());
        for (backend_id, runtime) in entries {
            let q = query.clone();
            let ns = ns.clone();
            let extra_visible_task = extra_visible_owned.clone();
            let kf = kind_filter_owned.clone();
            let et = entity_type_owned.clone();
            let pf = props_filter_owned.clone();
            let tg = tags_owned.clone();
            let sl = search_limit;
            let should_fail = fail_id
                .as_deref()
                .map(|id| id == backend_id.as_str())
                .unwrap_or(false);
            let should_panic = panic_id
                .as_deref()
                .map(|id| id == backend_id.as_str())
                .unwrap_or(false);
            let should_hang = hang_ids.iter().any(|id| id == backend_id.as_str());
            let delay = delay_backend
                .as_ref()
                .and_then(|(id, delay)| (id == backend_id.as_str()).then_some(*delay));
            #[cfg(test)]
            let entity_override: Option<Vec<SearchHit>> = self
                .entity_hits_override
                .as_ref()
                .and_then(|m| m.get(backend_id.as_str()))
                .cloned();
            #[cfg(not(test))]
            let entity_override: Option<Vec<SearchHit>> = None;
            #[cfg(test)]
            let note_override: Option<Vec<NoteSearchHit>> = self
                .note_hits_override
                .as_ref()
                .and_then(|m| m.get(backend_id.as_str()))
                .cloned();
            #[cfg(not(test))]
            let note_override: Option<Vec<NoteSearchHit>> = None;
            let joined_backend_id = backend_id.clone();
            let search = async move {
                if should_panic {
                    panic!("injected backend search panic");
                }
                if should_hang {
                    // Never resolves — exercises the fan-out timeout (MAJ-2).
                    std::future::pending::<()>().await;
                    unreachable!("a pending future never resolves");
                }
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                if should_fail {
                    return (
                        backend_id,
                        Err(khive_runtime::RuntimeError::Internal(
                            "injected failure".to_string(),
                        )),
                        None::<Vec<NoteSearchHit>>,
                    );
                }
                if search_notes {
                    if let Some(hits) = note_override {
                        return (backend_id, Ok(vec![]), Some(hits));
                    }
                } else if let Some(hits) = entity_override {
                    return (backend_id, Ok(hits), None);
                }
                let token = match runtime.authorize_with_visibility(ns, extra_visible_task) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(
                            error = %bounded_backend_cause_for_log(&e.to_string()),
                            "fan_out_search: authorization denied for namespace"
                        );
                        return (backend_id, Err(e), None);
                    }
                };
                if search_notes {
                    let result = runtime
                        .search_notes(
                            &token,
                            &q,
                            None,
                            sl,
                            kf.as_deref(),
                            include_superseded,
                            &tg,
                            pf.as_ref(),
                        )
                        .await;
                    match result {
                        // Full search_limit-bounded window is retained here —
                        // truncating to the caller's limit per backend (MAJ-4)
                        // would remove candidates the RRF merge needs to fairly
                        // rank a hit that only places #2+ on any single backend.
                        // `rrf_merge_note_hits` applies `limit` once, after merge.
                        Ok(note_hits) => (backend_id, Ok(vec![]), Some(note_hits)),
                        Err(e) => (backend_id, Err(e), None),
                    }
                } else {
                    let result = runtime
                        .hybrid_search(
                            &token,
                            &q,
                            None,
                            sl,
                            kf.as_deref(),
                            et.as_deref(),
                            &tg,
                            pf.as_ref(),
                        )
                        .await;
                    match result {
                        // See the note-substrate arm above: no per-backend
                        // truncation before RRF merge (MAJ-4).
                        Ok(hits) => (backend_id, Ok(hits), None),
                        Err(e) => (backend_id, Err(e), None),
                    }
                }
            };
            let search = async move {
                let result = search.await;
                (result, tokio::time::Instant::now())
            };
            let handle = tokio::spawn(khive_storage::inherit_request_read_context(
                khive_storage::scope_request_read_deadline_at(request_deadline, search),
            ));
            handles.push((joined_backend_id, handle));
        }

        let mut per_backend: Vec<BackendSearchResult> = Vec::new();
        let mut entity_ranked_lists: Vec<Vec<SearchHit>> = Vec::new();
        let mut note_ranked_lists: Vec<Vec<NoteSearchHit>> = Vec::new();
        let interrupt_settlement_deadline =
            request_deadline.async_at() + khive_db::sqlite_interrupt_grace_from_env();
        for (joined_backend_id, mut handle) in handles {
            let joined = if handle.is_finished() {
                Ok((&mut handle).await)
            } else {
                match tokio::time::timeout_at(request_deadline.async_at(), &mut handle).await {
                    Ok(result) => Ok(result),
                    Err(elapsed) => {
                        let settled =
                            tokio::time::timeout_at(interrupt_settlement_deadline, &mut handle)
                                .await;
                        if settled.is_err() {
                            handle.abort();
                            let _ = (&mut handle).await;
                        }
                        // The grace window exists only to let SQLite tear down
                        // an interrupted statement and return its permit.  It
                        // must not extend the advertised request deadline.
                        Err(elapsed)
                    }
                }
            };
            match joined {
                Ok(Ok(((backend_id, Ok(hits), note_hits_opt), completed_at)))
                    if completed_at <= request_deadline.async_at() =>
                {
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
                Ok(Ok(((backend_id, Err(e), _), completed_at)))
                    if completed_at <= request_deadline.async_at() =>
                {
                    per_backend.push(BackendSearchResult {
                        backend_id,
                        hits: vec![],
                        note_hits: vec![],
                        error: Some(e.to_string()),
                    });
                }
                Ok(Err(join_err)) => {
                    let error = khive_runtime::RuntimeError::Internal(format!(
                        "backend search task join failed: {join_err}"
                    ));
                    tracing::warn!(
                        backend = %bounded_backend_id_for_log(joined_backend_id.as_str()),
                        error = %bounded_backend_cause_for_log(&error.to_string()),
                        "backend search task failed"
                    );
                    per_backend.push(BackendSearchResult {
                        backend_id: joined_backend_id,
                        hits: vec![],
                        note_hits: vec![],
                        error: Some(error.to_string()),
                    });
                }
                Ok(Ok((_late_result, _completed_at))) => {
                    tracing::warn!(
                        backend = %bounded_backend_id_for_log(joined_backend_id.as_str()),
                        timeout_ms,
                        "backend search task completed after the shared request deadline"
                    );
                    per_backend.push(BackendSearchResult {
                        backend_id: joined_backend_id,
                        hits: vec![],
                        note_hits: vec![],
                        error: Some(format!("backend search timed out after {timeout_ms}ms")),
                    });
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        backend = %bounded_backend_id_for_log(joined_backend_id.as_str()),
                        timeout_ms,
                        "backend search task timed out"
                    );
                    per_backend.push(BackendSearchResult {
                        backend_id: joined_backend_id,
                        hits: vec![],
                        note_hits: vec![],
                        error: Some(format!("backend search timed out after {timeout_ms}ms")),
                    });
                }
            }
        }

        let merged_entities =
            rrf_merge_entity_hits_filtered(entity_ranked_lists, limit as usize, request.source());
        let merged_notes =
            rrf_merge_note_hits_filtered(note_ranked_lists, limit as usize, request.source());
        (merged_entities, merged_notes, per_backend)
    }
}

/// Per-backend fan-out search timeout (MAJ-2 fix): bounds how long a single
/// backend's search task may run before the coordinator gives up on it and
/// reports a timeout-specific error for that backend, so one hung backend
/// cannot block the whole fan-out from returning healthy siblings' results.
const DEFAULT_BACKEND_SEARCH_TIMEOUT_MS: u64 = 5_000;

/// Return the cached per-backend fan-out search timeout in milliseconds.
/// See `KHIVE_COORDINATOR_SEARCH_TIMEOUT_MS` in
/// `crates/kkernel/docs/api/configuration.md`.
fn backend_search_timeout_ms() -> u64 {
    static TIMEOUT_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TIMEOUT_MS.get_or_init(|| {
        let ms = std::env::var("KHIVE_COORDINATOR_SEARCH_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_BACKEND_SEARCH_TIMEOUT_MS);
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_COORDINATOR_SEARCH_TIMEOUT_MS",
            ms.to_string(),
        );
        ms
    })
}

/// Per-backend fan-out over-fetch bound for RRF merge fairness (MAJ-4 fix).
///
/// Each backend must contribute more than just the caller's `limit` worth of
/// ranked candidates, or a candidate that ranks #2+ on multiple backends can
/// never out-fuse a rank-1 singleton that only one backend saw — truncating
/// to `limit` per backend removes it before the merge ever sees it.
/// `request.candidate_limit()` already widens the per-backend fetch for
/// request-filter recall; this widens further (bounded) so unfiltered
/// fan-out searches get the same fairness. The caller's `limit` is applied
/// exactly once, after the RRF merge (`rrf_merge_entity_hits` /
/// `rrf_merge_note_hits`).
const RRF_FANOUT_MULTIPLIER: u32 = 10;
/// Same cap shape as `FILTERED_SCAN_CAP` in khive-pack-kg's search handler,
/// applied to the fan-out-wide candidate window.
const RRF_FANOUT_CAP: u32 = 500;

fn rrf_fanout_search_limit(request: &ValidatedSearchRequest) -> u32 {
    request
        .candidate_limit()
        .max(request.limit().saturating_mul(RRF_FANOUT_MULTIPLIER))
        .min(RRF_FANOUT_CAP)
}

// ---- RRF merge ----

#[derive(Default)]
struct RrfMergeBucket {
    score: f64,
    source: Option<SearchSource>,
    title: Option<String>,
    snippet: Option<String>,
}

/// Merge multiple ranked entity hit lists via Reciprocal Rank Fusion (k=60).
#[cfg(test)]
pub(super) fn rrf_merge_entity_hits(lists: Vec<Vec<SearchHit>>, limit: usize) -> Vec<SearchHit> {
    rrf_merge_entity_hits_filtered(lists, limit, None)
}

fn rrf_merge_entity_hits_filtered(
    lists: Vec<Vec<SearchHit>>,
    limit: usize,
    source_filter: Option<SearchSource>,
) -> Vec<SearchHit> {
    const K: f64 = 60.0;

    let mut scores: HashMap<Uuid, RrfMergeBucket> = HashMap::new();

    for list in &lists {
        for (i, hit) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            let rrf = 1.0 / (K + rank);
            let entry = scores.entry(hit.entity_id).or_default();
            entry.score += rrf;
            entry.source = Some(match entry.source {
                Some(source) => source.union(hit.source),
                None => hit.source,
            });
            if entry.title.is_none() {
                entry.title = hit.title.clone();
            }
            if entry.snippet.is_none() {
                entry.snippet = hit.snippet.clone();
            }
        }
    }

    let mut merged: Vec<SearchHit> = scores
        .into_iter()
        .filter_map(|(id, bucket)| {
            let source = bucket.source.expect("each bucket gets a source");
            if source_filter.is_some_and(|expected| source != expected) {
                return None;
            }
            let det_score = DeterministicScore::from_f64(bucket.score);
            Some(SearchHit {
                entity_id: id,
                score: det_score,
                source,
                title: bucket.title,
                snippet: bucket.snippet,
            })
        })
        .collect();

    merged.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    merged.truncate(limit);
    merged
}

/// Merge multiple ranked note hit lists via Reciprocal Rank Fusion (k=60).
#[cfg(test)]
pub(super) fn rrf_merge_note_hits(
    lists: Vec<Vec<NoteSearchHit>>,
    limit: usize,
) -> Vec<NoteSearchHit> {
    rrf_merge_note_hits_filtered(lists, limit, None)
}

fn rrf_merge_note_hits_filtered(
    lists: Vec<Vec<NoteSearchHit>>,
    limit: usize,
    source_filter: Option<SearchSource>,
) -> Vec<NoteSearchHit> {
    const K: f64 = 60.0;

    let mut scores: HashMap<Uuid, RrfMergeBucket> = HashMap::new();

    for list in &lists {
        for (i, hit) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            let rrf = 1.0 / (K + rank);
            let entry = scores.entry(hit.note_id).or_default();
            entry.score += rrf;
            entry.source = Some(match entry.source {
                Some(source) => source.union(hit.source),
                None => hit.source,
            });
            if entry.title.is_none() {
                entry.title = hit.title.clone();
            }
            if entry.snippet.is_none() {
                entry.snippet = hit.snippet.clone();
            }
        }
    }

    let mut merged: Vec<NoteSearchHit> = scores
        .into_iter()
        .filter_map(|(id, bucket)| {
            let source = bucket.source.expect("each bucket gets a source");
            if source_filter.is_some_and(|expected| source != expected) {
                return None;
            }
            let det_score = DeterministicScore::from_f64(bucket.score);
            Some(NoteSearchHit {
                note_id: id,
                score: det_score,
                source,
                title: bucket.title,
                snippet: bucket.snippet,
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors khive-mcp's
    // backend_error_message_masks_a_url_credential_whose_terminator_crosses_the_input_cap:
    // the credential's terminating `@` lands past the function's own input
    // cap, so a masker that only sees the truncated prefix can never
    // recognize the span and the password prefix would survive untouched.

    #[test]
    fn backend_cause_for_log_masks_a_url_credential_whose_terminator_crosses_the_input_cap() {
        const MAX_INPUT_CHARS: usize = 4_096;
        let marker = "CoordinatorCauseMarkerXYZ789";
        let padding = "z".repeat(MAX_INPUT_CHARS + 200);
        let password = format!("{marker}{padding}");
        let url = format!("postgres://svc:{password}@internal-host.example.com/db");
        let message = format!("backend probe failed: {url}");

        let at_offset = message.find('@').expect("test fixture must contain '@'");
        assert!(at_offset > MAX_INPUT_CHARS);

        let bounded = bounded_backend_cause_for_log(&message);
        assert!(
            !bounded.contains(marker),
            "no fragment of the credential may survive masking: {bounded}"
        );
        assert!(
            bounded.contains("***MASKED***"),
            "the redaction marker must be present: {bounded}"
        );
    }

    #[test]
    fn backend_id_for_log_masks_a_url_credential_whose_terminator_crosses_the_input_cap() {
        const MAX_INPUT_CHARS: usize = 4_096;
        let marker = "CoordinatorIdMarkerXYZ789";
        let padding = "z".repeat(MAX_INPUT_CHARS + 200);
        let password = format!("{marker}{padding}");
        let backend_id = format!("postgres://svc:{password}@internal-host.example.com/db");

        let at_offset = backend_id.find('@').expect("test fixture must contain '@'");
        assert!(at_offset > MAX_INPUT_CHARS);

        let bounded = bounded_backend_id_for_log(&backend_id);
        assert!(
            !bounded.contains(marker),
            "no fragment of the credential may survive masking: {bounded}"
        );
    }
}
