use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use khive_mcp::coordinator::{CoordSearchResult, CoordinatorService};
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_pack_kg::handlers::{SearchSubstrate, ValidatedSearchRequest};
use khive_runtime::{
    BackendId, DomainDisposition, KhiveRuntime, Namespace, NamespaceToken, NoteSearchHit,
    PackRegistry, RankScoreKind, RuntimeConfig, RuntimeError, RuntimeResult, SearchHit,
    SearchSignals, SearchSource, StorageBackend, VerbRegistry, VerbRegistryBuilder,
};
use khive_score::DeterministicScore;
use khive_storage::{Entity, Note, StorageError};
use serde_json::{json, Value};
use tokio::sync::watch;
use uuid::Uuid;

use super::{
    scope_search_metadata_observer, BackendRegistry, BackendSearchResult, SearchMetadataPhase,
    SearchMetadataRead, SubstrateCoordinator, SubstrateCoordinatorService,
};

fn memory_runtime() -> Arc<KhiveRuntime> {
    let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
    backend.prepare_core_schema().expect("core schema");
    Arc::new(KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:search-cancellation".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        },
    ))
}

pub(super) struct SearchFixture {
    pub(super) runtime: Arc<KhiveRuntime>,
    empty: Arc<KhiveRuntime>,
    pub(super) namespace: Namespace,
    pub(super) token: NamespaceToken,
    pub(super) registry: VerbRegistry,
    pub(super) entities: Vec<Entity>,
    pub(super) notes: Vec<Note>,
    pub(super) entity_hits: Vec<SearchHit>,
    pub(super) note_hits: Vec<NoteSearchHit>,
}

impl SearchFixture {
    pub(super) async fn new() -> Self {
        let runtime = memory_runtime();
        let namespace = Namespace::local();
        let token = runtime.authorize(namespace.clone()).unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(runtime.config().gate.clone());
        builder.with_default_namespace(namespace.as_str());
        builder.with_actor_id(runtime.config().actor_id.clone());
        builder.with_runtime_event_store(&runtime).unwrap();
        PackRegistry::register_packs(&["kg".into()], (*runtime).clone(), &mut builder).unwrap();
        let registry = builder.build().unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        let mut entities = Vec::new();
        let mut notes = Vec::new();
        let mut entity_hits = Vec::new();
        let mut note_hits = Vec::new();
        for (index, created, updated) in [(0, 1_000_000, 4_000_000), (1, 2_000_000, 3_000_000)] {
            let entity_id = Uuid::from_u128(1 + index);
            let note_id = Uuid::from_u128(11 + index);
            let mut entity = Entity::minimal(
                entity_id,
                "local",
                "concept",
                format!("candidate {index}"),
                created,
                updated,
            );
            entity.properties = Some(json!({"fixture": index}));
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
            let mut note = Note::new("local", "observation", format!("candidate {index}"));
            note.id = note_id;
            note.name = Some(format!("note {index}"));
            note.created_at = created;
            note.updated_at = updated;
            note.properties = Some(json!({"fixture": index}));
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            entities.push(runtime.get_entity(&token, entity_id).await.unwrap());
            notes.push(
                runtime
                    .notes(&token)
                    .unwrap()
                    .get_note(note_id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            let hit = SearchHit {
                entity_id,
                score: DeterministicScore::from_f64(0.9 - index as f64 / 10.0),
                rank_score_kind: RankScoreKind::Keyword,
                signals: SearchSignals {
                    vector_similarity: None,
                    keyword_score: Some(DeterministicScore::from_raw(17 + index as i64)),
                },
                source: SearchSource::Text,
                title: Some(format!("candidate {index}")),
                snippet: Some(format!("snippet {index}")),
            };
            note_hits.push(NoteSearchHit {
                note_id,
                score: hit.score,
                rank_score_kind: hit.rank_score_kind,
                signals: hit.signals,
                source: hit.source,
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
            });
            entity_hits.push(hit);
        }
        Self {
            runtime,
            empty: memory_runtime(),
            namespace,
            token,
            registry,
            entities,
            notes,
            entity_hits,
            note_hits,
        }
    }

    pub(super) fn coordinator(&self) -> SubstrateCoordinator {
        let mut backends = BackendRegistry::new();
        backends.register(
            BackendId::parse("alpha").unwrap(),
            Arc::clone(&self.runtime),
        );
        backends.register(BackendId::parse("beta").unwrap(), Arc::clone(&self.empty));
        let coordinator = SubstrateCoordinator::new(backends)
            .with_entity_hits_override(HashMap::from([
                ("alpha".into(), self.entity_hits.clone()),
                ("beta".into(), vec![]),
            ]))
            .with_note_hits_override(HashMap::from([
                ("alpha".into(), self.note_hits.clone()),
                ("beta".into(), vec![]),
            ]));
        for id in self
            .entities
            .iter()
            .map(|row| row.id)
            .chain(self.notes.iter().map(|row| row.id))
        {
            coordinator.record_created(id, BackendId::parse("alpha").unwrap());
        }
        coordinator
    }

    pub(super) fn service(&self) -> SubstrateCoordinatorService {
        SubstrateCoordinatorService::new(self.coordinator())
    }

    pub(super) fn request(
        &self,
        substrate: SearchSubstrate,
        order: &str,
        range: bool,
    ) -> (ValidatedSearchRequest, Value) {
        let kind = match substrate {
            SearchSubstrate::Entity => "entity",
            SearchSubstrate::Note => "note",
        };
        let mut args = json!({"kind":kind,"query":"candidate","limit":10,"order_by":order});
        if range {
            args["created_after"] = json!("1970-01-01T00:00:00.500000Z");
            args["created_before"] = json!("1970-01-01T00:00:02.500000Z");
        }
        (
            ValidatedSearchRequest::from_value(args.clone(), &self.registry).unwrap(),
            args,
        )
    }

    pub(super) async fn assert_unchanged(&self) {
        for expected in &self.entities {
            let actual = self
                .runtime
                .get_entity(&self.token, expected.id)
                .await
                .unwrap();
            assert_eq!(actual.properties, expected.properties);
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        for expected in &self.notes {
            let actual = self
                .runtime
                .notes(&self.token)
                .unwrap()
                .get_note(expected.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&actual, expected);
        }
    }

    pub(super) async fn close(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.registry.shutdown_audit_batch())
            .await
            .expect("audit drain settles")
            .unwrap();
    }

    fn ids(&self, substrate: SearchSubstrate) -> Vec<Uuid> {
        match substrate {
            SearchSubstrate::Entity => self.entities.iter().map(|row| row.id).collect(),
            SearchSubstrate::Note => self.notes.iter().map(|row| row.id).collect(),
        }
    }

    fn hits(&self, substrate: SearchSubstrate) -> (Vec<SearchHit>, Vec<NoteSearchHit>) {
        match substrate {
            SearchSubstrate::Entity => (self.entity_hits.clone(), vec![]),
            SearchSubstrate::Note => (vec![], self.note_hits.clone()),
        }
    }
}

// Scoped to this task. The sender remains alive until the whole tested future
// returns; cancellation therefore means an explicit true, not sender closure.
pub(super) async fn observe<T>(
    phase: SearchMetadataPhase,
    substrate: SearchSubstrate,
    cancel_at: Option<Uuid>,
    future: impl Future<Output = T>,
) -> (T, Vec<SearchMetadataRead>) {
    let (sender, receiver) = watch::channel(false);
    let log = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&log);
    let signal = sender.clone();
    let observer: Arc<dyn Fn(SearchMetadataRead) + Send + Sync> = Arc::new(move |read| {
        let cancel =
            read.phase == phase && read.substrate == substrate && Some(read.id) == cancel_at;
        observed.lock().unwrap().push(read);
        if cancel {
            signal.send(true).expect("active scoped request");
        }
    });
    let result = khive_storage::scope_request_read_cancellation(
        receiver,
        scope_search_metadata_observer(observer, future),
    )
    .await;
    assert_eq!(
        *sender.borrow(),
        cancel_at.is_some(),
        "target read must be reached exactly through the observer"
    );
    let reads = std::mem::take(&mut *log.lock().unwrap());
    (result, reads)
}

pub(super) fn assert_reads(
    reads: &[SearchMetadataRead],
    phase: SearchMetadataPhase,
    substrate: SearchSubstrate,
    expected: &[Uuid],
) {
    assert_eq!(
        reads.len(),
        expected.len(),
        "no later metadata completion may escape cancellation"
    );
    for (read, id) in reads.iter().zip(expected) {
        assert_eq!(read.phase, phase);
        assert_eq!(read.substrate, substrate);
        assert_eq!(read.id, *id);
    }
}

pub(super) fn assert_stopped<T>(result: RuntimeResult<T>) {
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("cancelled metadata read returned success"),
    };
    assert!(
        matches!(error.refusal_source(), RuntimeError::Storage(StorageError::Timeout { operation }) if operation == "search"),
        "wrong source: {error}"
    );
    assert_eq!(
        error.batch_write_disposition(),
        Some(DomainDisposition::NotCommitted)
    );
}

async fn bounded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("search cancellation test settles");
}

fn hit_ids(
    entities: &[SearchHit],
    notes: &[NoteSearchHit],
    substrate: SearchSubstrate,
) -> Vec<Uuid> {
    match substrate {
        SearchSubstrate::Entity => entities.iter().map(|hit| hit.entity_id).collect(),
        SearchSubstrate::Note => notes.iter().map(|hit| hit.note_id).collect(),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn finalizer_stops_after_first_and_last_real_metadata_read() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let ids = fixture.ids(substrate);
            for (order, range, expected_order) in [
                ("score", true, vec![ids[0], ids[1]]),
                ("created_at", false, vec![ids[1], ids[0]]),
                ("updated_at", false, vec![ids[0], ids[1]]),
            ] {
                let (request, _) = fixture.request(substrate, order, range);
                let coordinator = fixture.coordinator();
                let (mut entities, mut notes) = fixture.hits(substrate);
                let (result, reads) = observe(
                    SearchMetadataPhase::Finalize,
                    substrate,
                    None,
                    coordinator.finalize_search_hits(
                        &mut entities,
                        &mut notes,
                        &request,
                        &fixture.namespace,
                    ),
                )
                .await;
                result.unwrap();
                assert_reads(&reads, SearchMetadataPhase::Finalize, substrate, &ids);
                assert_eq!(hit_ids(&entities, &notes, substrate), expected_order);
                for stop in 0..2 {
                    let (mut entities, mut notes) = fixture.hits(substrate);
                    let (result, reads) = observe(
                        SearchMetadataPhase::Finalize,
                        substrate,
                        Some(ids[stop]),
                        coordinator.finalize_search_hits(
                            &mut entities,
                            &mut notes,
                            &request,
                            &fixture.namespace,
                        ),
                    )
                    .await;
                    assert_stopped(result);
                    assert_reads(
                        &reads,
                        SearchMetadataPhase::Finalize,
                        substrate,
                        &ids[..=stop],
                    );
                }
            }
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn finalizer_refuses_stopped_empty_input_and_tolerates_missing_active_metadata() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let (request, _) = fixture.request(substrate, "created_at", false);
            let coordinator = fixture.coordinator();
            let (sender, receiver) = watch::channel(true);
            let mut empty_entities = vec![];
            let mut empty_notes = vec![];
            assert_stopped(
                khive_storage::scope_request_read_cancellation(
                    receiver,
                    coordinator.finalize_search_hits(
                        &mut empty_entities,
                        &mut empty_notes,
                        &request,
                        &fixture.namespace,
                    ),
                )
                .await,
            );
            assert!(*sender.borrow());
            assert!(empty_entities.is_empty() && empty_notes.is_empty());
            assert_stopped(
                khive_storage::scope_request_read_deadline(
                    Duration::ZERO,
                    coordinator.finalize_search_hits(
                        &mut empty_entities,
                        &mut empty_notes,
                        &request,
                        &fixture.namespace,
                    ),
                )
                .await,
            );
            assert!(empty_entities.is_empty() && empty_notes.is_empty());
            let missing = Uuid::from_u128(999);
            let (mut entities, mut notes) = fixture.hits(substrate);
            match substrate {
                SearchSubstrate::Entity => {
                    let mut hit = entities[0].clone();
                    hit.entity_id = missing;
                    entities.push(hit);
                }
                SearchSubstrate::Note => {
                    let mut hit = notes[0].clone();
                    hit.note_id = missing;
                    notes.push(hit);
                }
            }
            let (result, reads) = observe(
                SearchMetadataPhase::Finalize,
                substrate,
                None,
                coordinator.finalize_search_hits(
                    &mut entities,
                    &mut notes,
                    &request,
                    &fixture.namespace,
                ),
            )
            .await;
            result.unwrap();
            let ids = fixture.ids(substrate);
            assert_reads(&reads, SearchMetadataPhase::Finalize, substrate, &ids);
            assert_eq!(
                hit_ids(&entities, &notes, substrate),
                [ids[1], ids[0], missing]
            );
            let (range, _) = fixture.request(substrate, "score", true);
            coordinator
                .finalize_search_hits(&mut entities, &mut notes, &range, &fixture.namespace)
                .await
                .unwrap();
            assert_eq!(
                hit_ids(&entities, &notes, substrate),
                [ids[1], ids[0]],
                "ordinary missing timestamps remain excluded by an explicit range"
            );
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[derive(Clone, Copy, Debug)]
enum FanoutRoute {
    Plain,
    Visibility,
    Token,
    Count,
}

type FanoutResult = RuntimeResult<(Vec<SearchHit>, Vec<NoteSearchHit>, Vec<BackendSearchResult>)>;

async fn fanout(
    route: FanoutRoute,
    coordinator: &SubstrateCoordinator,
    fixture: &SearchFixture,
    request: &ValidatedSearchRequest,
    args: &Value,
) -> FanoutResult {
    match route {
        FanoutRoute::Plain => {
            coordinator
                .fan_out_search(request, &fixture.namespace)
                .await
        }
        FanoutRoute::Visibility => {
            coordinator
                .fan_out_search_with_visibility(request, &fixture.namespace, &[])
                .await
        }
        FanoutRoute::Token => {
            coordinator
                .fan_out_search_with_token(request, &fixture.token, args, &[])
                .await
        }
        FanoutRoute::Count => coordinator
            .fan_out_search_with_token_and_count(request, &fixture.token, args, &[])
            .await
            .map(|(entities, notes, backends, dropped)| {
                assert_eq!(dropped, 0);
                (entities, notes, backends)
            }),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn all_fanout_routes_propagate_finalizer_stop_for_entities_and_notes() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let ids = fixture.ids(substrate);
            let (request, args) = fixture.request(substrate, "created_at", false);
            for route in [
                FanoutRoute::Plain,
                FanoutRoute::Visibility,
                FanoutRoute::Token,
                FanoutRoute::Count,
            ] {
                let coordinator = fixture.coordinator();
                let (entities, notes, backends) =
                    fanout(route, &coordinator, &fixture, &request, &args)
                        .await
                        .unwrap();
                assert_eq!(
                    hit_ids(&entities, &notes, substrate),
                    [ids[1], ids[0]],
                    "positive {route:?}"
                );
                assert_eq!(backends.len(), 2);
                assert!(backends.iter().all(|backend| backend.error.is_none()));
                let (result, reads) = observe(
                    SearchMetadataPhase::Finalize,
                    substrate,
                    Some(ids[0]),
                    fanout(route, &coordinator, &fixture, &request, &args),
                )
                .await;
                assert_stopped(result);
                assert_reads(&reads, SearchMetadataPhase::Finalize, substrate, &ids[..1]);
            }
        }
        let empty = SubstrateCoordinator::new(BackendRegistry::new());
        let (request, args) = fixture.request(SearchSubstrate::Entity, "score", false);
        for route in [
            FanoutRoute::Plain,
            FanoutRoute::Visibility,
            FanoutRoute::Token,
            FanoutRoute::Count,
        ] {
            let (sender, receiver) = watch::channel(true);
            assert_stopped(
                khive_storage::scope_request_read_cancellation(
                    receiver,
                    fanout(route, &empty, &fixture, &request, &args),
                )
                .await,
            );
            assert!(*sender.borrow());
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[derive(Clone, Copy, Debug)]
enum ServiceRoute {
    Unscoped,
    Scoped,
    Count,
}

async fn service_search(
    route: ServiceRoute,
    service: &SubstrateCoordinatorService,
    fixture: &SearchFixture,
    request: &ValidatedSearchRequest,
    args: &Value,
) -> RuntimeResult<CoordSearchResult> {
    match route {
        ServiceRoute::Unscoped => {
            service
                .fan_out_search(request, &fixture.namespace, &[])
                .await
        }
        ServiceRoute::Scoped => {
            service
                .fan_out_search_scoped(request, &fixture.token, args, &[])
                .await
        }
        ServiceRoute::Count => service
            .fan_out_note_search_scoped_with_count(request, &fixture.token, args, &[])
            .await
            .map(|(result, dropped)| {
                assert_eq!(dropped, 0);
                result
            }),
    }
}

fn assert_complete(
    fixture: &SearchFixture,
    result: &CoordSearchResult,
    substrate: SearchSubstrate,
) {
    let ids = fixture.ids(substrate);
    assert_eq!(
        hit_ids(&result.entity_hits, &result.note_hits, substrate),
        ids
    );
    assert!(!result.partial);
    assert_eq!(result.per_backend.len(), 2);
    assert!(result
        .per_backend
        .iter()
        .all(|backend| backend.error.is_none()));
    for (index, id) in ids.iter().enumerate() {
        let expected_score =
            khive_score::rrf_score_zero_based(index, khive_retrieval::DEFAULT_RRF_K);
        match substrate {
            SearchSubstrate::Entity => {
                let hit = &result.entity_hits[index];
                assert_eq!(hit.score, expected_score);
                assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
                assert_eq!(hit.signals, fixture.entity_hits[index].signals);
                assert_eq!(hit.source, SearchSource::Text);
                assert_eq!(hit.title, fixture.entity_hits[index].title);
                assert_eq!(hit.snippet, fixture.entity_hits[index].snippet);
                assert_eq!(result.entity_kinds[id], "concept");
                assert_eq!(
                    result.entity_created_at[id],
                    fixture.entities[index].created_at
                );
                assert_eq!(
                    result.entity_updated_at[id],
                    fixture.entities[index].updated_at
                );
                assert_eq!(result.entity_versions[id], fixture.entities[index].version);
            }
            SearchSubstrate::Note => {
                let hit = &result.note_hits[index];
                assert_eq!(hit.score, expected_score);
                assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
                assert_eq!(hit.signals, fixture.note_hits[index].signals);
                assert_eq!(hit.source, SearchSource::Text);
                assert_eq!(hit.title, fixture.note_hits[index].title);
                assert_eq!(hit.snippet, fixture.note_hits[index].snippet);
                assert_eq!(result.note_kinds[id], "observation");
                assert_eq!(result.note_created_at[id], fixture.notes[index].created_at);
                assert_eq!(result.note_updated_at[id], fixture.notes[index].updated_at);
                assert_eq!(result.note_versions[id], fixture.notes[index].version);
                assert_eq!(result.note_names[id], fixture.notes[index].name);
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn service_routes_propagate_hydration_stop_without_finalizer_masking() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let ids = fixture.ids(substrate);
            let (request, args) = fixture.request(substrate, "score", false);
            for route in [
                ServiceRoute::Unscoped,
                ServiceRoute::Scoped,
                ServiceRoute::Count,
            ] {
                let service = fixture.service();
                let (result, reads) = observe(
                    SearchMetadataPhase::Hydrate,
                    substrate,
                    None,
                    service_search(route, &service, &fixture, &request, &args),
                )
                .await;
                assert_complete(
                    &fixture,
                    &result.unwrap_or_else(|error| panic!("positive {route:?}: {error}")),
                    substrate,
                );
                assert_reads(&reads, SearchMetadataPhase::Hydrate, substrate, &ids);
                for stop in 0..2 {
                    let (result, reads) = observe(
                        SearchMetadataPhase::Hydrate,
                        substrate,
                        Some(ids[stop]),
                        service_search(route, &service, &fixture, &request, &args),
                    )
                    .await;
                    assert_stopped(result);
                    assert_reads(
                        &reads,
                        SearchMetadataPhase::Hydrate,
                        substrate,
                        &ids[..=stop],
                    );
                }
                let (finalize_request, finalize_args) =
                    fixture.request(substrate, "created_at", false);
                let (result, reads) = observe(
                    SearchMetadataPhase::Finalize,
                    substrate,
                    Some(ids[0]),
                    service_search(route, &service, &fixture, &finalize_request, &finalize_args),
                )
                .await;
                assert_stopped(result);
                assert_reads(&reads, SearchMetadataPhase::Finalize, substrate, &ids[..1]);
            }
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn backend_failure_remains_partial_success_while_parent_read_is_active() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        let coordinator = fixture.coordinator().with_failing_backend("beta");
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let (sender, receiver) = watch::channel(false);
            khive_storage::scope_request_read_cancellation(receiver, async {
                let (request, _) = fixture.request(substrate, "score", false);
                let service = SubstrateCoordinatorService::new(
                    fixture.coordinator().with_failing_backend("beta"),
                );
                let result = service
                    .fan_out_search(&request, &fixture.namespace, &[])
                    .await
                    .unwrap_or_else(|error| panic!("ordinary backend failure escaped: {error}"));
                assert_eq!(
                    hit_ids(&result.entity_hits, &result.note_hits, substrate),
                    fixture.ids(substrate)
                );
                assert!(result.partial);
                assert_eq!(result.per_backend.len(), 2);
                let failed = result
                    .per_backend
                    .iter()
                    .find(|row| row.backend_id.as_str() == "beta")
                    .unwrap();
                assert_eq!(
                    failed.error.as_ref().unwrap().kind,
                    khive_mcp::coordinator::BackendSearchFailureKind::BackendError
                );
                let (entities, notes, _) = coordinator
                    .fan_out_search(&request, &fixture.namespace)
                    .await
                    .unwrap();
                assert_eq!(
                    hit_ids(&entities, &notes, substrate),
                    fixture.ids(substrate)
                );
                khive_storage::ensure_request_read_active("search")
                    .expect("backend failure did not stop parent request");
                sender.send(true).expect("request scope remains installed");
                let error = khive_storage::ensure_request_read_active("search")
                    .expect_err("explicit stop must be visible in the parent request");
                assert!(
                    matches!(error, StorageError::Timeout { operation } if operation == "search")
                );
            })
            .await;
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn actual_coordinator_metadata_stop_has_no_mcp_page_and_preserves_proof() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        let server = KhiveMcpServer::from_registry(fixture.registry.clone())
            .with_coordinator(Arc::new(fixture.service()));
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let ids = fixture.ids(substrate);
            for (phase, order) in [
                (SearchMetadataPhase::Finalize, "created_at"),
                (SearchMetadataPhase::Hydrate, "score"),
            ] {
                let (_, args) = fixture.request(substrate, order, false);
                let params = || RequestParams {
                    ops: json!([{"tool":"search", "args":args.clone()}]).to_string(),
                    presentation: Some("verbose".into()),
                    format: Some("json".into()),
                    ..Default::default()
                };
                let positive: Value =
                    serde_json::from_str(&server.dispatch_request_local(params()).await.unwrap())
                        .unwrap();
                let entry = &positive["results"][0];
                assert_eq!(entry["ok"], true, "{entry}");
                let rows = entry["result"].as_array().expect("positive result page");
                let expected = if order == "created_at" {
                    vec![ids[1], ids[0]]
                } else {
                    ids.clone()
                };
                assert_eq!(
                    rows.iter()
                        .map(|row| Uuid::parse_str(row["id"].as_str().unwrap()).unwrap())
                        .collect::<Vec<_>>(),
                    expected
                );
                let (response, reads) = observe(
                    phase,
                    substrate,
                    Some(ids[0]),
                    server.dispatch_request_local(params()),
                )
                .await;
                let response: Value =
                    serde_json::from_str(&response.expect("operation failure remains an envelope"))
                        .unwrap();
                assert_eq!(response["results"].as_array().unwrap().len(), 1);
                let entry = &response["results"][0];
                assert_eq!(entry["ok"], false, "{entry}");
                assert!(entry.get("result").is_none(), "no partial page: {entry}");
                assert_eq!(entry["domain_disposition"], "not_committed");
                assert_eq!(entry["error"]["domain_disposition"], "not_committed");
                assert_eq!(entry["error"]["kind"], "runtime_error");
                assert_eq!(entry["error"]["retryable"], true);
                assert_eq!(
                    entry["error"]["message"],
                    RuntimeError::Storage(StorageError::Timeout {
                        operation: "search".into()
                    })
                    .to_string()
                );
                assert_reads(&reads, phase, substrate, &ids[..1]);
                fixture.assert_unchanged().await;
            }
        }
        fixture.close().await;
    })
    .await;
}

fn read_substrate(substrate: SearchSubstrate) -> khive_runtime::error::SearchReadSubstrate {
    match substrate {
        SearchSubstrate::Entity => khive_runtime::error::SearchReadSubstrate::Entity,
        SearchSubstrate::Note => khive_runtime::error::SearchReadSubstrate::Note,
    }
}

fn install_read_fault(
    runtime: &KhiveRuntime,
    substrate: SearchSubstrate,
    denials: Arc<std::sync::atomic::AtomicUsize>,
) {
    use khive_db::test_hooks::TestHooks;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::Ordering;
    assert_eq!(
        runtime.backend().pool().max_readers(),
        0,
        "private memory reads share the writer connection"
    );
    let table = match substrate {
        SearchSubstrate::Entity => "entities",
        SearchSubstrate::Note => "notes",
    };
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit_sqlite(|connection| {
            connection.test_authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Read { table_name, .. } if table_name == table)
                && denials.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        })
        .unwrap();
}

fn remove_read_fault(runtime: &KhiveRuntime) {
    use khive_db::test_hooks::TestHooks;
    use rusqlite::hooks::{AuthContext, Authorization};
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit_sqlite(|connection| {
            connection.test_authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        })
        .unwrap();
}

fn assert_read_failure<T>(result: RuntimeResult<T>, backend: &str, substrate: SearchSubstrate) {
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("required search read returned success"),
    };
    let RuntimeError::SearchRead { context } = &error else {
        panic!("missing typed search context: {error}")
    };
    assert_eq!(context.backend().as_str(), backend);
    assert_eq!(context.substrate(), read_substrate(substrate));
    assert!(
        matches!(
            context.source(),
            RuntimeError::Storage(StorageError::Driver { .. })
        ),
        "actual database denial must be the cause: {error:?}"
    );
    let value = khive_runtime::failure_entry_parts(khive_runtime::FailureEntrySource::Runtime {
        error,
        boundary: DomainDisposition::Unknown,
    })
    .into_error();
    assert_eq!(value["code"], "search_read_failed");
    assert_eq!(value["cause_class"], "storage");
    assert_eq!(
        value["search_context"],
        json!({"backend": backend, "substrate": read_substrate(substrate).as_str()})
    );
}

async fn fault_metadata<T>(
    fixture: &SearchFixture,
    phase: SearchMetadataPhase,
    substrate: SearchSubstrate,
    target: Uuid,
    future: impl Future<Output = RuntimeResult<T>>,
) -> (RuntimeResult<T>, Vec<SearchMetadataRead>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    let (ready_sender, ready_receiver) = oneshot::channel();
    let (resume_sender, resume_receiver) = oneshot::channel();
    let ready = Arc::new(Mutex::new(Some(ready_sender)));
    let resume = Arc::new(Mutex::new(Some(resume_receiver)));
    let hook: super::SearchMetadataHook = Arc::new(move |read| {
        let selected = read.phase == phase && read.substrate == substrate && read.id == target;
        let paused = if selected {
            Some((
                ready.lock().unwrap().take().expect("one selected read"),
                resume.lock().unwrap().take().expect("one resume"),
            ))
        } else {
            None
        };
        Box::pin(async move {
            if let Some((ready, resume)) = paused {
                ready.send(read).expect("controller alive");
                resume.await.expect("controller releases read");
            }
        })
    });
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&log);
    let observer: Arc<dyn Fn(SearchMetadataRead) + Send + Sync> =
        Arc::new(move |read| recorded.lock().unwrap().push(read));
    let denials = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = watch::channel(false);
    let search = khive_storage::scope_request_read_cancellation(receiver, async {
        let result = super::scope_before_search_metadata(
            hook,
            scope_search_metadata_observer(observer, future),
        )
        .await;
        let active = khive_storage::ensure_request_read_active("search");
        (result, active)
    });
    let controller = async {
        let read = ready_receiver.await.expect("metadata hook reached");
        install_read_fault(&fixture.runtime, substrate, Arc::clone(&denials));
        resume_sender.send(()).expect("paused read still alive");
        read
    };
    let joined = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(search, controller)
    })
    .await;
    // The joined futures hold no connection here, including on timeout.
    remove_read_fault(&fixture.runtime);
    let ((result, active), read) = joined.expect("fault controller and search settle");
    assert_eq!(
        read,
        SearchMetadataRead {
            phase,
            substrate,
            id: target
        }
    );
    assert_eq!(
        denials.load(Ordering::SeqCst),
        1,
        "exactly one actual read denied"
    );
    assert!(
        !*sender.borrow(),
        "no cancellation substitutes for the read fault"
    );
    active.expect("parent remains active after the read failure");
    let reads = std::mem::take(&mut *log.lock().unwrap());
    (result, reads)
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn required_metadata_errors_refuse_first_and_later_reads() {
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let ids = fixture.ids(substrate);
            for (phase, order, range) in [
                (SearchMetadataPhase::Finalize, "score", true),
                (SearchMetadataPhase::Finalize, "created_at", false),
                (SearchMetadataPhase::Finalize, "updated_at", false),
                (SearchMetadataPhase::Hydrate, "score", false),
            ] {
                let (request, _) = fixture.request(substrate, order, range);
                for index in 0..2 {
                    let coordinator = fixture.coordinator();
                    let service = fixture.service();
                    let (mut entities, mut notes) = fixture.hits(substrate);
                    let future = async {
                        match phase {
                            SearchMetadataPhase::Finalize => {
                                coordinator
                                    .finalize_search_hits(
                                        &mut entities,
                                        &mut notes,
                                        &request,
                                        &fixture.namespace,
                                    )
                                    .await
                            }
                            SearchMetadataPhase::Hydrate => service
                                .fan_out_search(&request, &fixture.namespace, &[])
                                .await
                                .map(|_| ()),
                        }
                    };
                    let (result, reads) =
                        fault_metadata(&fixture, phase, substrate, ids[index], future).await;
                    assert_reads(&reads, phase, substrate, &ids[..index]);
                    assert_read_failure(result, "alpha", substrate);
                    fixture.assert_unchanged().await;
                }
                let healthy = fixture
                    .service()
                    .fan_out_search(&request, &fixture.namespace, &[])
                    .await
                    .unwrap();
                assert_eq!(healthy.entity_kinds.len() + healthy.note_kinds.len(), 2);
                assert_eq!(
                    healthy.entity_versions.len() + healthy.note_versions.len(),
                    2
                );
                assert!(!healthy.partial);
            }
        }
        fixture.close().await;
    })
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn selected_owner_read_errors_are_not_missing_hits() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            for (order, range) in [("score", false), ("created_at", false), ("score", true)] {
                let (request, _) = fixture.request(substrate, order, range);
                let service = fixture.service();
                let denials = Arc::new(AtomicUsize::new(0));
                install_read_fault(&fixture.runtime, substrate, Arc::clone(&denials));
                let (sender, receiver) = watch::channel(false);
                let result = khive_storage::scope_request_read_cancellation(receiver, async {
                    let result = service
                        .fan_out_search(&request, &fixture.namespace, &[])
                        .await;
                    (result, khive_storage::ensure_request_read_active("search"))
                })
                .await;
                remove_read_fault(&fixture.runtime);
                assert_eq!(denials.load(Ordering::SeqCst), 1);
                assert!(!*sender.borrow());
                result.1.expect("parent stays active");
                assert_read_failure(result.0, "alpha", substrate);
            }
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cold_search_owner_distinguishes_misses_failures_and_surviving_owner() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    bounded(async {
        let fixture = SearchFixture::new().await;
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            let empty = SubstrateCoordinator::new(BackendRegistry::new());
            assert!(empty
                .locate_for_search(
                    Uuid::from_u128(999),
                    &fixture.namespace,
                    read_substrate(substrate)
                )
                .await
                .unwrap()
                .is_none());
            let mut backends = BackendRegistry::new();
            backends.register(
                BackendId::parse("alpha").unwrap(),
                Arc::clone(&fixture.empty),
            );
            backends.register(
                BackendId::parse("beta").unwrap(),
                Arc::clone(&fixture.runtime),
            );
            let coordinator = SubstrateCoordinator::new(backends);
            let missing = Uuid::from_u128(999);
            assert!(coordinator
                .locate_for_search(missing, &fixture.namespace, read_substrate(substrate))
                .await
                .unwrap()
                .is_none());
            for (id, found) in [(missing, false), (fixture.ids(substrate)[0], true)] {
                let denials = Arc::new(AtomicUsize::new(0));
                install_read_fault(&fixture.empty, substrate, Arc::clone(&denials));
                let result = coordinator
                    .locate_for_search(id, &fixture.namespace, read_substrate(substrate))
                    .await;
                remove_read_fault(&fixture.empty);
                assert_eq!(denials.load(Ordering::SeqCst), 1);
                if found {
                    assert_eq!(result.unwrap().unwrap().as_str(), "beta");
                } else {
                    assert_read_failure(result, "alpha", substrate);
                }
            }
            // A cached owner that confirms absence retains the optional result.
            coordinator.record_created(missing, BackendId::parse("alpha").unwrap());
            assert!(coordinator
                .locate_for_search(missing, &fixture.namespace, read_substrate(substrate))
                .await
                .unwrap()
                .is_none());
            coordinator.record_created(missing, BackendId::parse("removed").unwrap());
            assert!(coordinator
                .locate_for_search(missing, &fixture.namespace, read_substrate(substrate))
                .await
                .unwrap()
                .is_none());
        }
        fixture.assert_unchanged().await;
        fixture.close().await;
    })
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn metadata_rows_disappearing_after_lookup_remain_true_misses() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    bounded(async {
        for substrate in [SearchSubstrate::Entity, SearchSubstrate::Note] {
            for phase in [SearchMetadataPhase::Finalize, SearchMetadataPhase::Hydrate] {
                let fixture = SearchFixture::new().await;
                let id = fixture.ids(substrate)[0];
                let runtime = Arc::clone(&fixture.runtime);
                let token = fixture.token.clone();
                let mut entity = fixture.entities[0].clone();
                let mut note = fixture.notes[0].clone();
                entity.deleted_at = Some(9_000_000);
                note.deleted_at = Some(9_000_000);
                let reached = Arc::new(AtomicUsize::new(0));
                let count = Arc::clone(&reached);
                let hook: super::SearchMetadataHook = Arc::new(move |read| {
                    let runtime = Arc::clone(&runtime);
                    let token = token.clone();
                    let entity = entity.clone();
                    let note = note.clone();
                    let count = Arc::clone(&count);
                    Box::pin(async move {
                        if read
                            == (SearchMetadataRead {
                                phase,
                                substrate,
                                id,
                            })
                        {
                            assert_eq!(count.fetch_add(1, Ordering::SeqCst), 0);
                            match substrate {
                                SearchSubstrate::Entity => runtime
                                    .entities(&token)
                                    .unwrap()
                                    .upsert_entity(entity)
                                    .await
                                    .unwrap(),
                                SearchSubstrate::Note => runtime
                                    .notes(&token)
                                    .unwrap()
                                    .upsert_note(note)
                                    .await
                                    .unwrap(),
                            }
                        }
                    })
                });
                let (request, _) =
                    fixture.request(substrate, "score", phase == SearchMetadataPhase::Finalize);
                let service = fixture.service();
                let result = super::scope_before_search_metadata(
                    hook,
                    service.fan_out_search(&request, &fixture.namespace, &[]),
                )
                .await
                .expect("genuine disappearance remains tolerated");
                assert_eq!(reached.load(Ordering::SeqCst), 1);
                assert!(!result.partial);
                assert_eq!(result.entity_kinds.len() + result.note_kinds.len(), 1);
                let expected = if phase == SearchMetadataPhase::Finalize {
                    vec![fixture.ids(substrate)[1]]
                } else {
                    fixture.ids(substrate)
                };
                assert_eq!(
                    hit_ids(&result.entity_hits, &result.note_hits, substrate),
                    expected
                );
                match substrate {
                    SearchSubstrate::Entity => assert_eq!(
                        fixture
                            .runtime
                            .entities(&fixture.token)
                            .unwrap()
                            .get_entity_including_deleted(id)
                            .await
                            .unwrap()
                            .unwrap()
                            .deleted_at,
                        Some(9_000_000)
                    ),
                    SearchSubstrate::Note => assert_eq!(
                        fixture
                            .runtime
                            .notes(&fixture.token)
                            .unwrap()
                            .get_note_including_deleted(id)
                            .await
                            .unwrap()
                            .unwrap()
                            .deleted_at,
                        Some(9_000_000)
                    ),
                }
                fixture.close().await;
            }
        }
    })
    .await;
}

fn mailbox_registry(runtime: &Arc<KhiveRuntime>, actor: &str) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(runtime.config().gate.clone());
    builder.with_default_namespace("local");
    builder.with_actor_id(Some(actor.into()));
    builder.with_runtime_event_store(runtime).unwrap();
    PackRegistry::register_packs(
        &["kg".into(), "comm".into()],
        (**runtime).clone(),
        &mut builder,
    )
    .unwrap();
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn mailbox_filtered_hit_never_reads_its_faulted_owner() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    bounded(async {
        let alpha = memory_runtime();
        let beta = memory_runtime();
        let sender_registry = mailbox_registry(&alpha, "sender");
        let sent = sender_registry.dispatch_with_identity("comm.send", json!({
            "to": "recipient", "content": "ordinary private mailbox content", "idempotency_key": "search-owner-filter",
        }), Some(khive_runtime::RequestIdentity { namespace: "local".into(), actor_id: Some("sender".into()), ..Default::default() })).await.unwrap();
        let id = Uuid::parse_str(sent["full_id"].as_str().unwrap()).unwrap();
        let alpha_token = alpha.authorize(Namespace::local()).unwrap();
        let beta_token = beta.authorize(Namespace::local()).unwrap();
        let outbound = alpha.notes(&alpha_token).unwrap().get_note(id).await.unwrap().unwrap();
        beta.notes(&beta_token).unwrap().upsert_note(outbound.clone()).await.unwrap();
        let owner_copy = beta.notes(&beta_token).unwrap().get_note(id).await.unwrap().unwrap();
        let hit = NoteSearchHit {
            note_id: id, score: DeterministicScore::from_f64(0.9), rank_score_kind: RankScoreKind::Keyword,
            signals: SearchSignals { vector_similarity: None, keyword_score: Some(DeterministicScore::from_raw(17)) },
            source: SearchSource::Text, title: Some("private title".into()), snippet: Some("ordinary private mailbox content".into()),
        };
        let mut backends = BackendRegistry::new();
        backends.register(BackendId::parse("alpha").unwrap(), Arc::clone(&alpha));
        backends.register(BackendId::parse("beta").unwrap(), Arc::clone(&beta));
        let coordinator = SubstrateCoordinator::new(backends).with_note_hits_override(HashMap::from([
            ("alpha".into(), vec![hit]), ("beta".into(), vec![]),
        ]));
        coordinator.record_created(id, BackendId::parse("beta").unwrap());
        let service = Arc::new(SubstrateCoordinatorService::new(coordinator));
        let hidden_registry = mailbox_registry(&alpha, "unrelated");
        let permitted_registry = mailbox_registry(&alpha, "sender");
        let hidden = KhiveMcpServer::from_registry(hidden_registry.clone()).with_coordinator(service.clone());
        let permitted = KhiveMcpServer::from_registry(permitted_registry.clone()).with_coordinator(service);
        for order in ["created_at", "score"] {
            let params = || RequestParams {
                ops: json!([{"tool": "search", "args": {"kind": "note", "query": "mailbox", "order_by": order, "limit": 10}}]).to_string(),
                presentation: Some("verbose".into()), format: Some("json".into()), ..Default::default()
            };
            // The permitted positive establishes that this real candidate exists.
            let positive: Value = serde_json::from_str(&permitted.dispatch_request_local(params()).await.unwrap()).unwrap();
            assert_eq!(positive["results"][0]["ok"], true, "{positive}");
            assert_eq!(positive["results"][0]["result"].as_array().unwrap().len(), 1);
            assert_eq!(positive["results"][0]["result"][0]["id"], id.to_string());
            let denials = Arc::new(AtomicUsize::new(0));
            install_read_fault(&beta, SearchSubstrate::Note, Arc::clone(&denials));
            let hidden_response = hidden.dispatch_request_local(params()).await;
            let hidden_denials = denials.load(Ordering::SeqCst);
            let permitted_response = permitted.dispatch_request_local(params()).await;
            remove_read_fault(&beta);
            let hidden_response: Value = serde_json::from_str(&hidden_response.unwrap()).unwrap();
            assert_eq!(hidden_response["results"][0]["ok"], true, "{hidden_response}");
            assert_eq!(hidden_response["results"][0]["result"], json!([]));
            assert!(!hidden_response.to_string().contains(&id.to_string()));
            assert!(!hidden_response.to_string().contains("ordinary private mailbox content"));
            assert_eq!(hidden_denials, 0, "early mailbox filter precedes owner lookup");
            assert_eq!(denials.load(Ordering::SeqCst), 1, "permitted candidate proves the owner fault was armed");
            let refused: Value = serde_json::from_str(&permitted_response.unwrap()).unwrap();
            let entry = &refused["results"][0];
            assert_eq!(entry["ok"], false, "{entry}");
            assert!(entry.get("result").is_none());
            assert_eq!(entry["error"]["code"], "search_read_failed");
            assert_eq!(entry["error"]["search_context"], json!({"backend": "beta", "substrate": "note"}));
            assert!(!entry.to_string().contains(&id.to_string()));
        }
        assert_eq!(alpha.notes(&alpha_token).unwrap().get_note(id).await.unwrap().unwrap(), outbound);
        assert_eq!(beta.notes(&beta_token).unwrap().get_note(id).await.unwrap().unwrap(), owner_copy);
        for registry in [sender_registry, hidden_registry, permitted_registry] {
            tokio::time::timeout(Duration::from_secs(5), registry.shutdown_audit_batch()).await.unwrap().unwrap();
        }
    }).await;
}
