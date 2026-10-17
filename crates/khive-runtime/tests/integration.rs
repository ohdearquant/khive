//! Integration tests for khive-runtime.
//!
//! Tests cover entity CRUD, graph operations, note memory, GQL query,
//! and namespace isolation using an in-memory runtime.

#[path = "../src/test_process.rs"]
mod test_process;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::blob::ContentRef;
use khive_storage::types::{Direction, PageRequest, TraversalOptions, TraversalRequest};
use khive_storage::{BlobStore, EdgeRelation, Event, EventFilter, NewAttachment};
use khive_types::{EventKind, SubstrateKind};
use std::collections::BTreeSet;
use uuid::Uuid;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("in-memory runtime")
}

include!("integration_tests/event_observation_tests.rs");

include!("integration_tests/entity_crud_tests.rs");

include!("integration_tests/graph_traversal_tests.rs");

include!("integration_tests/note_creation_tests.rs");

include!("integration_tests/gql_value_tests.rs");

include!("integration_tests/gql_pagination_tests.rs");

include!("integration_tests/gql_static_pair_tests.rs");

include!("integration_tests/gql_type_and_warning_tests.rs");

// =============================================================================
// Namespace isolation
// =============================================================================

#[tokio::test]
async fn namespace_isolation() {
    let rt = rt();
    let ns_a_tok = rt.authorize(Namespace::parse("ns-a").unwrap()).unwrap();
    let ns_b_tok = rt.authorize(Namespace::parse("ns-b").unwrap()).unwrap();

    rt.create_entity_with_embedding_report(
        &ns_a_tok,
        "concept",
        None,
        "EntityA",
        None,
        None,
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();
    rt.create_entity_with_embedding_report(
        &ns_b_tok,
        "concept",
        None,
        "EntityB",
        None,
        None,
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let a_entities = rt
        .list_entities(&ns_a_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(a_entities.len(), 1);
    assert_eq!(a_entities[0].name, "EntityA");

    let b_entities = rt
        .list_entities(&ns_b_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(b_entities.len(), 1);
    assert_eq!(b_entities[0].name, "EntityB");
}

// =============================================================================
// Hybrid search indexing
// =============================================================================

#[tokio::test]
async fn create_entity_indexes_into_text_search() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "FlashAttention",
            Some("efficient attention mechanism"),
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let hits = rt
        .hybrid_search(&tok, "FlashAttention", None, 10, None, None, &[], None)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.entity_id == entity.id),
        "newly created entity should be findable via hybrid_search (text path)"
    );
}

#[tokio::test]
async fn create_entity_no_embedding_model_does_not_propagate_vector_error() {
    // KhiveRuntime::memory() has embedding_model: None — vector indexing is silently skipped.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let result = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "SilentVectorSkip",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record);
    assert!(
        result.is_ok(),
        "create_entity must not propagate Unconfigured from vector store"
    );
}

// =============================================================================
// Soft-delete visibility
// =============================================================================

/// Soft-deleted entities must not appear in hybrid_search results.
#[tokio::test]
async fn hybrid_search_excludes_soft_deleted_entities() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "SoftDeleteMe",
            Some("entity that will be soft-deleted"),
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // Confirm the entity is visible before deletion.
    let hits_before = rt
        .hybrid_search(&tok, "SoftDeleteMe", None, 10, None, None, &[], None)
        .await
        .unwrap();
    assert!(
        hits_before.iter().any(|h| h.entity_id == entity.id),
        "entity should appear in hybrid_search before soft-delete"
    );

    rt.delete_entity(&tok, entity.id, false).await.unwrap(); // soft delete

    let hits_after = rt
        .hybrid_search(&tok, "SoftDeleteMe", None, 10, None, None, &[], None)
        .await
        .unwrap();
    assert!(
        !hits_after.iter().any(|h| h.entity_id == entity.id),
        "soft-deleted entity must not appear in hybrid_search"
    );
}

/// Hard-deleted entities are gone from storage entirely and never appear in hybrid_search.
#[tokio::test]
async fn hybrid_search_excludes_hard_deleted_entities() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "HardDeleteMe",
            Some("entity that will be hard-deleted"),
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    let hits_before = rt
        .hybrid_search(&tok, "HardDeleteMe", None, 10, None, None, &[], None)
        .await
        .unwrap();
    assert!(
        hits_before.iter().any(|h| h.entity_id == entity.id),
        "entity should appear in hybrid_search before hard-delete"
    );

    rt.delete_entity(&tok, entity.id, true).await.unwrap(); // hard delete

    // Hard-deleted rows are gone from the entity store; the FTS/vector indexes may still
    // have stale entries. The soft-delete filter sees no alive entity and drops the hit.
    let hits_after = rt
        .hybrid_search(&tok, "HardDeleteMe", None, 10, None, None, &[], None)
        .await
        .unwrap();
    assert!(
        !hits_after.iter().any(|h| h.entity_id == entity.id),
        "hard-deleted entity must not appear in hybrid_search"
    );
}

include!("integration_tests/note_deletion_and_persistence_tests.rs");

include!("integration_tests/edge_observation_and_conflict_tests.rs");

include!("integration_tests/secret_admission_tests.rs");

// =============================================================================
// EmbedderRegistry integration tests
// =============================================================================

mod embedder_registry_tests {
    use async_trait::async_trait;
    use khive_gate::AllowAllGate;
    use khive_runtime::{
        EmbedderProvider, KhiveRuntime, NamespaceToken, PackRuntime, RequestIdentity,
        RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
    };
    use khive_types::{HandlerDef, Namespace, Pack, VerbCategory, Visibility};
    use lattice_embed::{EmbeddingModel, EmbeddingService};
    use serde_json::Value;
    use std::sync::Arc;

    // ── MockEmbedderProvider ─────────────────────────────────────────────────

    /// A synthetic embedding provider that returns a fixed vector of `42.0` values.
    ///
    /// Used to verify that custom providers are reachable via
    /// `KhiveRuntime::embedder` after registration.
    struct MockEmbedderProvider {
        name: String,
        dims: usize,
    }

    impl MockEmbedderProvider {
        fn new(name: &str, dims: usize) -> Self {
            Self {
                name: name.to_owned(),
                dims,
            }
        }
    }

    struct MockEmbeddingService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for MockEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|_| vec![42.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "mock-embedding-service"
        }
    }

    #[async_trait]
    impl EmbedderProvider for MockEmbedderProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Ok(Arc::new(MockEmbeddingService { dims: self.dims }))
        }
    }

    struct EmbedderInitPack {
        runtime: KhiveRuntime,
    }

    impl Pack for EmbedderInitPack {
        const NAME: &'static str = "embedder-init-test";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &["concept"];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "initialize_embedder",
            description: "initialize the test embedder",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for EmbedderInitPack {
        fn name(&self) -> &str {
            Self::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }

        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.runtime
                .create_entity_with_embedding_report(
                    token,
                    "concept",
                    None,
                    "embedder initialization trigger",
                    None,
                    None,
                    vec![],
                )
                .await
                .map(|(record, _report)| record)?;
            Ok(Value::Null)
        }
    }

    fn memory_rt_no_model() -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime")
    }

    // ── Test: register + embedder round-trip ─────────────────────────────────

    #[tokio::test]
    async fn register_embedder_and_retrieve_via_embedder_method() {
        let rt = memory_rt_no_model();
        rt.register_embedder(MockEmbedderProvider::new("mock", 384));

        let service = rt
            .embedder("mock")
            .await
            .expect("embedder lookup must succeed after registration");

        let texts = vec!["hello world".to_string()];
        let vecs = service
            .embed(&texts, EmbeddingModel::AllMiniLmL6V2)
            .await
            .expect("mock service must embed successfully");

        assert_eq!(vecs.len(), 1);
        assert_eq!(vecs[0].len(), 384);
        assert!(
            vecs[0].iter().all(|&v| (v - 42.0_f32).abs() < 1e-6),
            "mock service must return constant 42.0 vector"
        );
    }

    // ── Test: registered names include custom provider ────────────────────────

    #[tokio::test]
    async fn embedder_initialization_writes_event() {
        let rt = memory_rt_no_model();
        let token = rt
            .authorize(Namespace::local())
            .expect("authorize local namespace");
        let event_store = rt.events(&token).expect("event store must be available");
        rt.register_embedder(MockEmbedderProvider::new("event-test-encoder", 64));

        rt.embedder("event-test-encoder")
            .await
            .expect("embedder initialization must succeed");

        let page = event_store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("query embedder initialization event");
        let event = page
            .items
            .iter()
            .find(|event| event.verb == "embedder.init")
            .expect("embedder initialization event must be written");

        assert_eq!(event.kind, khive_types::EventKind::EmbedderInitialized);
        assert_eq!(event.payload["model_name"], "event-test-encoder");
        let duration_us = event.payload["duration_us"]
            .as_i64()
            .expect("duration_us must be an integer");
        assert!(duration_us > 0);
        assert_eq!(event.duration_us, duration_us);
    }

    #[tokio::test]
    async fn embedder_initialization_uses_triggering_request_identity() {
        let rt = memory_rt_no_model();
        rt.register_embedder(MockEmbedderProvider::new("request-identity-encoder", 64));
        let tenant = Namespace::parse("tenant-request").expect("valid tenant namespace");
        let event_store = rt
            .events(&rt.authorize(tenant.clone()).expect("authorize tenant"))
            .expect("event store must be available");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(EmbedderInitPack {
            runtime: rt.clone(),
        });
        builder.with_default_namespace("baked-namespace");
        builder.with_actor_id(Some("baked-actor".to_string()));
        let registry = builder.build().expect("registry builds");

        registry
            .dispatch_with_identity(
                "initialize_embedder",
                serde_json::json!({"namespace": tenant.as_str()}),
                Some(RequestIdentity {
                    namespace: tenant.as_str().to_string(),
                    actor_id: Some("request-actor".to_string()),
                    visible_namespaces: vec![],
                    process_ref: None,
                    request_id: None,
                }),
            )
            .await
            .expect("request-triggered embedder initialization must succeed");

        let page = event_store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("query embedder initialization event");
        let event = page
            .items
            .iter()
            .find(|event| event.verb == "embedder.init")
            .expect("embedder initialization event must be written");

        assert_eq!(event.namespace, tenant.as_str());
        assert_eq!(event.actor, "actor:request-actor");
    }

    #[tokio::test]
    async fn registered_names_includes_custom_provider() {
        let rt = memory_rt_no_model();
        rt.register_embedder(MockEmbedderProvider::new("my-encoder", 128));

        let names = rt.registered_embedding_model_names();
        assert!(
            names.contains(&"my-encoder".to_string()),
            "registered_embedding_model_names must include custom provider 'my-encoder'; got {names:?}"
        );
    }

    // ── Test: dual-embedding regression — both MiniLM and paraphrase reachable ─

    #[tokio::test]
    async fn dual_embedding_regression_both_models_registered() {
        use khive_runtime::RuntimeConfig;
        let rt = KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("runtime with two models");

        let names = rt.registered_embedding_model_names();

        assert!(
            names.contains(&"all-minilm-l6-v2".to_string()),
            "MiniLM must be registered; names: {names:?}"
        );
        assert!(
            names.contains(&"paraphrase-multilingual-minilm-l12-v2".to_string()),
            "paraphrase must be registered; names: {names:?}"
        );

        // Verify resolve_embedding_model works for both.
        rt.resolve_embedding_model(Some("all-minilm-l6-v2"))
            .expect("MiniLM must resolve");
        rt.resolve_embedding_model(Some("paraphrase"))
            .expect("paraphrase alias must resolve");
    }

    // ── Test: unknown embedder returns UnknownModel ───────────────────────────

    #[tokio::test]
    async fn embedder_unknown_name_returns_error() {
        let rt = memory_rt_no_model();
        let err = rt
            .embedder("no-such-model")
            .await
            .err()
            .expect("expected Err for unknown embedder name, got Ok");
        assert!(
            matches!(err, RuntimeError::UnknownModel(ref n) if n == "no-such-model"),
            "expected UnknownModel for unregistered name; got {err:?}"
        );
    }

    // ── Test: custom provider registered via pack hook is reachable end-to-end ─
    //
    // This is the integration counterpart to the unit tests in
    // `embedder_registry.rs`. It verifies the full stack: a pack overrides
    // `register_embedders`, the transport calls `VerbRegistry::call_register_embedders`,
    // and the custom provider can be resolved and used via `rt.embedder(name)`.

    #[tokio::test]
    async fn pack_register_embedders_hook_makes_provider_reachable() {
        use async_trait::async_trait;
        use khive_runtime::pack::HandlerDef;
        use khive_runtime::NamespaceToken;
        use khive_runtime::{PackRuntime, VerbRegistry, VerbRegistryBuilder};
        use khive_types::Pack;
        use serde_json::Value;

        struct EmbedderPack;

        impl Pack for EmbedderPack {
            const NAME: &'static str = "embedder-test-pack";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[];
        }

        #[async_trait]
        impl PackRuntime for EmbedderPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            fn register_embedders(&self, runtime: &KhiveRuntime) {
                runtime.register_embedder(MockEmbedderProvider::new("pack-custom-encoder", 256));
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, khive_runtime::RuntimeError> {
                Ok(Value::Null)
            }
        }

        let rt = memory_rt_no_model();
        // Simulate what the transport does: build the registry, then call the hook.
        let mut builder = VerbRegistryBuilder::new();
        builder.register(EmbedderPack);
        let registry = builder.build().expect("registry builds");
        registry.call_register_embedders(&rt);

        // After the hook fires, the custom provider must be reachable.
        let service = rt
            .embedder("pack-custom-encoder")
            .await
            .expect("pack-contributed provider must be reachable after call_register_embedders");

        let texts = vec!["test sentence".to_string()];
        let vecs = service
            .embed(&texts, EmbeddingModel::AllMiniLmL6V2)
            .await
            .expect("custom service must embed without error");
        assert_eq!(vecs.len(), 1);
        assert_eq!(
            vecs[0].len(),
            256,
            "dims must match provider declaration (256)"
        );
    }

    // ── Test: failing provider build() returns Err instead of panicking ───────

    #[tokio::test]
    async fn failing_provider_build_returns_err_not_panic() {
        struct FailingProvider;

        #[async_trait]
        impl EmbedderProvider for FailingProvider {
            fn name(&self) -> &str {
                "failing-provider"
            }
            fn dimensions(&self) -> usize {
                128
            }
            async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
                Err(RuntimeError::Internal(
                    "simulated provider construction failure".into(),
                ))
            }
        }

        let rt = memory_rt_no_model();
        rt.register_embedder(FailingProvider);

        let result = rt.embedder("failing-provider").await;
        assert!(
            result.is_err(),
            "embedder() must return Err when build() fails, not panic; got Ok"
        );
        let err = result.err().expect("checked above");
        let msg = err.to_string();
        assert!(
            msg.contains("simulated provider construction failure")
                || msg.contains("build() failed")
                || msg.contains("Internal"),
            "error must carry build failure context; got: {msg}"
        );
    }
}

include!("integration_tests/supports_and_refutes_pairing_tests.rs");

include!("integration_tests/source_kind_and_edge_update_tests.rs");

include!("integration_tests/visible_namespace_read_tests.rs");

include!("integration_tests/visibility_token_and_mutation_tests.rs");

// =============================================================================
// hybrid_search cross-namespace Option B limitation documented and tested
// =============================================================================

/// Verifies that `hybrid_search` with a visible-set token returns entities from
/// ALL visible namespaces (not just the primary namespace).
///
/// After FTS+ANN consolidation, `fts_entities` is a single shared table with a
/// `namespace` column. `hybrid_search` passes `visible_ns` as a `TextFilter`
/// so entities from any visible namespace are surfaced in one query pass.
#[tokio::test]
async fn hybrid_search_surfaces_all_visible_namespaces() {
    let rt = rt();

    let ns_primary = Namespace::parse("hs-primary-ns").unwrap();
    let ns_extra = Namespace::parse("hs-extra-ns").unwrap();

    let tok_primary = rt.authorize(ns_primary.clone()).unwrap();
    let tok_extra = rt.authorize(ns_extra.clone()).unwrap();

    // Create an entity in primary namespace with a distinctive term.
    let entity_in_primary = rt
        .create_entity_with_embedding_report(
            &tok_primary,
            "concept",
            None,
            "StellarPrimary",
            Some("unique stellar primary concept"),
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // Create an entity in the extra namespace with the same distinctive term.
    let entity_in_extra = rt
        .create_entity_with_embedding_report(
            &tok_extra,
            "concept",
            None,
            "StellarExtra",
            Some("unique stellar extra concept"),
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // Visible-set token: primary = hs-primary-ns, also sees hs-extra-ns.
    let vis_tok = rt
        .authorize_with_visibility(ns_primary.clone(), vec![ns_extra.clone()])
        .unwrap();

    // Search: FTS-only (no embedding model in test runtime).
    // With consolidated fts_entities, both namespace entities should surface.
    let hits = rt
        .hybrid_search(&vis_tok, "stellar", None, 20, None, None, &[], None)
        .await
        .unwrap();

    let hit_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();

    // Primary entity must surface.
    assert!(
        hit_ids.contains(&entity_in_primary.id),
        "hybrid_search must return entity from primary namespace; \
         expected entity_id={}, got: {hit_ids:?}",
        entity_in_primary.id,
    );

    // Extra-namespace entity must also surface (Phase-1.5 limitation lifted).
    // The consolidated fts_entities table + namespace column filter enables cross-namespace FTS.
    assert!(
        hit_ids.contains(&entity_in_extra.id),
        "hybrid_search must return entity from visible extra namespace; \
         entity_id={} missing from: {hit_ids:?}",
        entity_in_extra.id,
    );

    // Direct read of the extra-namespace entity via get_entity must still work.
    let fetched = rt
        .get_entity(&vis_tok, entity_in_extra.id)
        .await
        .expect("get_entity via visible-set token must return extra-namespace entity");
    assert_eq!(
        fetched.id, entity_in_extra.id,
        "visible-set read of extra-namespace entity must succeed"
    );
}

include!("integration_tests/cross_namespace_mutation_tests.rs");

include!("integration_tests/stats_and_harness_guard_tests.rs");
