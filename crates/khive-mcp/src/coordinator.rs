//! CoordinatorService trait seam — dependency inversion for ADR-029 Phase 2.
//!
//! `khive-mcp` defines the contract; `kkernel` provides the concrete implementation.
//! This avoids a crate-cycle: kkernel depends on khive-mcp, so khive-mcp cannot
//! depend on kkernel. The trait is the stable boundary.

use std::fmt;

use async_trait::async_trait;
use uuid::Uuid;

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
    /// `kind` controls which substrate to search:
    /// - `"entity"` or any granular entity kind → entity fan-out via `hybrid_search`
    /// - `"note"` or any granular note kind → note fan-out via `search_notes`
    ///
    /// Granular kinds that cannot be resolved to a substrate fall through to the
    /// registry (single-backend path); the coordinator does not silently drop results.
    async fn fan_out_search(
        &self,
        kind: &str,
        query: &str,
        namespace: &Namespace,
        limit: u32,
    ) -> CoordSearchResult;

    /// True when only one backend is registered (zero-change invariant check).
    fn is_single_backend(&self) -> bool;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal mock for server-routing tests (T6 in the test plan).
    pub struct MockCoordinator {
        pub link_called: std::sync::atomic::AtomicBool,
        pub search_called: std::sync::atomic::AtomicBool,
        pub single_backend: bool,
    }

    impl MockCoordinator {
        pub fn multi_backend() -> Arc<Self> {
            Arc::new(Self {
                link_called: std::sync::atomic::AtomicBool::new(false),
                search_called: std::sync::atomic::AtomicBool::new(false),
                single_backend: false,
            })
        }

        pub fn single_backend_instance() -> Arc<Self> {
            Arc::new(Self {
                link_called: std::sync::atomic::AtomicBool::new(false),
                search_called: std::sync::atomic::AtomicBool::new(false),
                single_backend: true,
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
            _kind: &str,
            _query: &str,
            _namespace: &Namespace,
            _limit: u32,
        ) -> CoordSearchResult {
            self.search_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            CoordSearchResult {
                entity_hits: vec![],
                note_hits: vec![],
                per_backend: vec![],
                partial: false,
            }
        }

        fn is_single_backend(&self) -> bool {
            self.single_backend
        }
    }

    // ── T6: server-level coordinator routing ─────────────────────────────────

    use crate::server::KhiveMcpServer;
    use crate::tools::request::RequestParams;
    use khive_runtime::{KhiveRuntime, Namespace as RuntimeNamespace, RuntimeConfig};

    fn make_registry() -> (khive_runtime::VerbRegistry, khive_runtime::KhiveRuntime) {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: RuntimeNamespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let gate = runtime.config().gate.clone();
        let default_ns = runtime.config().default_namespace.clone();
        let actor_id = runtime.config().actor_id.clone();
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.with_gate(gate);
        builder.with_default_namespace(default_ns.as_str());
        builder.with_actor_id(actor_id);
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

    /// T6a: A multi-backend server MUST route `link` through the coordinator.
    ///
    /// This test must FAIL before BLOCKER-1 is wired (coordinator never called)
    /// and PASS after wiring.
    #[tokio::test]
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
            })
            .await;

        assert!(
            coord
                .link_called
                .load(std::sync::atomic::Ordering::SeqCst),
            "T6a: coordinator.link must be called when a link op is dispatched through a multi-backend server"
        );
    }

    /// T6b: A multi-backend server MUST route `search` through the coordinator.
    ///
    /// This test must FAIL before BLOCKER-1 is wired and PASS after wiring.
    #[tokio::test]
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
            })
            .await;

        assert!(
            coord
                .search_called
                .load(std::sync::atomic::Ordering::SeqCst),
            "T6b: coordinator.fan_out_search must be called when a search op is dispatched through a multi-backend server"
        );
    }

    /// T6c: A single-backend server must NOT route through the coordinator.
    ///
    /// When the coordinator reports `is_single_backend() == true`, the zero-change
    /// invariant requires the registry path (unchanged from pre-coordinator code).
    #[tokio::test]
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
}
