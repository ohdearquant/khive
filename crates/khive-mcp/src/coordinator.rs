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
    /// Kind string for each entity hit, keyed by entity UUID.
    /// Populated by the coordinator after the RRF merge. Missing entries mean
    /// the kind could not be resolved (e.g. the owning backend errored).
    pub entity_kinds: std::collections::HashMap<uuid::Uuid, String>,
    /// Kind string for each note hit, keyed by note UUID.
    /// Populated by the coordinator after the RRF merge.
    pub note_kinds: std::collections::HashMap<uuid::Uuid, String>,
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
    /// `kind_filter` is the granular kind to pass as a storage-level filter
    /// (`entity_kind` for entity substrate, `note_kind` for note substrate).
    /// Pass `None` for substrate-level (`kind="entity"` or `kind="note"`) searches.
    ///
    /// `props_filter` and `tags` are entity-substrate filters forwarded to each
    /// backend's `hybrid_search`. When either is active the per-backend candidate
    /// window is widened so that sparse matches ranked below the bare `limit` are
    /// not cut off before filtering (mirrors the single-backend handler).
    /// Both are ignored for note-substrate searches.
    ///
    /// Granular kinds that cannot be resolved to a substrate fall through to the
    /// registry (single-backend path); the coordinator does not silently drop results.
    #[allow(clippy::too_many_arguments)]
    async fn fan_out_search(
        &self,
        kind: &str,
        query: &str,
        namespace: &Namespace,
        limit: u32,
        kind_filter: Option<&str>,
        props_filter: Option<&serde_json::Value>,
        tags: &[String],
    ) -> CoordSearchResult;

    /// True when only one backend is registered (zero-change invariant check).
    fn is_single_backend(&self) -> bool;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use khive_runtime::{NoteSearchHit, SearchHit, SearchSource};
    use std::sync::Arc;

    /// Minimal mock for server-routing tests (T6 in the test plan).
    pub struct MockCoordinator {
        pub link_called: std::sync::atomic::AtomicBool,
        pub search_called: std::sync::atomic::AtomicBool,
        pub single_backend: bool,
        /// The `limit` value `fan_out_search` was last called with (MCP-AUD-003).
        pub last_limit: std::sync::atomic::AtomicU32,
    }

    impl MockCoordinator {
        pub fn multi_backend() -> Arc<Self> {
            Arc::new(Self {
                link_called: std::sync::atomic::AtomicBool::new(false),
                search_called: std::sync::atomic::AtomicBool::new(false),
                single_backend: false,
                last_limit: std::sync::atomic::AtomicU32::new(0),
            })
        }

        pub fn single_backend_instance() -> Arc<Self> {
            Arc::new(Self {
                link_called: std::sync::atomic::AtomicBool::new(false),
                search_called: std::sync::atomic::AtomicBool::new(false),
                single_backend: true,
                last_limit: std::sync::atomic::AtomicU32::new(0),
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
            kind: &str,
            _query: &str,
            _namespace: &Namespace,
            limit: u32,
            _kind_filter: Option<&str>,
            _props_filter: Option<&serde_json::Value>,
            _tags: &[String],
        ) -> CoordSearchResult {
            self.search_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.last_limit
                .store(limit, std::sync::atomic::Ordering::SeqCst);
            let id = Uuid::from_u128(1);
            let is_note = kind == "note";
            CoordSearchResult {
                entity_hits: if is_note {
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
                note_hits: if is_note {
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
                per_backend: vec![],
                partial: false,
                entity_kinds: std::collections::HashMap::from([(id, "concept".to_string())]),
                note_kinds: std::collections::HashMap::from([(id, "observation".to_string())]),
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

    /// T6a: a multi-backend server MUST route `link` through the coordinator.
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
            let hit = &response["results"][0]["result"][0];

            assert_eq!(
                hit.get("source").and_then(serde_json::Value::as_str),
                Some(expected_source),
                "{kind} hit must expose its retrieval source; got: {hit}"
            );
        }
    }

    /// T6d: a multi-backend search with a malformed `tags` value must return a
    /// per-op error rather than silently returning unfiltered results (see
    /// crates/khive-mcp/docs/api/coordinator.md#t6d for the regression this guards).
    #[tokio::test]
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
