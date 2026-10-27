//! `SubstrateCoordinatorService` — concrete implementation of the `CoordinatorService`
//! trait defined in `khive-mcp`. Wraps `SubstrateCoordinator` and adapts its types
//! to the trait interface used by `KhiveMcpServer`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use uuid::Uuid;

use khive_mcp::coordinator::{
    BackendSearchFailure as CoordBackendFailure,
    BackendSearchFailureKind as CoordBackendFailureKind, BackendSearchResult as CoordBackendResult,
    CoordError, CoordLinkResult, CoordSearchResult, CoordinatorService,
};
use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::Namespace;
use khive_runtime::{BackendId, NamespaceToken, NoteSearchHit, RuntimeError, SearchHit};
use khive_storage::EdgeRelation;

use super::dispatch::{BackendSearchFailureKind, SubstrateCoordinator};
use super::search_profile;

type ScopedSearchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(CoordSearchResult, u64), RuntimeError>> + Send + 'a>>;

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

    // The two search bodies are boxed inside these plain functions so the profiling
    // scope around them holds a pointer. Nesting the unboxed search future inside
    // that scope put a second copy of it in the caller's poll frame, which overflowed
    // a 2 MiB test-thread stack in a debug build.
    fn search_unscoped<'a>(
        &'a self,
        request: &'a ValidatedSearchRequest,
        namespace: &'a Namespace,
        extra_visible: &'a [Namespace],
    ) -> Pin<Box<dyn Future<Output = CoordSearchResult> + Send + 'a>> {
        Box::pin(async move {
            let (entity_hits, note_hits, per_backend) = self
                .inner
                .fan_out_search_with_visibility(request, namespace, extra_visible)
                .await;

            let started = search_profile::start();
            let result = self
                .hydrate_search_result(namespace, entity_hits, note_hits, per_backend, None, None)
                .await;
            search_profile::stage("hydrate", started, &[]);
            result
        })
    }

    fn search_scoped_with_count<'a>(
        &'a self,
        request: &'a ValidatedSearchRequest,
        token: &'a NamespaceToken,
        args: &'a serde_json::Value,
        extra_visible: &'a [Namespace],
    ) -> ScopedSearchFuture<'a> {
        Box::pin(async move {
            if request.substrate() == khive_pack_kg::handlers::SearchSubstrate::Entity {
                return Ok((
                    self.fan_out_search(request, token.gate_namespace(), extra_visible)
                        .await,
                    0,
                ));
            }
            let mailbox = if request.substrate() == khive_pack_kg::handlers::SearchSubstrate::Note {
                self.inner
                    .primary_runtime()
                    .map(|runtime| runtime.authorize_mailbox_view(token, "search", None, args))
                    .transpose()?
            } else {
                None
            };
            let (entity_hits, note_hits, per_backend, dropped) = self
                .inner
                .fan_out_search_with_token_and_count(request, token, args, extra_visible)
                .await;
            let started = search_profile::start();
            let result = self
                .hydrate_search_result(
                    token.namespace(),
                    entity_hits,
                    note_hits,
                    per_backend,
                    Some(token),
                    mailbox,
                )
                .await;
            search_profile::stage("hydrate", started, &[]);
            Ok((result, dropped))
        })
    }

    async fn hydrate_search_result(
        &self,
        namespace: &Namespace,
        entity_hits: Vec<SearchHit>,
        mut note_hits: Vec<NoteSearchHit>,
        per_backend: Vec<super::dispatch::BackendSearchResult>,
        caller: Option<&NamespaceToken>,
        mailbox: Option<khive_runtime::MailboxView>,
    ) -> CoordSearchResult {
        let partial = per_backend.iter().any(|r| r.error.is_some());

        // Batch-fetch entity kind + created_at for each merged entity hit.
        // We locate each hit's owning backend and call get_entity on it.
        // By-ID (locate/get_entity) is namespace-agnostic (ADR-007 Rev 6), so
        // `extra_visible` does not apply here — only the fan-out search above
        // is namespace-filtered.
        let mut entity_kinds: HashMap<Uuid, String> = HashMap::new();
        let mut entity_created_at: HashMap<Uuid, i64> = HashMap::new();
        let mut entity_updated_at: HashMap<Uuid, i64> = HashMap::new();
        let mut entity_versions: HashMap<Uuid, i64> = HashMap::new();
        for hit in &entity_hits {
            if khive_storage::request_read_is_cancelled() {
                break;
            }
            let backend_id = self.inner.locate(hit.entity_id, namespace).await;
            if let Some(bid) = backend_id {
                if let Some(entry) = self.inner.registry().get(&bid) {
                    let rt = &entry.runtime;
                    if let Ok(authorized) = rt.authorize(namespace.clone()) {
                        let token = caller.cloned().unwrap_or(authorized);
                        if let Ok(entity) = rt.get_entity(&token, hit.entity_id).await {
                            entity_created_at.insert(hit.entity_id, entity.created_at);
                            entity_updated_at.insert(hit.entity_id, entity.updated_at);
                            entity_versions.insert(hit.entity_id, entity.version);
                            entity_kinds.insert(hit.entity_id, entity.kind);
                        }
                    }
                }
            }
        }

        // Batch-fetch note kind + name + created_at for each merged note hit.
        let mut note_kinds: HashMap<Uuid, String> = HashMap::new();
        let mut note_created_at: HashMap<Uuid, i64> = HashMap::new();
        let mut note_updated_at: HashMap<Uuid, i64> = HashMap::new();
        let mut note_versions: HashMap<Uuid, i64> = HashMap::new();
        let mut note_names: HashMap<Uuid, Option<String>> = HashMap::new();
        for hit in &note_hits {
            if khive_storage::request_read_is_cancelled() {
                break;
            }
            let backend_id = self.inner.locate(hit.note_id, namespace).await;
            if let Some(bid) = backend_id {
                if let Some(entry) = self.inner.registry().get(&bid) {
                    let rt = &entry.runtime;
                    if let Ok(authorized) = rt.authorize(namespace.clone()) {
                        let token = caller.cloned().unwrap_or(authorized);
                        if let Ok(store) = rt.notes(&token) {
                            if let Ok(Some(note)) = store.get_note(hit.note_id).await {
                                if let Some(view) = mailbox.as_ref() {
                                    if !view.permits_message_note(&token, &note) {
                                        continue;
                                    }
                                }
                                note_versions.insert(hit.note_id, note.version);
                                note_created_at.insert(hit.note_id, note.created_at);
                                note_updated_at.insert(hit.note_id, note.updated_at);
                                note_names.insert(hit.note_id, note.name.clone());
                                note_kinds.insert(hit.note_id, note.kind);
                            }
                        }
                    }
                }
            }
        }

        if mailbox.is_some() {
            note_hits.retain(|hit| note_versions.contains_key(&hit.note_id));
        }
        let coord_per_backend: Vec<CoordBackendResult> = per_backend
            .into_iter()
            .map(|r| {
                let vector_selected = self
                    .inner
                    .registry()
                    .get(&r.backend_id)
                    .is_some_and(|entry| entry.runtime.vector_arm_selected());
                CoordBackendResult {
                    backend_id: r.backend_id,
                    entity_hits: r.hits,
                    note_hits: r.note_hits,
                    vector_selected,
                    error: r.error.map(|failure| CoordBackendFailure {
                        kind: match failure.kind {
                            BackendSearchFailureKind::BackendError => {
                                CoordBackendFailureKind::BackendError
                            }
                            BackendSearchFailureKind::Timeout => CoordBackendFailureKind::Timeout,
                        },
                        message: failure.message,
                    }),
                    vector_error: r.vector_error,
                }
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
            entity_updated_at,
            entity_versions,
            note_created_at,
            note_updated_at,
            note_versions,
            note_names,
        }
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

    #[allow(clippy::too_many_arguments)]
    async fn link(
        &self,
        namespace: &Namespace,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        resurrect: bool,
    ) -> Result<CoordLinkResult, CoordError> {
        self.inner
            .link_cross_backend_observed(
                namespace, source_id, target_id, relation, weight, metadata, resurrect,
            )
            .await
            .and_then(|row| {
                let cross_backend = row.edge.target_backend.is_some();
                let target_backend_id = row
                    .edge
                    .target_backend
                    .as_deref()
                    .map(BackendId::parse)
                    .transpose()
                    .map_err(|error| format!("stored target backend is invalid: {error}"))?;
                Ok(CoordLinkResult {
                    edge: row.edge,
                    cross_backend,
                    target_backend_id,
                    mutation: row.disposition,
                })
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
        search_profile::profiled(self.search_unscoped(request, namespace, extra_visible)).await
    }

    async fn fan_out_search_scoped(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        args: &serde_json::Value,
        extra_visible: &[Namespace],
    ) -> Result<CoordSearchResult, RuntimeError> {
        search_profile::profiled(async {
            self.search_scoped_with_count(request, token, args, extra_visible)
                .await
                .map(|(result, _)| result)
        })
        .await
    }

    async fn fan_out_note_search_scoped_with_count(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        args: &serde_json::Value,
        extra_visible: &[Namespace],
    ) -> Result<(CoordSearchResult, u64), RuntimeError> {
        search_profile::profiled(self.search_scoped_with_count(request, token, args, extra_visible))
            .await
    }

    fn is_single_backend(&self) -> bool {
        self.inner.is_single_backend()
    }
}

#[cfg(test)]
mod note_status_count_tests {
    use super::*;
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    use khive_runtime::{KhiveRuntime, PackRegistry, VerbRegistryBuilder};
    use khive_storage::note::{Note, NoteStore};
    use khive_storage::types::TextDocument;
    use serde_json::{json, Value};

    use crate::coordinator::BackendRegistry;

    const QUERY: &str = "coordinatorstatusfixture";

    // This bounds all cooperative setup, search and cleanup awaits in each case.
    // It does not preempt synchronous native code.
    async fn bounded(future: impl Future<Output = ()>) {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .expect("coordinator status fixture exceeded its async budget");
    }

    fn runtime() -> Arc<KhiveRuntime> {
        Arc::new(KhiveRuntime::memory().expect("memory runtime"))
    }

    fn search_request(
        runtime: &KhiveRuntime,
        status: Option<Value>,
    ) -> (ValidatedSearchRequest, Value) {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(runtime.config().gate.clone());
        builder.with_default_namespace(runtime.config().default_namespace.as_str());
        builder.with_actor_id(runtime.config().actor_id.clone());
        PackRegistry::register_packs(&["kg".to_string()], runtime.clone(), &mut builder)
            .expect("register KG pack");
        let registry = builder.build().expect("KG registry");
        runtime.install_edge_rules(registry.all_edge_rules());
        let mut args = json!({"kind": "note", "query": QUERY, "limit": 10});
        if let Some(status) = status {
            args["note_status"] = status;
        }
        let request = ValidatedSearchRequest::from_value(args.clone(), &registry)
            .expect("canonical note search");
        (request, args)
    }

    async fn seed(runtime: &KhiveRuntime, id: Uuid, status: &str) {
        let token = runtime
            .authorize(Namespace::local())
            .expect("fixture token");
        let mut note = Note::new("local", "observation", QUERY);
        note.id = id;
        note.status = status.into();
        // Isolated backend seeding makes lifecycle states explicit while the
        // search still uses real storage, text retrieval and runtime hydration.
        runtime
            .backend()
            .upsert_note(note)
            .await
            .expect("seed note");
        runtime
            .text_for_notes(&token)
            .expect("note text store")
            .upsert_document(TextDocument {
                subject_id: id,
                kind: khive_types::SubstrateKind::Note,
                record_kind: Some("observation".into()),
                namespace: "local".into(),
                title: None,
                body: QUERY.into(),
                tags: vec![],
                metadata: None,
                updated_at: std::time::SystemTime::now().into(),
            })
            .await
            .expect("index note");
    }

    #[tokio::test]
    async fn direct_leg_preserves_selector_and_counts_every_excluded_status() {
        bounded(async {
            let runtime = runtime();
            let active = Uuid::new_v4();
            let archived = Uuid::new_v4();
            let custom = Uuid::new_v4();
            seed(&runtime, active, "active").await;
            seed(&runtime, archived, "archived").await;
            seed(&runtime, custom, "snoozed").await;
            let token = runtime.authorize(Namespace::local()).expect("search token");
            let service = SubstrateCoordinatorService::new(SubstrateCoordinator::single(
                Arc::clone(&runtime),
            ));
            for (status, expected_count, mut expected_ids) in [
                (None, 2, vec![active]),
                (Some(Value::Null), 2, vec![active]),
                (Some(json!("active")), 2, vec![active]),
                (Some(json!("archived")), 2, vec![archived]),
                (Some(json!("any")), 0, vec![active, archived, custom]),
            ] {
                let (request, args) = search_request(&runtime, status);
                let (result, dropped) = service
                    .fan_out_note_search_scoped_with_count(&request, &token, &args, &[])
                    .await
                    .expect("scoped count search");
                assert_eq!(dropped, expected_count, "direct selector exclusion count");
                let mut actual_ids: Vec<_> =
                    result.note_hits.iter().map(|hit| hit.note_id).collect();
                actual_ids.sort();
                expected_ids.sort();
                assert_eq!(actual_ids, expected_ids);
                assert_eq!(result.per_backend.len(), 1);
                assert!(result.per_backend[0].error.is_none());
                assert!(!result.partial);
                assert!(result.entity_hits.is_empty());
            }
        })
        .await;
    }

    #[tokio::test]
    async fn spawned_legs_sum_counts_even_when_excluded_uuid_is_shared() {
        bounded(async {
            let left = runtime();
            let right = runtime();
            let shared_archived = Uuid::new_v4();
            let left_active = Uuid::new_v4();
            let right_active = Uuid::new_v4();
            seed(&left, shared_archived, "archived").await;
            seed(&right, shared_archived, "archived").await;
            seed(&right, Uuid::new_v4(), "snoozed").await;
            seed(&left, left_active, "active").await;
            seed(&right, right_active, "active").await;
            let token = left.authorize(Namespace::local()).expect("search token");
            let (request, args) = search_request(&left, None);
            let mut backends = BackendRegistry::new();
            backends.register(BackendId::main(), Arc::clone(&left));
            backends.register(BackendId::parse("secondary").unwrap(), right);
            let service = SubstrateCoordinatorService::new(SubstrateCoordinator::new(backends));
            let (result, dropped) = service
                .fan_out_note_search_scoped_with_count(&request, &token, &args, &[])
                .await
                .expect("fan-out count search");
            assert_eq!(
                dropped, 3,
                "completed backend counts must sum without UUID union"
            );
            let mut actual_ids: Vec<_> = result.note_hits.iter().map(|hit| hit.note_id).collect();
            actual_ids.sort();
            let mut expected_ids = vec![left_active, right_active];
            expected_ids.sort();
            assert_eq!(actual_ids, expected_ids);
            assert_eq!(result.per_backend.len(), 2);
            assert!(result.per_backend.iter().all(|leg| leg.error.is_none()));
            assert!(!result.partial);
            let (any_request, any_args) = search_request(&left, Some(json!("any")));
            let (any_result, any_dropped) = service
                .fan_out_note_search_scoped_with_count(&any_request, &token, &any_args, &[])
                .await
                .expect("fan-out Any search");
            assert_eq!(any_dropped, 0, "Any must not exclude any gathered note");
            assert_eq!(any_result.note_hits.len(), 4);
            assert!(!any_result.partial);
        })
        .await;
    }

    #[tokio::test]
    async fn failed_leg_contributes_neither_hits_nor_count() {
        bounded(async {
            let healthy = runtime();
            let failed = runtime();
            let active = Uuid::new_v4();
            seed(&healthy, active, "active").await;
            seed(&healthy, Uuid::new_v4(), "archived").await;
            seed(&failed, Uuid::new_v4(), "archived").await;
            seed(&failed, Uuid::new_v4(), "snoozed").await;
            let failed_token = failed
                .authorize(Namespace::local())
                .expect("failed fixture token");
            let (failed_request, failed_args) = search_request(&failed, None);
            let (_, prepared_count) =
                SubstrateCoordinatorService::new(SubstrateCoordinator::single(Arc::clone(&failed)))
                    .fan_out_note_search_scoped_with_count(
                        &failed_request,
                        &failed_token,
                        &failed_args,
                        &[],
                    )
                    .await
                    .expect("positive failed-backend premise");
            assert_eq!(prepared_count, 2, "failed fixture has two real exclusions");
            let token = healthy.authorize(Namespace::local()).expect("search token");
            let (request, args) = search_request(&healthy, None);
            let mut backends = BackendRegistry::new();
            backends.register(BackendId::main(), Arc::clone(&healthy));
            backends.register(BackendId::parse("failed").unwrap(), failed);
            let service = SubstrateCoordinatorService::new(
                SubstrateCoordinator::new(backends).with_failing_backend("failed"),
            );
            let (result, dropped) = service
                .fan_out_note_search_scoped_with_count(&request, &token, &args, &[])
                .await
                .expect("partial count search");
            assert_eq!(dropped, 1, "failed backend must not publish a status count");
            assert_eq!(result.note_hits.len(), 1);
            assert_eq!(result.note_hits[0].note_id, active);
            assert!(result.partial);
            let failed = result
                .per_backend
                .iter()
                .find(|leg| leg.backend_id.as_str() == "failed")
                .expect("failed leg");
            assert!(failed.error.is_some());
            assert!(failed.note_hits.is_empty());
        })
        .await;
    }
}
