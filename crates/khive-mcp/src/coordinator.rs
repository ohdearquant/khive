//! CoordinatorService trait seam — dependency inversion for ADR-029 Phase 2.
//!
//! `khive-mcp` defines the contract; `kkernel` provides the concrete implementation.
//! This avoids a crate-cycle: kkernel depends on khive-mcp, so khive-mcp cannot
//! depend on kkernel. The trait is the stable boundary.

use std::fmt;

use async_trait::async_trait;
use uuid::Uuid;

use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::Namespace;
use khive_runtime::{BackendId, NoteSearchHit, SearchHit};
use khive_storage::{Edge, EdgeRelation};

/// Result of a cross-backend link operation.
pub struct CoordLinkResult {
    /// The edge that was written (on the source backend).
    pub edge: Edge,
    /// True when source and target are on different backends.
    pub cross_backend: bool,
    /// The target backend id when `cross_backend` is true.
    pub target_backend_id: Option<BackendId>,
}

/// Error variants the coordinator can produce.
pub enum CoordError {
    /// The given UUID was not found on any registered backend.
    UnknownNode { id: Uuid },
    /// The proposed edge violates ADR-002 endpoint rules.
    EdgeRuleViolation(String),
    /// A backend operation failed.
    Backend(String),
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoordError::UnknownNode { id } => write!(f, "node {id} not found on any backend"),
            CoordError::EdgeRuleViolation(msg) => write!(f, "edge rule violation: {msg}"),
            CoordError::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl fmt::Debug for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl From<CoordError> for khive_runtime::RuntimeError {
    fn from(e: CoordError) -> Self {
        match e {
            CoordError::UnknownNode { id } => {
                khive_runtime::RuntimeError::NotFound(format!("node {id} not found on any backend"))
            }
            CoordError::EdgeRuleViolation(msg) => khive_runtime::RuntimeError::InvalidInput(msg),
            CoordError::Backend(msg) => khive_runtime::RuntimeError::Internal(msg),
        }
    }
}

/// Per-backend contribution to a fan-out search.
pub struct BackendSearchResult {
    pub backend_id: BackendId,
    pub entity_hits: Vec<SearchHit>,
    pub note_hits: Vec<NoteSearchHit>,
    /// Populated when this backend errored during the fan-out.
    pub error: Option<String>,
}

/// Merged fan-out search result.
pub struct CoordSearchResult {
    /// RRF-merged entity hits across all backends.
    pub entity_hits: Vec<SearchHit>,
    /// RRF-merged note hits across all backends.
    pub note_hits: Vec<NoteSearchHit>,
    /// Per-backend detail (for diagnostics).
    pub per_backend: Vec<BackendSearchResult>,
    /// True when at least one backend errored (results may be incomplete).
    pub partial: bool,
    /// Kind string for each entity hit, keyed by entity UUID.
    /// Populated by the coordinator after the RRF merge. Missing entries mean
    /// the kind could not be resolved (e.g. the owning backend errored).
    pub entity_kinds: std::collections::HashMap<uuid::Uuid, String>,
    /// Kind string for each note hit, keyed by note UUID.
    /// Populated by the coordinator after the RRF merge.
    pub note_kinds: std::collections::HashMap<uuid::Uuid, String>,
    /// `created_at` (micros) for each entity hit, keyed by entity UUID —
    /// row-shape parity with the KG pack's single-backend search serializer
    /// (`crates/khive-pack-kg/src/handlers/search.rs`). Populated alongside
    /// `entity_kinds`; missing entries follow the same resolution rule.
    pub entity_created_at: std::collections::HashMap<uuid::Uuid, i64>,
    /// `created_at` (micros) for each note hit, keyed by note UUID. Same
    /// parity purpose and resolution rule as `entity_created_at`.
    pub note_created_at: std::collections::HashMap<uuid::Uuid, i64>,
    /// Stored `name` for each note hit, keyed by note UUID — distinct from
    /// `title` (the search-hit display title). Same parity purpose and
    /// resolution rule as `entity_created_at`.
    pub note_names: std::collections::HashMap<uuid::Uuid, Option<String>>,
}

/// Cross-backend coordinator seam visible to `khive-mcp`.
///
/// Implemented by `kkernel::coordinator::SubstrateCoordinatorService`.
/// `khive-mcp` holds an `Option<Arc<dyn CoordinatorService>>` and calls through
/// when in multi-backend mode; single-backend servers hold `None` and dispatch
/// through the `VerbRegistry` unchanged (zero-change invariant).
#[async_trait]
pub trait CoordinatorService: Send + Sync {
    /// Resolve the owning backend for a UUID.
    ///
    /// Namespace-agnostic per ADR-007 Rev 3: presence of the record in a backend
    /// is sufficient — the record's stored namespace is not compared to the caller.
    async fn locate(&self, id: Uuid) -> Option<BackendId>;

    /// Prewarm the locator cache after a successful create so the first
    /// `locate()` for the new record is a cache hit rather than a backend scan.
    fn record_created(&self, id: Uuid, backend_id: BackendId);

    /// The primary backend id (used to prewarm after create).
    fn primary_backend_id(&self) -> Option<BackendId>;

    /// Cross-backend link (D3). Locates both endpoints, validates the relation,
    /// and writes the edge on the source backend with `target_backend` stamped
    /// when the endpoints are on different backends.
    async fn link(
        &self,
        namespace: &Namespace,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
    ) -> Result<CoordLinkResult, CoordError>;

    /// Fan-out search across all registered backends (D4).
    ///
    /// `request` is the KG handler's canonical validated search contract. It
    /// carries the resolved substrate plus every supported filter, so this
    /// boundary cannot silently narrow the public wire shape.
    ///
    /// `extra_visible` is the caller's resolved extra read-visibility
    /// namespaces beyond `namespace` itself (MAJ-3 fix) — the same
    /// `['local'] ∪ visible_namespaces` set the normal registry dispatch path
    /// authorizes with, or empty when the caller named an explicit
    /// `namespace=` (which intentionally narrows visibility). Implementations
    /// must authorize each backend's search token with this widened set
    /// rather than `namespace` alone, or a namespace visible only through
    /// `visible_namespaces` silently drops out of coordinator search results.
    async fn fan_out_search(
        &self,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
        extra_visible: &[Namespace],
    ) -> CoordSearchResult;

    /// True when only one backend is registered (zero-change invariant check).
    fn is_single_backend(&self) -> bool;
}

#[cfg(test)]
pub(crate) mod tests {
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
        /// When `true`, `fan_out_search` returns zero hits regardless of
        /// substrate — used to construct the "complete-empty" (healthy, no
        /// match) and "degraded-empty" (backend failed, no survivor)
        /// envelope fixtures (ADR-130 §1 regression coverage).
        pub empty_hits: bool,
        pub last_search_request: std::sync::Mutex<Option<ValidatedSearchRequest>>,
        /// The `limit` value `fan_out_search` was last called with (MCP-AUD-003).
        pub last_limit: std::sync::atomic::AtomicU32,
        /// The `extra_visible` slice `fan_out_search` was last called with
        /// (MAJ-3 visibility-scope regression coverage).
        pub last_extra_visible: std::sync::Mutex<Vec<Namespace>>,
    }

    impl MockCoordinator {
        pub fn multi_backend() -> Arc<Self> {
            Arc::new(Self {
                link_called: std::sync::atomic::AtomicBool::new(false),
                search_called: std::sync::atomic::AtomicBool::new(false),
                single_backend: false,
                failed_backend: None,
                empty_hits: false,
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
                empty_hits: true,
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
                failed_backend: Some(BackendId::new(failed_backend)),
                empty_hits: false,
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
                failed_backend: Some(BackendId::new(failed_backend)),
                empty_hits: true,
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
                empty_hits: false,
                last_search_request: std::sync::Mutex::new(None),
                last_limit: std::sync::atomic::AtomicU32::new(0),
                last_extra_visible: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl CoordinatorService for MockCoordinator {
        async fn locate(&self, _id: Uuid) -> Option<BackendId> {
            Some(BackendId::main())
        }

        fn record_created(&self, _id: Uuid, _backend_id: BackendId) {}

        fn primary_backend_id(&self) -> Option<BackendId> {
            Some(BackendId::main())
        }

        async fn link(
            &self,
            _namespace: &Namespace,
            _source_id: Uuid,
            _target_id: Uuid,
            _relation: EdgeRelation,
            _weight: f64,
            _metadata: Option<serde_json::Value>,
        ) -> Result<CoordLinkResult, CoordError> {
            self.link_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
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
                        source: SearchSource::Both,
                        title: Some("entity result".to_string()),
                        snippet: None,
                    }]
                },
                note_hits: if is_note && !self.empty_hits {
                    vec![NoteSearchHit {
                        note_id: id,
                        score: Default::default(),
                        source: SearchSource::Vector,
                        title: Some("note result".to_string()),
                        snippet: None,
                    }]
                } else {
                    vec![]
                },
                per_backend: self
                    .failed_backend
                    .iter()
                    .cloned()
                    .map(|backend_id| BackendSearchResult {
                        backend_id,
                        entity_hits: vec![],
                        note_hits: vec![],
                        error: Some("injected search failure".to_string()),
                    })
                    .collect(),
                partial: self.failed_backend.is_some(),
                entity_kinds: std::collections::HashMap::from([(id, "concept".to_string())]),
                note_kinds: std::collections::HashMap::from([(id, "observation".to_string())]),
                entity_created_at: std::collections::HashMap::from([(id, 1_700_000_000_000_000)]),
                note_created_at: std::collections::HashMap::from([(id, 1_700_000_000_000_000)]),
                note_names: std::collections::HashMap::from([(
                    id,
                    Some("note result".to_string()),
                )]),
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
        let token = runtime
            .authorize(RuntimeNamespace::local())
            .expect("authorize event store");
        let event_store = runtime.events(&token).expect("in-memory event store");
        builder.with_event_store(event_store);
        khive_runtime::PackRegistry::register_packs(
            &["kg".to_string()],
            runtime.clone(),
            &mut builder,
        )
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
            (r#"search(kind="note", query="x")"#, None),
            (r#"[search(kind="entity", query="x"), stats()]"#, None),
            (r#"search(kind="entity", query="x") | stats()"#, None),
            (r#"search(kind="entity", query="x")"#, Some("human")),
        ];

        for (ops, presentation) in cases {
            let (registry, _runtime) = make_registry();
            let coord = MockCoordinator::degraded_multi_backend("archive");
            let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "test-cfg")
                .with_coordinator(Arc::clone(&coord) as Arc<dyn CoordinatorService>);
            let raw = server
                .dispatch_request_local(RequestParams {
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
        assert!(search.get("partial").is_none());
        assert!(search.get("missing_backends").is_none());
        assert!(search.get("backend_errors").is_none());
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
            r#"search(kind="entity", query="gate parity", limit=7, namespace="tenant-a")"#
                .to_string(),
        ];

        for ops in operations {
            for server in [&direct_server, &coordinator_server] {
                server
                    .dispatch_request_local(RequestParams {
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
            .create_entity(&token, "concept", None, "T6cEntity", None, None, vec![])
            .await
            .expect("create entity");
        let _ = entity;

        let _result = server
            .dispatch_request_local(RequestParams {
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
}
