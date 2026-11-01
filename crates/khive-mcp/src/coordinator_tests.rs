use super::*;
use khive_pack_kg::handlers::SearchSubstrate;
use khive_runtime::{NoteSearchHit, SearchHit, SearchSource};
use serde_json::{json, Value};
use std::sync::Arc;

/// Minimal mock for server-routing tests (T6 in the test plan).
pub struct MockCoordinator {
    pub link_called: std::sync::atomic::AtomicBool,
    pub search_called: std::sync::atomic::AtomicBool,
    pub single_backend: bool,
    pub failed_backend: Option<BackendId>,
    /// A backend whose vector arm alone fails — its text arm still runs
    /// and contributes hits, so it must not appear in `failed_backend`'s
    /// whole-backend-error reporting.
    pub vector_failed_backend: Option<BackendId>,
    /// When `true`, `fan_out_search` returns zero hits regardless of
    /// substrate — used to construct the "complete-empty" (healthy, no
    /// match) and "degraded-empty" (backend failed, no survivor)
    /// envelope fixtures (ADR-130 §1 regression coverage).
    pub empty_hits: bool,
    /// Metadata delivered by the most recent intercepted singleton link.
    pub last_link_metadata: std::sync::Mutex<Option<Option<Value>>>,
    pub last_search_request: std::sync::Mutex<Option<ValidatedSearchRequest>>,
    /// The `limit` value `fan_out_search` was last called with (MCP-AUD-003).
    pub last_limit: std::sync::atomic::AtomicU32,
    /// The `extra_visible` slice passed to `fan_out_search`, preserving
    /// the caller's resolved extra read-visibility scope.
    pub last_extra_visible: std::sync::Mutex<Vec<Namespace>>,
}

impl MockCoordinator {
    pub fn multi_backend() -> Arc<Self> {
        Arc::new(Self {
            link_called: std::sync::atomic::AtomicBool::new(false),
            search_called: std::sync::atomic::AtomicBool::new(false),
            single_backend: false,
            failed_backend: None,
            vector_failed_backend: None,
            empty_hits: false,
            last_link_metadata: std::sync::Mutex::new(None),
            last_search_request: std::sync::Mutex::new(None),
            last_limit: std::sync::atomic::AtomicU32::new(0),
            last_extra_visible: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Healthy (no backend failure), but the merged result set is empty —
    /// a genuine no-match. Pairs with `degraded_empty_multi_backend` to
    /// distinguish "complete-empty" from "degraded-empty" (ADR-130 §1).
    pub fn empty_multi_backend() -> Arc<Self> {
        Arc::new(Self {
            link_called: std::sync::atomic::AtomicBool::new(false),
            search_called: std::sync::atomic::AtomicBool::new(false),
            single_backend: false,
            failed_backend: None,
            vector_failed_backend: None,
            empty_hits: true,
            last_link_metadata: std::sync::Mutex::new(None),
            last_search_request: std::sync::Mutex::new(None),
            last_limit: std::sync::atomic::AtomicU32::new(0),
            last_extra_visible: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn degraded_multi_backend(failed_backend: &str) -> Arc<Self> {
        Arc::new(Self {
            link_called: std::sync::atomic::AtomicBool::new(false),
            search_called: std::sync::atomic::AtomicBool::new(false),
            single_backend: false,
            failed_backend: Some(BackendId::parse(failed_backend).expect("valid backend id")),
            vector_failed_backend: None,
            empty_hits: false,
            last_link_metadata: std::sync::Mutex::new(None),
            last_search_request: std::sync::Mutex::new(None),
            last_limit: std::sync::atomic::AtomicU32::new(0),
            last_extra_visible: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// A backend failed AND no hit survives — the `search_incomplete`
    /// fixture (ADR-130 §1).
    pub fn degraded_empty_multi_backend(failed_backend: &str) -> Arc<Self> {
        Arc::new(Self {
            link_called: std::sync::atomic::AtomicBool::new(false),
            search_called: std::sync::atomic::AtomicBool::new(false),
            single_backend: false,
            failed_backend: Some(BackendId::parse(failed_backend).expect("valid backend id")),
            vector_failed_backend: None,
            empty_hits: true,
            last_link_metadata: std::sync::Mutex::new(None),
            last_search_request: std::sync::Mutex::new(None),
            last_limit: std::sync::atomic::AtomicU32::new(0),
            last_extra_visible: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// A backend's vector arm alone failed — its text arm still ran and
    /// contributed hits, so this must read as `status="complete"` with
    /// `arm_participation.text.status="ran"`, not as a whole-backend
    /// failure.
    pub fn vector_degraded_multi_backend(vector_failed_backend: &str) -> Arc<Self> {
        Arc::new(Self {
            link_called: std::sync::atomic::AtomicBool::new(false),
            search_called: std::sync::atomic::AtomicBool::new(false),
            single_backend: false,
            failed_backend: None,
            vector_failed_backend: Some(
                BackendId::parse(vector_failed_backend).expect("valid backend id"),
            ),
            empty_hits: false,
            last_link_metadata: std::sync::Mutex::new(None),
            last_search_request: std::sync::Mutex::new(None),
            last_limit: std::sync::atomic::AtomicU32::new(0),
            last_extra_visible: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn single_backend_instance() -> Arc<Self> {
        Arc::new(Self {
            link_called: std::sync::atomic::AtomicBool::new(false),
            search_called: std::sync::atomic::AtomicBool::new(false),
            single_backend: true,
            failed_backend: None,
            vector_failed_backend: None,
            empty_hits: false,
            last_link_metadata: std::sync::Mutex::new(None),
            last_search_request: std::sync::Mutex::new(None),
            last_limit: std::sync::atomic::AtomicU32::new(0),
            last_extra_visible: std::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl CoordinatorService for MockCoordinator {
    async fn fan_out_search_scoped(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        _args: &serde_json::Value,
        extra_visible: &[Namespace],
    ) -> Result<CoordSearchResult, RuntimeError> {
        // This fake only renders observation notes; message membership is
        // exercised by the concrete coordinator's integration scenarios.
        Ok(self
            .fan_out_search(request, token.gate_namespace(), extra_visible)
            .await)
    }

    async fn fan_out_note_search_scoped_with_count(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        args: &serde_json::Value,
        extra_visible: &[Namespace],
    ) -> Result<(CoordSearchResult, u64), RuntimeError> {
        // This routing-only fixture contains exactly one active observation
        // candidate when nonempty, and no excluded candidates otherwise. Real
        // hydration/backend-sum semantics are tested by the concrete service.
        let selector = request.note_status().ok_or_else(|| {
            RuntimeError::Internal("note routing fixture requires a note selector".into())
        })?;
        let mut result = self
            .fan_out_search_scoped(request, token, args, extra_visible)
            .await?;
        let count =
            if selector == khive_storage::note::NoteStatusSelector::Archived && !self.empty_hits {
                result.note_hits.clear();
                1
            } else {
                0
            };
        Ok((result, count))
    }

    async fn locate(&self, _id: Uuid) -> Option<BackendId> {
        Some(BackendId::main())
    }

    fn record_created(&self, _id: Uuid, _backend_id: BackendId) {}

    fn primary_backend_id(&self) -> Option<BackendId> {
        Some(BackendId::main())
    }

    #[allow(clippy::too_many_arguments)]
    async fn link(
        &self,
        _namespace: &Namespace,
        _source_id: Uuid,
        _target_id: Uuid,
        _relation: EdgeRelation,
        _weight: f64,
        metadata: Option<serde_json::Value>,
        _resurrect: bool,
    ) -> Result<CoordLinkResult, CoordError> {
        self.link_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *self.last_link_metadata.lock().unwrap() = Some(metadata);
        Err(CoordError::UnknownNode { id: Uuid::new_v4() })
    }

    async fn fan_out_search(
        &self,
        request: &ValidatedSearchRequest,
        _namespace: &Namespace,
        extra_visible: &[Namespace],
    ) -> CoordSearchResult {
        self.search_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.last_limit
            .store(request.limit(), std::sync::atomic::Ordering::SeqCst);
        *self.last_search_request.lock().unwrap() = Some(request.clone());
        *self.last_extra_visible.lock().unwrap() = extra_visible.to_vec();
        let id = Uuid::from_u128(1);
        let is_note = request.substrate() == SearchSubstrate::Note;
        CoordSearchResult {
            entity_hits: if is_note || self.empty_hits {
                vec![]
            } else {
                vec![SearchHit {
                    entity_id: id,
                    score: Default::default(),
                    rank_score_kind: khive_runtime::RankScoreKind::Rrf,
                    signals: khive_runtime::SearchSignals::default(),
                    source: SearchSource::Both,
                    title: Some("entity result".to_string()),
                    snippet: None,
                }]
            },
            note_hits: if is_note && !self.empty_hits {
                vec![NoteSearchHit {
                    note_id: id,
                    score: Default::default(),
                    rank_score_kind: khive_runtime::RankScoreKind::Vector,
                    signals: khive_runtime::SearchSignals::default(),
                    source: SearchSource::Vector,
                    title: Some("note result".to_string()),
                    snippet: None,
                }]
            } else {
                vec![]
            },
            per_backend: std::iter::once(BackendSearchResult {
                backend_id: BackendId::main(),
                entity_hits: vec![],
                note_hits: vec![],
                vector_selected: true,
                error: None,
                vector_error: None,
            })
            .chain(
                self.failed_backend
                    .iter()
                    .cloned()
                    .map(|backend_id| BackendSearchResult {
                        backend_id,
                        entity_hits: vec![],
                        note_hits: vec![],
                        vector_selected: true,
                        error: Some(BackendSearchFailure::backend("injected search failure")),
                        vector_error: None,
                    }),
            )
            .chain(
                self.vector_failed_backend
                    .iter()
                    .cloned()
                    .map(|backend_id| BackendSearchResult {
                        backend_id,
                        entity_hits: vec![SearchHit {
                            entity_id: id,
                            score: Default::default(),
                            rank_score_kind: khive_runtime::RankScoreKind::Keyword,
                            signals: khive_runtime::SearchSignals::default(),
                            source: SearchSource::Text,
                            title: Some("entity result".to_string()),
                            snippet: None,
                        }],
                        note_hits: vec![],
                        vector_selected: true,
                        error: None,
                        vector_error: Some("injected vector-arm failure".to_string()),
                    }),
            )
            .collect(),
            partial: self.failed_backend.is_some(),
            entity_kinds: std::collections::HashMap::from([(id, "concept".to_string())]),
            note_kinds: std::collections::HashMap::from([(id, "observation".to_string())]),
            entity_created_at: std::collections::HashMap::from([(id, 1_700_000_000_000_000)]),
            entity_updated_at: std::collections::HashMap::from([(id, 1_700_000_000_000_000)]),
            entity_versions: std::collections::HashMap::from([(id, 1)]),
            note_created_at: std::collections::HashMap::from([(id, 1_700_000_000_000_000)]),
            note_updated_at: std::collections::HashMap::from([(id, 1_700_000_000_000_000)]),
            note_versions: std::collections::HashMap::from([(id, 1)]),
            note_names: std::collections::HashMap::from([(id, Some("note result".to_string()))]),
        }
    }

    fn is_single_backend(&self) -> bool {
        self.single_backend
    }
}

// ── T6: server-level coordinator routing ─────────────────────────────────

use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;
use khive_runtime::{
    AllowAllGate, Gate, GateDecision, GateError, GateRef, GateRequest, KhiveRuntime,
    Namespace as RuntimeNamespace, RuntimeConfig,
};
use khive_storage::{Event, EventFilter, PageRequest};
use khive_types::{EventKind, EventOutcome};

fn make_registry() -> (khive_runtime::VerbRegistry, khive_runtime::KhiveRuntime) {
    make_registry_with_gate(Arc::new(AllowAllGate))
}

fn make_registry_with_gate(
    gate: GateRef,
) -> (khive_runtime::VerbRegistry, khive_runtime::KhiveRuntime) {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: RuntimeNamespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        gate: Arc::clone(&gate),
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let default_ns = runtime.config().default_namespace.clone();
    let actor_id = runtime.config().actor_id.clone();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.with_default_namespace(default_ns.as_str());
    builder.with_actor_id(actor_id);
    builder
        .with_runtime_event_store(&runtime)
        .expect("configure trusted runtime audit store");
    khive_runtime::PackRegistry::register_packs(&["kg".to_string()], runtime.clone(), &mut builder)
        .expect("register kg");
    let registry = builder.build().expect("build registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (registry, runtime)
}

#[derive(Debug, Default)]
struct CapturingGate {
    requests: std::sync::Mutex<Vec<GateRequest>>,
    deny: bool,
}

impl CapturingGate {
    fn denying() -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
            deny: true,
        }
    }
}

impl Gate for CapturingGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        self.requests.lock().unwrap().push(req.clone());
        if self.deny && req.verb != "authorize" {
            Ok(GateDecision::deny("denied by coordinator parity test"))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

async fn audit_events(runtime: &KhiveRuntime, namespace: &str) -> Vec<Event> {
    let token = runtime
        .authorize(RuntimeNamespace::parse(namespace).expect("audit namespace"))
        .expect("authorize audit query");
    runtime
        .events(&token)
        .expect("runtime event store")
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                ..EventFilter::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query audit events")
        .items
}

/// T6a: a multi-backend server MUST route `link` through the coordinator.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6a_multi_backend_server_routes_link_through_coordinator() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let src_id = Uuid::new_v4();
    let tgt_id = Uuid::new_v4();
    let ops = format!(
        r#"link(source_id="{}", target_id="{}", relation="implements")"#,
        src_id, tgt_id
    );
    let _result = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops,
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await;

    assert!(
            coord
                .link_called
                .load(std::sync::atomic::Ordering::SeqCst),
            "T6a: coordinator.link must be called when a link op is dispatched through a multi-backend server"
        );
}

/// A top-level dependency_kind must reach the coordinator just as it does
/// the single-backend KG handler; explicit metadata wins on a conflict.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_link_preserves_dependency_kind_metadata() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();

    for (metadata, expected) in [
        (
            r#"{"tag":"keep"}"#,
            json!({"tag": "keep", "dependency_kind": "artifact"}),
        ),
        (
            r#"{"dependency_kind":"runtime"}"#,
            json!({"dependency_kind": "runtime"}),
        ),
    ] {
        *coord.last_link_metadata.lock().unwrap() = None;
        let ops = format!(
            r#"link(source_id="{source_id}", target_id="{target_id}", relation="depends_on", metadata={metadata}, dependency_kind="artifact")"#
        );
        server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("coordinator dispatch must return an operation result");
        assert_eq!(
            coord.last_link_metadata.lock().unwrap().clone(),
            Some(Some(expected)),
            "coordinator must see the KG handler's merged metadata"
        );
    }
}

/// Dispatches one request on `server` and returns its first result entry.
async fn first_result_entry(server: &KhiveMcpServer, ops: &str) -> Value {
    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: ops.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("per-op failures are results, not dispatch errors");
    let response: Value = serde_json::from_str(&raw).expect("JSON response");
    response["results"][0].clone()
}

/// Every accepted spelling of a singleton link must take the coordinator
/// path, not only the canonical one.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_link_routes_every_accepted_spelling_through_the_coordinator() {
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    for ops in [
        format!(r#"link(source_id="{source_id}", target_id="{target_id}", relation="implements")"#),
        format!(r#"link(source="{source_id}", target="{target_id}", kind="implements")"#),
        format!(
            r#"link(source="{source_id}", target_id="{target_id}", kind="implements", dependency_kind="artifact")"#
        ),
    ] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        first_result_entry(&server, &ops).await;
        assert!(
            coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
            "{ops} must be routed through the coordinator"
        );
    }
}

/// A call giving both spellings of one field is refused by the link handler
/// even when the values agree. The coordinator intercept must not accept it
/// by ignoring one spelling: it falls through, and the refusal is the one
/// the registry path returns for the same call.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_link_refuses_both_spellings_of_one_field_like_the_handler() {
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    for (ops, alias) in [
        (
            format!(
                r#"link(source_id="{source_id}", target_id="{target_id}", relation="implements", kind="implements")"#
            ),
            "kind",
        ),
        (
            format!(
                r#"link(source_id="{source_id}", source="{source_id}", target_id="{target_id}", relation="implements")"#
            ),
            "source",
        ),
        (
            format!(
                r#"link(source_id="{source_id}", target_id="{target_id}", target="{target_id}", relation="implements")"#
            ),
            "target",
        ),
    ] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let entry = first_result_entry(&server, &ops).await;

        let (registry, _runtime) = make_registry();
        let registry_only = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg");
        let handler_entry = first_result_entry(&registry_only, &ops).await;

        assert_eq!(entry["ok"], json!(false), "{ops} must be refused: {entry}");
        assert!(
            entry["error"]
                .to_string()
                .contains(&format!("`{alias}` is an alias for")),
            "{ops} must be refused as an alias conflict: {entry}"
        );
        assert_eq!(
            entry["error"], handler_entry["error"],
            "{ops} must be refused exactly as the handler refuses it"
        );
        assert!(
            !coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
            "{ops} must not reach the coordinator"
        );
    }
}

/// The same edge in the canonical and the aliased spelling is one write, so a
/// parallel batch refuses both ops before either reaches the coordinator.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_mixed_spelling_links_to_one_edge_conflict_before_dispatch() {
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    for ops in [
        format!(
            r#"[link(source_id="{source_id}", target_id="{target_id}", relation="implements", weight=0.1), link(source_id="{source_id}", target_id="{target_id}", relation="implements", weight=0.9)]"#
        ),
        format!(
            r#"[link(source_id="{source_id}", target_id="{target_id}", relation="implements", weight=0.1), link(source="{source_id}", target="{target_id}", kind="implements", weight=0.9)]"#
        ),
    ] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: ops.clone(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("conflicts are per-op errors");
        let response: Value = serde_json::from_str(&raw).expect("JSON response");
        for entry in response["results"].as_array().expect("result rows") {
            assert_eq!(entry["ok"], json!(false), "{ops}: {entry}");
            assert!(
                entry["error"].to_string().contains("writes overlap"),
                "{ops} must conflict: {entry}"
            );
        }
        assert!(
            !coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
            "{ops}: neither op may reach the coordinator"
        );
    }
}

/// T6b: a multi-backend server MUST route `search` through the coordinator.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6b_multi_backend_server_routes_search_through_coordinator() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let _result = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="anything")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await;

    assert!(
            coord
                .search_called
                .load(std::sync::atomic::Ordering::SeqCst),
            "T6b: coordinator.fan_out_search must be called when a search op is dispatched through a multi-backend server"
        );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_search_forwards_the_complete_validated_filter_contract() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    for (ops, expected_substrate) in [
        (
            r#"search(kind="concept", query="typed entity", limit=4, entity_kind="concept", entity_type="theorem", properties={"tier":"hot"}, tags=["reviewed"], min_score=0.25)"#,
            SearchSubstrate::Entity,
        ),
        (
            r#"search(kind="observation", query="typed note", limit=5, note_kind="observation", include_superseded=true, properties={"status":"open"}, tags=["urgent"], min_score=0.5)"#,
            SearchSubstrate::Note,
        ),
    ] {
        server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: ops.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("validated search must dispatch");

        let captured = coord
            .last_search_request
            .lock()
            .unwrap()
            .clone()
            .expect("coordinator must receive a validated request");
        assert_eq!(captured.substrate(), expected_substrate);
        match expected_substrate {
            SearchSubstrate::Entity => {
                assert_eq!(captured.query(), "typed entity");
                assert_eq!(captured.limit(), 4);
                assert_eq!(captured.kind_filter(), Some("concept"));
                assert_eq!(captured.entity_type(), Some("theorem"));
                assert!(!captured.include_superseded());
                assert_eq!(captured.properties(), Some(&json!({"tier": "hot"})));
                assert_eq!(captured.tags(), &["reviewed".to_string()]);
                assert_eq!(captured.min_score(), 0.25);
            }
            SearchSubstrate::Note => {
                assert_eq!(captured.query(), "typed note");
                assert_eq!(captured.limit(), 5);
                assert_eq!(captured.kind_filter(), Some("observation"));
                assert_eq!(captured.entity_type(), None);
                assert!(captured.include_superseded());
                assert_eq!(captured.properties(), Some(&json!({"status": "open"})));
                assert_eq!(captured.tags(), &["urgent".to_string()]);
                assert_eq!(captured.min_score(), 0.5);
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_search_rejects_filters_for_the_wrong_substrate() {
    for ops in [
        r#"search(kind="entity", query="x", note_kind="observation")"#,
        r#"search(kind="entity", query="x", include_superseded=true)"#,
        r#"search(kind="note", query="x", entity_kind="concept")"#,
        r#"search(kind="note", query="x", entity_type="theorem")"#,
    ] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: ops.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("validation failures are per-op errors");
        let response: Value = serde_json::from_str(&raw).expect("JSON response");
        let entry = &response["results"][0];
        assert_eq!(entry["ok"], json!(false), "unexpected response: {entry}");
        assert!(
            entry["error"]
                .to_string()
                .contains("only valid when kind resolves"),
            "error must explain the substrate mismatch: {entry}"
        );
        assert!(!coord
            .search_called
            .load(std::sync::atomic::Ordering::SeqCst));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn degraded_search_advisory_survives_single_batch_chain_and_presentation() {
    let cases = [
        (r#"search(kind="note", query="x")"#, None, true),
        (
            r#"[search(kind="entity", query="x"), stats()]"#,
            None,
            false,
        ),
        (r#"search(kind="entity", query="x") | stats()"#, None, false),
        (r#"search(kind="entity", query="x")"#, Some("human"), false),
    ];

    for (ops, presentation, is_note) in cases {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::degraded_multi_backend("archive");
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: ops.to_string(),
                presentation: presentation.map(str::to_string),
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("degraded search still returns successful partial data");
        let response: Value = serde_json::from_str(&raw).expect("JSON response");
        let search = &response["results"][0];
        assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
        assert!(search["result"].is_array());
        assert_eq!(
            search["status"],
            json!("partial"),
            "unexpected response: {search}"
        );
        assert_eq!(search["partial"], json!(true));
        assert_eq!(search["missing_backends"], json!(["archive"]));
        let expected_text_candidates = usize::from(!is_note);
        assert_eq!(
            search["arm_participation"],
            json!({
                "text": {"mode": "all_terms", "status": "error", "candidate_count": expected_text_candidates},
                "vector": {"status": "error", "candidate_count": 1}
            }),
            "selected arms must remain typed on partial-with-hit responses"
        );
        assert_eq!(
            search["backend_errors"],
            json!({
                "archive": {
                    "kind": "backend_error",
                    "message": "injected search failure"
                }
            })
        );
        assert_eq!(response["status"], json!("success"));
    }
}

/// ADR-130 §1 completeness contract, complete-empty case: a healthy
/// (no backend failure) search with zero merged hits is a genuine
/// no-match — `ok: true`, `status: "complete"`, empty `result`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn search_complete_empty_reports_status_complete_and_stays_ok() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::empty_multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="nothing matches")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("a clean no-match is still a successful dispatch");
    let response: Value = serde_json::from_str(&raw).expect("JSON response");
    let search = &response["results"][0];
    assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
    assert_eq!(search["status"], json!("complete"));
    assert_eq!(search["result"], json!([]));
    assert_eq!(
        search["arm_participation"],
        json!({
            "text": {
                "mode": "all_terms",
                "status": "ran",
                "candidate_count": 0,
                "reason": "No text candidate survived matching, filtering, fusion, and the result limit. Plain text search combines normalized term groups conjunctively; try fewer terms."
            },
            "vector": {"status": "ran", "candidate_count": 0}
        })
    );
    assert!(search.get("partial").is_none());
    assert!(search.get("missing_backends").is_none());
    assert!(search.get("backend_errors").is_none());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinated_search_reports_effective_any_term_mode_for_both_substrates() {
    for kind in ["entity", "note"] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::empty_multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let raw = server
            .dispatch_request_local(RequestParams {
                ops: format!(
                    r#"search(kind="{kind}", query="nothing matches", text_mode="any_term")"#
                ),
                ..Default::default()
            })
            .await
            .expect("coordinated search dispatch");
        let response: Value = serde_json::from_str(&raw).expect("JSON response");
        let entry = &response["results"][0];
        assert_eq!(entry["ok"], true, "{kind}: {entry}");
        assert_eq!(entry["arm_participation"]["text"]["mode"], "any_term");
        assert_eq!(
            entry["arm_participation"]["text"]["reason"],
            "No text candidate survived matching, filtering, fusion, and the result limit."
        );
        assert_eq!(
            coord
                .last_search_request
                .lock()
                .unwrap()
                .as_ref()
                .expect("validated request")
                .text_mode_name(),
            "any_term"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinated_any_term_mode_survives_partial_and_incomplete_results() {
    for (kind, partial) in [("entity", true), ("entity", false), ("note", false)] {
        let (registry, _runtime) = make_registry();
        let coord = if partial {
            MockCoordinator::degraded_multi_backend("archive")
        } else {
            MockCoordinator::degraded_empty_multi_backend("archive")
        };
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let raw = server
            .dispatch_request_local(RequestParams {
                ops: format!(
                    r#"search(kind="{kind}", query="nothing matches", text_mode="any_term")"#
                ),
                ..Default::default()
            })
            .await
            .expect("coordinated search dispatch");
        let response: Value = serde_json::from_str(&raw).expect("JSON response");
        let entry = &response["results"][0];
        let arms = if partial {
            assert_eq!(entry["ok"], true, "{kind}: {entry}");
            assert_eq!(entry["status"], "partial");
            &entry["arm_participation"]
        } else {
            assert_eq!(entry["ok"], false, "{kind}: {entry}");
            assert_eq!(entry["error"]["kind"], "search_incomplete");
            &entry["error"]["arm_participation"]
        };
        assert_eq!(arms["text"]["mode"], "any_term");
        assert_eq!(arms["text"]["status"], "error");
        assert!(arms["text"].get("reason").is_none());
        assert!(arms.get("vector").is_some());
        assert_eq!(arms.as_object().expect("arm object").len(), 2);
    }
}

/// A vector-arm-only failure (the text arm still ran and contributed a
/// hit) must read as a healthy `status="complete"` response, never a
/// whole-backend failure: no `partial`/`missing_backends`/`backend_errors`,
/// and `arm_participation` alone carries the vector arm's error while the
/// text arm still reports `"ran"`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn search_vector_arm_failure_reports_complete_status_with_arm_participation_error() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::vector_degraded_multi_backend("archive");
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="LoRA")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("a vector-arm-only failure is still a successful dispatch");
    let response: Value = serde_json::from_str(&raw).expect("JSON response");
    let search = &response["results"][0];
    assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
    assert_eq!(
        search["status"],
        json!("complete"),
        "unexpected response: {search}"
    );
    assert!(
        !search["result"].as_array().unwrap().is_empty(),
        "text arm's hit must survive: {search}"
    );
    assert!(
        search.get("partial").is_none(),
        "a vector-arm-only failure must not read as partial: {search}"
    );
    assert!(
        search.get("missing_backends").is_none(),
        "a backend that returned text hits is not missing: {search}"
    );
    assert!(
        search.get("backend_errors").is_none(),
        "unexpected response: {search}"
    );
    assert_eq!(
        search["arm_participation"]["text"]["status"],
        json!("ran"),
        "unexpected response: {search}"
    );
    assert_eq!(
        search["arm_participation"]["vector"]["status"],
        json!("error"),
        "unexpected response: {search}"
    );
}

/// ADR-130 §1 completeness contract, degraded-empty case: a backend
/// failed and nothing survived — the operation must fail outright with
/// `error.kind: "search_incomplete"`, never a successful empty result.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn search_degraded_empty_returns_search_incomplete_error() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::degraded_empty_multi_backend("archive");
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="degraded")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("dispatch itself succeeds; the per-op entry carries ok:false");
    let response: Value = serde_json::from_str(&raw).expect("JSON response");
    let search = &response["results"][0];
    assert_eq!(search["ok"], json!(false), "unexpected response: {search}");
    assert!(
        search.get("result").is_none(),
        "unexpected response: {search}"
    );
    assert_eq!(search["error"]["kind"], json!("search_incomplete"));
    assert_eq!(search["error"]["retryable"], json!(false));
    assert_eq!(search["error"]["missing_backends"], json!(["archive"]));
    assert_eq!(
        search["error"]["arm_participation"],
        json!({
            "text": {"mode": "all_terms", "status": "error", "candidate_count": 0},
            "vector": {"status": "error", "candidate_count": 0}
        })
    );
    assert_eq!(
        search["error"]["backend_errors"],
        json!({
            "archive": {
                "kind": "backend_error",
                "message": "injected search failure"
            }
        })
    );
    assert!(search["error"]["message"].as_str().is_some());
}

/// ADR-130 §1, post-filter-empty case: pre-filter fusion found a hit, but
/// `min_score` removed it — completeness is judged AFTER filtering, so
/// this is also `search_incomplete`, not a successful empty result.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn search_degraded_hit_removed_by_min_score_returns_search_incomplete() {
    let (registry, _runtime) = make_registry();
    // `degraded_multi_backend` returns one hit with score 0.0 (Default).
    let coord = MockCoordinator::degraded_multi_backend("archive");
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="degraded", min_score=0.5)"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("dispatch itself succeeds; the per-op entry carries ok:false");
    let response: Value = serde_json::from_str(&raw).expect("JSON response");
    let search = &response["results"][0];
    assert_eq!(search["ok"], json!(false), "unexpected response: {search}");
    assert_eq!(search["error"]["kind"], json!("search_incomplete"));
    assert_eq!(search["error"]["missing_backends"], json!(["archive"]));
    assert_eq!(
        search["error"]["backend_errors"]["archive"]["message"],
        json!("injected search failure")
    );
}

/// MIN-1: the coordinator's serialized entity/note rows must carry the
/// same canonical fields as the KG pack's single-backend search handler
/// (`crates/khive-pack-kg/src/handlers/search.rs`) — `kind` (duplicating
/// entity_kind/note_kind), `name`, and `created_at` — not just the
/// compatibility subset.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_search_rows_carry_kg_handler_row_shape_parity() {
    for (kind, kind_field) in [("entity", "entity_kind"), ("note", "note_kind")] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: format!(r#"search(kind="{kind}", query="anything")"#),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("search dispatch must succeed");
        let response: Value = serde_json::from_str(&raw).expect("response must be valid JSON");
        let hit = &response["results"][0]["result"][0];

        assert!(
            hit.get("id").and_then(Value::as_str).is_some(),
            "{kind}: {hit}"
        );
        assert!(
            hit.get("kind").and_then(Value::as_str).is_some(),
            "{kind}: {hit}"
        );
        assert_eq!(hit["kind"], hit[kind_field], "{kind}: {hit}");
        assert!(hit.get("name").is_some(), "{kind}: missing name: {hit}");
        assert!(
            hit.get("created_at").and_then(Value::as_str).is_some(),
            "{kind}: missing created_at: {hit}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinator_and_registry_routes_submit_equivalent_link_and_search_gate_requests() {
    let direct_gate = Arc::new(CapturingGate::default());
    let coordinator_gate = Arc::new(CapturingGate::default());
    let (direct_registry, _direct_runtime) =
        make_registry_with_gate(Arc::clone(&direct_gate) as GateRef);
    let (coordinator_registry, coordinator_runtime) =
        make_registry_with_gate(Arc::clone(&coordinator_gate) as GateRef);
    let direct_server =
        KhiveMcpServer::from_registry_with_meta(direct_registry, "local", "test-cfg");
    let coord = MockCoordinator::multi_backend();
    let coordinator_server =
        KhiveMcpServer::from_registry_with_meta(coordinator_registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let operations = [
        format!(
            r#"link(source_id="{source_id}", target_id="{target_id}", relation="implements", namespace="tenant-a")"#
        ),
        r#"search(kind="entity", query="gate parity", limit=7, namespace="tenant-a")"#.to_string(),
    ];

    for ops in operations {
        for server in [&direct_server, &coordinator_server] {
            server
                .dispatch_request_local(RequestParams {
                    plan: None,
                    ops: ops.clone(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                })
                .await
                .expect("dispatch returns a per-operation result");
        }
    }

    let direct_requests: Vec<_> = direct_gate
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request.verb.as_str(), "link" | "search"))
        .cloned()
        .collect();
    let coordinator_requests: Vec<_> = coordinator_gate
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request.verb.as_str(), "link" | "search"))
        .cloned()
        .collect();
    assert_eq!(direct_requests.len(), 2);
    assert_eq!(coordinator_requests.len(), 2);
    for (direct, coordinated) in direct_requests.iter().zip(&coordinator_requests) {
        assert_eq!(
            serde_json::to_value(direct).unwrap(),
            serde_json::to_value(coordinated).unwrap()
        );
    }

    let coordinator_audits = audit_events(&coordinator_runtime, "tenant-a").await;
    assert_eq!(coordinator_audits.len(), 2);
    assert!(coordinator_audits
        .iter()
        .all(|event| event.payload["decision"] == "allow"));
    assert!(coordinator_audits
        .iter()
        .any(|event| event.verb == "link" && event.outcome == EventOutcome::Error));
    assert!(coordinator_audits
        .iter()
        .any(|event| event.verb == "search" && event.outcome == EventOutcome::Success));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinator_route_gates_and_audits_before_search_filter_validation() {
    let gate = Arc::new(CapturingGate::denying());
    let (registry, runtime) = make_registry_with_gate(Arc::clone(&gate) as GateRef);
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                // `note_kind` is invalid for an entity search. Denial must win
                // before the intercepted handler validates that filter.
                ops: r#"search(kind="entity", query="gate parity", note_kind="observation", namespace="tenant-a")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("dispatch returns a denied per-operation result");
    let response: Value = serde_json::from_str(&raw).expect("JSON response");
    let error = response["results"][0]["error"].to_string();
    assert!(
        error.contains("denied by coordinator parity test"),
        "gate denial must precede handler validation: {response}"
    );
    assert!(!error.contains("note_kind"));

    assert_eq!(
        gate.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.verb == "search")
            .count(),
        1
    );
    assert!(!coord
        .search_called
        .load(std::sync::atomic::Ordering::SeqCst));
    let audits = audit_events(&runtime, "tenant-a").await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].verb, "search");
    assert_eq!(audits[0].outcome, EventOutcome::Denied);
    assert_eq!(audits[0].payload["decision"], "deny");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_search_serializes_entity_and_note_sources() {
    for (kind, expected_source) in [("entity", "both"), ("note", "vector")] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: format!(r#"search(kind="{kind}", query="anything")"#),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("search dispatch must succeed");
        let response: serde_json::Value =
            serde_json::from_str(&raw).expect("response must be valid JSON");
        let entry = &response["results"][0];
        let hit = &entry["result"][0];

        assert_eq!(
            hit.get("source").and_then(serde_json::Value::as_str),
            Some(expected_source),
            "{kind} hit must expose its retrieval source; got: {hit}"
        );
        let expected_text_candidates = usize::from(kind == "entity");
        let mut expected_participation = json!({
            "text": {"mode": "all_terms", "status": "ran", "candidate_count": expected_text_candidates},
            "vector": {"status": "ran", "candidate_count": 1}
        });
        if kind == "note" {
            expected_participation["text"]["reason"] = json!(
                    "No text candidate survived matching, filtering, fusion, and the result limit. Plain text search combines normalized term groups conjunctively; try fewer terms."
                );
        }
        assert_eq!(
            entry["arm_participation"], expected_participation,
            "{kind} search must count final candidates by source membership"
        );
        assert!(entry.get("partial").is_none());
        assert!(entry.get("missing_backends").is_none());
    }
}

/// T6d: a multi-backend search with a malformed `tags` value must return a
/// per-op error rather than silently returning unfiltered results (see
/// crates/khive-mcp/docs/api/coordinator.md#t6d for the regression this guards).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6d_malformed_tags_return_per_op_error_in_multi_backend() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    // Pass a non-string entry in the tags array; the strict parser must reject this.
    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="anything", tags=[42])"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("T6d: dispatch must not return an MCP-level error");

    let result_val: serde_json::Value =
        serde_json::from_str(&raw).expect("T6d: response must be valid JSON");
    let first = result_val
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .expect("T6d: results array must be non-empty");
    assert_eq!(
        first.get("ok").and_then(serde_json::Value::as_bool),
        Some(false),
        "T6d: malformed tags must produce ok=false; got {:?}",
        first
    );
    // The coordinator must NOT have been called — rejection happens before dispatch.
    assert!(
        !coord
            .search_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "T6d: coordinator must not be reached when tags validation fails"
    );
}

/// T6e / PR #549 blocker: a multi-backend `search` with a malformed
/// `namespace` must fail closed and never reach the coordinator (see
/// crates/khive-mcp/docs/api/coordinator.md#t6e-namespace for the RUNTIME-AUD-002 regression).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6e_multi_backend_search_malformed_namespace_fails_closed() {
    let cases: [(&str, &str); 5] = [
        ("null", "null"),
        ("number", "42"),
        ("boolean", "true"),
        ("array", r#"["local"]"#),
        ("object", r#"{"ns":"local"}"#),
    ];

    for (label, ns_literal) in cases {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

        let ops = format!(r#"search(kind="entity", query="anything", namespace={ns_literal})"#);
        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .unwrap_or_else(|e| panic!("T6e case {label}: dispatch must not MCP-error: {e}"));

        let result_val: serde_json::Value =
            serde_json::from_str(&raw).expect("T6e: response must be valid JSON");
        let first = result_val
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .unwrap_or_else(|| panic!("T6e case {label}: results array must be non-empty"));

        assert_eq!(
            first.get("ok").and_then(serde_json::Value::as_bool),
            Some(false),
            "T6e case {label}: malformed namespace must fail closed; got {first:?}"
        );
        let err_text = first.get("error").map(|e| e.to_string().to_lowercase());
        assert!(
            err_text.as_deref().is_some_and(|e| e.contains("namespace")),
            "T6e case {label}: error must name the namespace; got {first:?}"
        );
        assert!(
                !coord
                    .search_called
                    .load(std::sync::atomic::Ordering::SeqCst),
                "T6e case {label}: coordinator.fan_out_search must NOT be called for a malformed namespace"
            );
    }
}

/// T6f / PR #549 blocker: same as T6e but for `link`'s namespace argument.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6f_multi_backend_link_malformed_namespace_fails_closed() {
    let cases: [(&str, &str); 5] = [
        ("null", "null"),
        ("number", "42"),
        ("boolean", "true"),
        ("array", r#"["local"]"#),
        ("object", r#"{"ns":"local"}"#),
    ];

    for (label, ns_literal) in cases {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

        let src_id = Uuid::new_v4();
        let tgt_id = Uuid::new_v4();
        let ops = format!(
            r#"link(source_id="{src_id}", target_id="{tgt_id}", relation="implements", namespace={ns_literal})"#
        );
        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .unwrap_or_else(|e| panic!("T6f case {label}: dispatch must not MCP-error: {e}"));

        let result_val: serde_json::Value =
            serde_json::from_str(&raw).expect("T6f: response must be valid JSON");
        let first = result_val
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .unwrap_or_else(|| panic!("T6f case {label}: results array must be non-empty"));

        assert_eq!(
            first.get("ok").and_then(serde_json::Value::as_bool),
            Some(false),
            "T6f case {label}: malformed namespace must fail closed; got {first:?}"
        );
        let err_text = first.get("error").map(|e| e.to_string().to_lowercase());
        assert!(
            err_text.as_deref().is_some_and(|e| e.contains("namespace")),
            "T6f case {label}: error must name the namespace; got {first:?}"
        );
        assert!(
            !coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
            "T6f case {label}: coordinator.link must NOT be called for a malformed namespace"
        );
    }
}

/// T6c: a single-backend server must NOT route through the coordinator
/// (zero-change invariant: unchanged from pre-coordinator code).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6c_single_backend_server_bypasses_coordinator() {
    let (registry, runtime) = make_registry();
    let coord = MockCoordinator::single_backend_instance();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    // Create a real entity so the search op succeeds via registry.
    let ns = RuntimeNamespace::local();
    let token = runtime.authorize(ns).expect("authorize");
    let entity = runtime
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "T6cEntity",
            None,
            None,
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .expect("create entity");
    let _ = entity;

    let _result = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"search(kind="entity", query="T6cEntity")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await;

    assert!(
        !coord
            .search_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "T6c: coordinator.fan_out_search must NOT be called for a single-backend server"
    );
    assert!(
        !coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
        "T6c: coordinator.link must NOT be called for a single-backend server"
    );
}

/// T6e: a multi-backend `search` limit beyond `u32::MAX` must be rejected
/// with a per-op error, not silently wrapped by `as u32` (see
/// crates/khive-mcp/docs/api/coordinator.md#t6e-limit for the MCP-AUD-003 regression).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6e_multi_backend_search_limit_matches_single_backend_u32_contract() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let too_large: u64 = u64::from(u32::MAX) + 2;
    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: format!(r#"search(kind="entity", query="anything", limit={too_large})"#),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("T6e: dispatch must not return an MCP-level error");

    let result_val: serde_json::Value =
        serde_json::from_str(&raw).expect("T6e: response must be valid JSON");
    let first = result_val
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .expect("T6e: results array must be non-empty");
    assert_eq!(
        first.get("ok").and_then(serde_json::Value::as_bool),
        Some(false),
        "T6e: an out-of-range limit must produce ok=false; got {:?}",
        first
    );
    assert!(
        !coord
            .search_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "T6e: coordinator must not be called with an out-of-range limit \
             (it must not silently wrap to a small value); recorded last_limit={}",
        coord.last_limit.load(std::sync::atomic::Ordering::SeqCst)
    );
}

/// T6e companion: a valid-but-huge `u32` limit (`u32::MAX`) must still
/// reach the coordinator, capped at 100.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn t6e_multi_backend_search_limit_u32_max_is_capped_at_100() {
    let (registry, _runtime) = make_registry();
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);

    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: format!(
                r#"search(kind="entity", query="anything", limit={})"#,
                u32::MAX
            ),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("T6e: dispatch must not return an MCP-level error");
    let _ = raw;

    assert!(
        coord
            .search_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "T6e: coordinator.fan_out_search must be called for a valid in-range limit"
    );
    assert_eq!(
        coord.last_limit.load(std::sync::atomic::Ordering::SeqCst),
        100,
        "T6e: u32::MAX must be capped at 100 before reaching the coordinator"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_link_uses_canonical_parameter_validation_before_writing() {
    let source = Uuid::new_v4();
    let target = Uuid::new_v4();
    for invalid in [
        r#"weight="0.5""#,
        "weight=true",
        "weight=[]",
        r#"resurrect="false""#,
        "resurrect=1",
        r#"verbose="true""#,
        r#"atomic="false""#,
        "dependency_kind=7",
        "unexpected=42",
    ] {
        for namespace in ["", r#", namespace="local""#] {
            let ops = format!(
                r#"link(source="{source}", target="{target}", kind="implements", {invalid}{namespace})"#
            );
            let (registry, _runtime) = make_registry();
            let coord = MockCoordinator::multi_backend();
            let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
                .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
            let entry = first_result_entry(&server, &ops).await;
            let (registry, _runtime) = make_registry();
            let canonical = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg");
            let expected = first_result_entry(&canonical, &ops).await;
            assert_eq!(expected["ok"], json!(false), "canonical must refuse {ops}");
            assert!(expected["error"].to_string().contains("bad params:"));
            assert_eq!(entry["error"], expected["error"], "parameter parity: {ops}");
            assert!(
                !coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
                "invalid parameters reached the coordinated write: {ops}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_link_keeps_valid_optional_parameters_and_help() {
    let source = Uuid::new_v4();
    let target = Uuid::new_v4();
    for optional in [
        "",
        ", weight=null, resurrect=null, verbose=null, atomic=null",
        ", weight=0.5, resurrect=true, verbose=false, atomic=true",
        r#", namespace="local", metadata={"ordinary":7}, dependency_kind="artifact""#,
    ] {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        let ops = format!(
            r#"link(source_id="{source}", target_id="{target}", relation="implements"{optional})"#
        );
        first_result_entry(&server, &ops).await;
        assert!(
            coord.link_called.load(std::sync::atomic::Ordering::SeqCst),
            "{ops}"
        );
    }

    // Help precedes both admission and parameter validation on the registry path.
    let gate = Arc::new(CapturingGate::denying());
    let (registry, _runtime) = make_registry_with_gate(gate.clone());
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
    let ops = format!(
        r#"link(source_id="{source}", target_id="{target}", relation="implements", weight="invalid", help=true)"#
    );
    let entry = first_result_entry(&server, &ops).await;
    let (registry, _runtime) = make_registry();
    let canonical = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg");
    let expected = first_result_entry(&canonical, &ops).await;
    assert_eq!(
        entry["ok"],
        json!(true),
        "help must not perform the link: {entry}"
    );
    assert_eq!(entry["result"], expected["result"]);
    assert!(!coord.link_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!gate
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| request.verb == "link"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multi_backend_link_admission_still_precedes_parameter_validation() {
    let source = Uuid::new_v4();
    let target = Uuid::new_v4();
    let gate = Arc::new(CapturingGate::denying());
    let (registry, _runtime) = make_registry_with_gate(gate.clone());
    let coord = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
        .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
    let ops = format!(
        r#"link(source_id="{source}", target_id="{target}", relation="implements", weight="invalid")"#
    );
    let entry = first_result_entry(&server, &ops).await;
    assert_eq!(entry["ok"], json!(false));
    assert!(
        entry["error"]
            .to_string()
            .contains("denied by coordinator parity test"),
        "{entry}"
    );
    assert!(
        !entry["error"].to_string().contains("bad params:"),
        "{entry}"
    );
    assert_eq!(
        gate.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.verb == "link")
            .count(),
        1
    );
    assert!(!coord.link_called.load(std::sync::atomic::Ordering::SeqCst));
}

// Signed ADR-130 Am8 r2 transport fixtures. These use the routing mock's
// explicit one-active-note population, not a fake production count fallback.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn note_disclosure_coordinator_single_parallel_chain_and_partial_empty() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "status-fixture")
            .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
        for (selector, count, expected_len) in [("active", 0, 1), ("archived", 1, 0), ("any", 0, 1)] {
            let mut expected_result_bytes = None;
            for ops in [
                json!([{"tool":"search","args":{"kind":"note","query":"x","note_status":selector}}]).to_string(),
                json!([{"tool":"search","args":{"kind":"note","query":"x","note_status":selector}}, {"tool":"stats","args":{}}]).to_string(),
                format!("search(kind=\"note\", query=\"x\", note_status=\"{selector}\") | stats()"),
            ] {
                let response = server.dispatch_request_local(RequestParams {
                    ops, presentation: Some("verbose".into()), format: Some("json".into()),
                    ..Default::default()
                }).await.unwrap();
                let response: Value = serde_json::from_str(&response).unwrap();
                let entry = &response["results"][0];
                assert_eq!(entry["ok"], true, "{entry}");
                assert_eq!(entry["note_status"], selector);
                assert_eq!(entry["note_status_dropped"].as_u64(), Some(count));
                assert_eq!(entry["result"].as_array().unwrap().len(), expected_len);
                let expected_rows = if expected_len == 0 { json!([]) } else {
                    let ranking = khive_pack_kg::handlers::search_rank_fields(
                        Default::default(), khive_runtime::RankScoreKind::Vector,
                        khive_runtime::SearchSignals::default(),
                    );
                    let mut row = json!({
                        "id": Uuid::from_u128(1).to_string(), "kind": "observation",
                        "note_kind": "observation", "name": "note result", "source": "vector",
                        "title": "note result", "snippet": null,
                        "created_at": khive_runtime::micros_to_iso(1_700_000_000_000_000),
                        "updated_at": khive_runtime::micros_to_iso(1_700_000_000_000_000), "version": 1,
                    });
                    row.as_object_mut().unwrap().extend(ranking.as_object().unwrap().clone());
                    json!([row])
                };
                assert_eq!(serde_json::to_vec(&entry["result"]).unwrap(),
                    serde_json::to_vec(&expected_rows).unwrap(), "canonical mock hit array must remain byte-identical");
                let bytes = serde_json::to_vec(&entry["result"]).unwrap();
                if let Some(expected) = &expected_result_bytes { assert_eq!(&bytes, expected); }
                else { expected_result_bytes = Some(bytes); }
                for sibling in response["results"].as_array().unwrap().iter().skip(1) {
                    assert!(sibling.get("note_status").is_none());
                    assert!(sibling.get("note_status_dropped").is_none());
                }
                assert!(response.get("note_status").is_none());
                assert!(response.get("note_status_dropped").is_none());
            }
        }
        let response = server.dispatch_request_local(RequestParams {
            ops: json!([
                {"tool":"search","args":{"kind":"entity","query":"x"}},
                {"tool":"search","args":{"help":true}},
                {"tool":"search","args":{"kind":"note","query":"x","note_status":"invalid"}}
            ]).to_string(), presentation: Some("verbose".into()), format: Some("json".into()),
            ..Default::default()
        }).await.unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        for entry in response["results"].as_array().unwrap() {
            assert!(entry.get("note_status").is_none());
            assert!(entry.get("note_status_dropped").is_none());
        }
        assert_eq!(response["results"][2]["ok"], false);
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::degraded_multi_backend("missing-status-leg");
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "status-fixture")
            .with_coordinator(coord as Arc<dyn CoordinatorService>);
        for selector in ["active", "archived"] {
            let response = server.dispatch_request_local(RequestParams {
                ops: json!([{"tool":"search","args":{"kind":"note","query":"x","note_status":selector}}]).to_string(),
                presentation: Some("verbose".into()), format: Some("json".into()), ..Default::default()
            }).await.unwrap();
            let response: Value = serde_json::from_str(&response).unwrap();
            let entry = &response["results"][0];
            if selector == "active" {
                assert_eq!(entry["ok"], true);
                assert_eq!(entry["status"], "partial");
                assert_eq!(entry["note_status"], "active");
                assert_eq!(entry["note_status_dropped"].as_u64(), Some(0));
            } else {
                assert_eq!(entry["ok"], false);
                assert_eq!(entry["error"]["kind"], "search_incomplete");
                assert!(entry.get("note_status").is_none());
                assert!(entry.get("note_status_dropped").is_none());
                assert!(entry.pointer("/error/search/note_status").is_none());
                assert!(entry.pointer("/error/search/note_status_dropped").is_none());
            }
        }
    }).await.expect("coordinator note transport fixture exceeded async budget");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinator_note_disclosure_all_presentations_format_result_only() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (registry, _runtime) = make_registry();
        let coord = MockCoordinator::multi_backend();
        let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "status-fixture")
            .with_coordinator(coord as Arc<dyn CoordinatorService>);
        for (selector, count) in [("active", 0_u64), ("archived", 1), ("any", 0)] {
            let ops = json!([{"tool":"search","args":{"kind":"note","query":"x","note_status":selector}}]).to_string();
            for (name, presentation) in [
                ("agent", khive_runtime::PresentationMode::Agent),
                ("human", khive_runtime::PresentationMode::Human),
                ("verbose", khive_runtime::PresentationMode::Verbose),
            ] {
                let json_response = server.dispatch_request_local(RequestParams {
                    ops: ops.clone(), presentation: Some(name.into()), format: Some("json".into()),
                    ..Default::default()
                }).await.unwrap();
                let json_response: Value = serde_json::from_str(&json_response).unwrap();
                let json_entry = &json_response["results"][0];
                for (format_name, format) in [
                    ("json", khive_runtime::OutputFormat::Json),
                    ("auto", khive_runtime::OutputFormat::Auto),
                    ("table", khive_runtime::OutputFormat::Table),
                ] {
                    let response = server.dispatch_request_local(RequestParams {
                        ops: ops.clone(), presentation: Some(name.into()), format: Some(format_name.into()),
                        ..Default::default()
                    }).await.unwrap();
                    let response: Value = serde_json::from_str(&response).unwrap();
                    let entry = &response["results"][0];
                    assert_eq!(entry["ok"], true);
                    assert_eq!(entry["note_status"], selector);
                    assert_eq!(entry["note_status_dropped"].as_u64(), Some(count));
                    let mut expected = json_entry.clone();
                    if format != khive_runtime::OutputFormat::Json {
                        expected["result"] = Value::String(khive_runtime::presentation::render_format_with_note_content(
                            json_entry["result"].clone(), format, presentation,
                            khive_runtime::presentation::NoteContentScope::None,
                        ));
                        assert!(!entry["result"].as_str().unwrap().contains("note_status"));
                    }
                    let mut actual = entry.clone();
                    // Execution counters are measured per request, independently of presentation.
                    expected.as_object_mut().unwrap().remove("usage");
                    actual.as_object_mut().unwrap().remove("usage");
                    assert_eq!(actual, expected, "{name}/{format_name}");
                }
            }
        }
    }).await.expect("coordinator presentation fixture exceeded async budget");
}
