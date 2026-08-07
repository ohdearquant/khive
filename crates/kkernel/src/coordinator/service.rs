//! `SubstrateCoordinatorService` — concrete implementation of the `CoordinatorService`
//! trait defined in `khive-mcp`. Wraps `SubstrateCoordinator` and adapts its types
//! to the trait interface used by `KhiveMcpServer`.

use std::collections::HashMap;

use async_trait::async_trait;
use uuid::Uuid;

use khive_mcp::coordinator::{
    BackendSearchResult as CoordBackendResult, CoordError, CoordLinkResult, CoordSearchResult,
    CoordinatorService,
};
use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::BackendId;
use khive_runtime::Namespace;
use khive_storage::EdgeRelation;

use super::dispatch::SubstrateCoordinator;

/// `CoordinatorService` wrapper around a [`SubstrateCoordinator`].
///
/// `KhiveMcpServer` holds `Option<Arc<dyn CoordinatorService>>` — it holds
/// `Some(Arc<SubstrateCoordinatorService>)` in multi-backend mode and `None`
/// for single-backend deployments (zero-change invariant).
pub struct SubstrateCoordinatorService {
    inner: SubstrateCoordinator,
}

impl SubstrateCoordinatorService {
    /// Wrap an existing [`SubstrateCoordinator`]. Search substrate and filter
    /// reconciliation are already captured by [`ValidatedSearchRequest`].
    pub fn new(coordinator: SubstrateCoordinator) -> Self {
        Self { inner: coordinator }
    }

    /// The primary backend id, if any.
    pub fn primary_backend_id_inner(&self) -> Option<BackendId> {
        self.inner.primary_runtime().map(|_| BackendId::main())
    }
}

#[async_trait]
impl CoordinatorService for SubstrateCoordinatorService {
    async fn locate(&self, id: Uuid) -> Option<BackendId> {
        // Locate uses `local` namespace for the capability check (authorization token).
        // The namespace is used only for `runtime.authorize()` — not to filter records
        // (ADR-007 Rev 3).
        let ns = Namespace::local();
        self.inner.locate(id, &ns).await
    }

    fn record_created(&self, id: Uuid, backend_id: BackendId) {
        self.inner.record_created(id, backend_id);
    }

    fn primary_backend_id(&self) -> Option<BackendId> {
        self.inner.registry().primary().map(|e| e.id.clone())
    }

    async fn link(
        &self,
        namespace: &Namespace,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
    ) -> Result<CoordLinkResult, CoordError> {
        self.inner
            .link_cross_backend(namespace, source_id, target_id, relation, weight, metadata)
            .await
            .map(|edge| {
                let cross_backend = edge.target_backend.is_some();
                let target_backend_id = edge.target_backend.as_deref().map(BackendId::new);
                CoordLinkResult {
                    edge,
                    cross_backend,
                    target_backend_id,
                }
            })
            .map_err(|msg| {
                if msg.contains("not found on any backend") {
                    CoordError::Backend(msg)
                } else if msg.contains("edge rule violation")
                    || msg.contains("self-loop")
                    || msg.contains("must be a note")
                {
                    CoordError::EdgeRuleViolation(msg)
                } else {
                    CoordError::Backend(msg)
                }
            })
    }

    async fn fan_out_search(
        &self,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
        extra_visible: &[Namespace],
    ) -> CoordSearchResult {
        let (entity_hits, note_hits, per_backend) = self
            .inner
            .fan_out_search_with_visibility(request, namespace, extra_visible)
            .await;

        let partial = per_backend.iter().any(|r| r.error.is_some());

        // Batch-fetch entity kind + created_at for each merged entity hit.
        // We locate each hit's owning backend and call get_entity on it.
        // By-ID (locate/get_entity) is namespace-agnostic (ADR-007 Rev 6), so
        // `extra_visible` does not apply here — only the fan-out search above
        // is namespace-filtered.
        let mut entity_kinds: HashMap<Uuid, String> = HashMap::new();
        let mut entity_created_at: HashMap<Uuid, i64> = HashMap::new();
        for hit in &entity_hits {
            let backend_id = self.inner.locate(hit.entity_id, namespace).await;
            if let Some(bid) = backend_id {
                if let Some(entry) = self.inner.registry().get(&bid) {
                    let rt = &entry.runtime;
                    if let Ok(token) = rt.authorize(namespace.clone()) {
                        if let Ok(entity) = rt.get_entity(&token, hit.entity_id).await {
                            entity_created_at.insert(hit.entity_id, entity.created_at);
                            entity_kinds.insert(hit.entity_id, entity.kind);
                        }
                    }
                }
            }
        }

        // Batch-fetch note kind + name + created_at for each merged note hit.
        let mut note_kinds: HashMap<Uuid, String> = HashMap::new();
        let mut note_created_at: HashMap<Uuid, i64> = HashMap::new();
        let mut note_names: HashMap<Uuid, Option<String>> = HashMap::new();
        for hit in &note_hits {
            let backend_id = self.inner.locate(hit.note_id, namespace).await;
            if let Some(bid) = backend_id {
                if let Some(entry) = self.inner.registry().get(&bid) {
                    let rt = &entry.runtime;
                    if let Ok(token) = rt.authorize(namespace.clone()) {
                        if let Ok(store) = rt.notes(&token) {
                            if let Ok(Some(note)) = store.get_note(hit.note_id).await {
                                note_created_at.insert(hit.note_id, note.created_at);
                                note_names.insert(hit.note_id, note.name.clone());
                                note_kinds.insert(hit.note_id, note.kind);
                            }
                        }
                    }
                }
            }
        }

        let coord_per_backend: Vec<CoordBackendResult> = per_backend
            .into_iter()
            .map(|r| CoordBackendResult {
                backend_id: r.backend_id,
                entity_hits: r.hits,
                note_hits: r.note_hits,
                error: r.error,
            })
            .collect();

        CoordSearchResult {
            entity_hits,
            note_hits,
            per_backend: coord_per_backend,
            partial,
            entity_kinds,
            note_kinds,
            entity_created_at,
            note_created_at,
            note_names,
        }
    }

    fn is_single_backend(&self) -> bool {
        self.inner.is_single_backend()
    }
}
