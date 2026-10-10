//! Provider metadata binding is separate from service construction and storage.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use khive_runtime::{
    runtime_config_from_khive_config, EmbedderProvider, EngineConfig, IngestAuditStore,
    KhiveConfig, KhiveRuntime, Namespace, NamespaceToken, PackFactory, PackRegistration,
    PackRegistry, PackRuntime, RuntimeConfig, RuntimeError, RuntimeResult, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::SqlStatement;
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};

const CUSTOM: &str = "binding-custom";
const BUILTIN: &str = "all-minilm-l6-v2";

#[derive(Default)]
struct Calls {
    builds: AtomicUsize,
    queries: AtomicUsize,
    hooks: AtomicUsize,
}
struct Provider {
    name: String,
    dims: usize,
    calls: Arc<Calls>,
}
struct Service {
    dims: usize,
    calls: Arc<Calls>,
}
#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.calls.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Service {
            dims: self.dims,
            calls: Arc::clone(&self.calls),
        }))
    }
}
#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0; self.dims]).collect())
    }
    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.queries.fetch_add(texts.len(), Ordering::SeqCst);
        self.embed(texts, model).await
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "engine-binding-test"
    }
}
fn peer(name: &str, dims: Option<i64>) -> EngineConfig {
    EngineConfig {
        name: name.into(),
        weight: 1.0,
        dims,
    }
}
fn config(peers: Vec<EngineConfig>) -> RuntimeConfig {
    RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        engines: Some(peers),
        embedding_model: None,
        additional_embedding_models: vec![],
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: vec![],
        visibility_receipts: None,
        packs: vec![],
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: vec![],
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    }
}
fn make_runtime(peers: Vec<EngineConfig>) -> KhiveRuntime {
    let runtime = KhiveRuntime::new(config(peers)).unwrap();
    assert!(!runtime.backend().is_file_backed());
    runtime
}
fn register(runtime: &KhiveRuntime, name: &str, dims: usize, calls: &Arc<Calls>) {
    runtime
        .try_register_embedder(Provider {
            name: name.into(),
            dims,
            calls: Arc::clone(calls),
        })
        .unwrap();
}
fn invalid(error: RuntimeError, contains: &[&str]) {
    assert!(matches!(&error, RuntimeError::InvalidInput(_)), "{error:?}");
    let message = error.to_string();
    for part in contains {
        assert!(message.contains(part), "{message:?} lacks {part:?}");
    }
}
async fn schema(runtime: &KhiveRuntime) -> Value {
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query(SqlStatement {
            sql: "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name".into(),
            params: vec![],
            label: Some("test_binding_schema_snapshot".into()),
        })
        .await
        .unwrap();
    serde_json::to_value(rows).unwrap()
}

#[tokio::test]
async fn toml_custom_first_binds_in_order_without_build_or_schema_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("khive.toml");
    std::fs::write(&path, format!("[[engines]]\nname = {CUSTOM:?}\ndims = 12\n[[engines]]\nname = {BUILTIN:?}\ndims = 384\n")).unwrap();
    let parsed = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    let runtime =
        KhiveRuntime::new(runtime_config_from_khive_config(&parsed, config(vec![]))).unwrap();
    assert_eq!(runtime.default_embedder_name(), CUSTOM);
    assert_eq!(
        runtime.config().embedding_model,
        None,
        "legacy enum must not promote the second engine"
    );
    assert!(matches!(
        runtime.bound_embedding_engine_names(),
        Err(RuntimeError::Unconfigured(_))
    ));
    let custom = Arc::new(Calls::default());
    let builtin = Arc::new(Calls::default());
    register(&runtime, CUSTOM, 12, &custom);
    register(&runtime, BUILTIN, 384, &builtin);
    let before = schema(&runtime).await;
    runtime.finalize_embedding_engines().unwrap();
    assert_eq!(
        runtime.bound_embedding_engine_names().unwrap(),
        [CUSTOM, BUILTIN]
    );
    assert_eq!(schema(&runtime).await, before);
    assert_eq!(custom.builds.load(Ordering::SeqCst), 0);
    assert_eq!(builtin.builds.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.embed_query("custom default").await.unwrap().len(),
        12
    );
    assert_eq!(
        runtime
            .embed_query_with_model(BUILTIN, "named built-in")
            .await
            .unwrap()
            .len(),
        384
    );
    assert_eq!(custom.builds.load(Ordering::SeqCst), 1);
    assert_eq!(builtin.builds.load(Ordering::SeqCst), 1);
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .vectors(&token)
        .expect("custom default has a named vector store");
}

#[test]
fn missing_later_peer_does_not_publish_partial_binding_and_retry_is_idempotent() {
    let runtime = make_runtime(vec![peer(CUSTOM, Some(12)), peer("missing-later", Some(8))]);
    let calls = Arc::new(Calls::default());
    register(&runtime, CUSTOM, 12, &calls);
    invalid(
        runtime.finalize_embedding_engines().unwrap_err(),
        &["missing-later", "not registered"],
    );
    assert!(matches!(
        runtime.bound_embedding_engine_names(),
        Err(RuntimeError::Unconfigured(_))
    ));
    register(&runtime, "missing-later", 8, &calls);
    runtime.finalize_embedding_engines().unwrap();
    runtime.finalize_embedding_engines().unwrap();
    assert_eq!(
        runtime.bound_embedding_engine_names().unwrap(),
        [CUSTOM, "missing-later"]
    );
    assert_eq!(calls.builds.load(Ordering::SeqCst), 0);
    invalid(
        runtime
            .try_register_embedder(Provider {
                name: CUSTOM.into(),
                dims: 12,
                calls,
            })
            .unwrap_err(),
        &[CUSTOM, "already bound"],
    );
}

#[test]
fn actual_dimensions_are_checked_after_registration_without_loading_models() {
    let builtin = make_runtime(vec![peer(BUILTIN, Some(385))]);
    invalid(
        builtin.finalize_embedding_engines().unwrap_err(),
        &[BUILTIN, "385", "384"],
    );
    let calls = Arc::new(Calls::default());
    let custom = make_runtime(vec![peer(CUSTOM, Some(12))]);
    invalid(
        custom
            .try_register_embedder(Provider {
                name: CUSTOM.into(),
                dims: 13,
                calls: Arc::clone(&calls),
            })
            .unwrap_err(),
        &[CUSTOM, "12", "13"],
    );
    let mut invalid_dimensions = vec![0];
    if usize::BITS > 32 {
        invalid_dimensions.push(usize::MAX);
    }
    for dims in invalid_dimensions {
        let runtime = make_runtime(vec![peer(CUSTOM, None)]);
        register(&runtime, CUSTOM, dims, &calls);
        invalid(
            runtime.finalize_embedding_engines().unwrap_err(),
            &[CUSTOM, "dimensions"],
        );
        assert!(runtime.bound_embedding_engine_names().is_err());
    }
    let manual = make_runtime(vec![]);
    invalid(
        manual
            .try_register_embedder(Provider {
                name: BUILTIN.into(),
                dims: 12,
                calls: Arc::clone(&calls),
            })
            .unwrap_err(),
        &[BUILTIN, "384"],
    );
    assert_eq!(calls.builds.load(Ordering::SeqCst), 0);
}

#[test]
fn configured_order_is_not_an_enum_limit_or_registered_name_enumeration() {
    let names: Vec<String> = (0..16).map(|i| format!("custom-engine-{i:02}")).collect();
    let runtime = make_runtime(names.iter().map(|name| peer(name, Some(4))).collect());
    let calls = Arc::new(Calls::default());
    register(&runtime, "unused-provider", 7, &calls);
    for name in names.iter().rev() {
        register(&runtime, name, 4, &calls);
    }
    runtime.finalize_embedding_engines().unwrap();
    assert_eq!(runtime.bound_embedding_engine_names().unwrap(), names);
    assert_eq!(runtime.default_embedder_name(), "custom-engine-00");
    assert_eq!(runtime.registered_embedding_model_names().len(), 17);
    assert_eq!(
        runtime.registered_embedding_model_names()[0],
        "unused-provider"
    );
    assert_eq!(calls.builds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_empty_keeps_manual_named_access_without_configured_participation() {
    let runtime = make_runtime(vec![]);
    let calls = Arc::new(Calls::default());
    register(&runtime, CUSTOM, 12, &calls);
    runtime.finalize_embedding_engines().unwrap();
    assert!(runtime.bound_embedding_engine_names().unwrap().is_empty());
    assert_eq!(runtime.default_embedder_name(), "");
    assert_eq!(runtime.registered_embedding_model_names(), [CUSTOM]);
    assert!(matches!(
        runtime.embed_query("no default").await,
        Err(RuntimeError::Unconfigured(_))
    ));
    assert_eq!(
        runtime
            .embed_query_with_model(CUSTOM, "explicit manual")
            .await
            .unwrap()
            .len(),
        12
    );
    assert_eq!(calls.builds.load(Ordering::SeqCst), 1);
}

static HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "binding_fixture.status",
    description: "Configured binding test",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[],
}];
struct BindingPack {
    runtime: KhiveRuntime,
    calls: Arc<Calls>,
}
impl Pack for BindingPack {
    const NAME: &'static str = "binding_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &HANDLERS;
}
#[async_trait]
impl PackRuntime for BindingPack {
    fn name(&self) -> &str {
        "binding_fixture"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &HANDLERS
    }
    fn register_embedders(&self, runtime: &KhiveRuntime) {
        self.calls.hooks.fetch_add(1, Ordering::SeqCst);
        register(runtime, CUSTOM, 12, &self.calls);
    }
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> RuntimeResult<Value> {
        Ok(
            json!({ "engines": self.runtime.bound_embedding_engine_names()?, "hooks": self.calls.hooks.load(Ordering::SeqCst) }),
        )
    }
}
struct Factory;
impl PackFactory for Factory {
    fn name(&self) -> &'static str {
        "binding_fixture"
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(BindingPack {
            runtime,
            calls: Arc::new(Calls::default()),
        })
    }
}
static FACTORY: Factory = Factory;
inventory::submit! { PackRegistration(&FACTORY) }

#[tokio::test]
async fn actual_ingest_registry_runs_provider_hooks_before_return() {
    let mut cfg = config(vec![peer(CUSTOM, Some(12))]);
    cfg.packs = vec!["binding_fixture".into()];
    let runtime = KhiveRuntime::new(cfg).unwrap();
    let registry = PackRegistry::build_ingest_registry(&runtime, IngestAuditStore::Detach).unwrap();
    assert_eq!(
        registry
            .dispatch("binding_fixture.status", json!({}))
            .await
            .unwrap(),
        json!({"engines": [CUSTOM], "hooks": 1})
    );
    assert_eq!(
        runtime.embed_query("ingest default").await.unwrap().len(),
        12
    );
    let missing = make_runtime(vec![peer("not-linked", None)]);
    assert!(
        matches!(PackRegistry::build_ingest_registry(&missing, IngestAuditStore::Detach), Err(RuntimeError::InvalidInput(message)) if message.contains("not-linked"))
    );
}

#[test]
fn cloned_and_core_registries_run_hooks_once_while_empty_pack_stays_empty() {
    let main = make_runtime(vec![peer(CUSTOM, Some(12))]);
    let pack = make_runtime(vec![]).with_core_embedders_from(&main);
    let core = pack.core();
    let clone = main.clone();
    let calls = Arc::new(Calls::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BindingPack {
        runtime: main.clone(),
        calls: Arc::clone(&calls),
    });
    let registry = builder.build().unwrap();
    registry
        .initialize_embedding_engines(&[&main, &clone, &core, &pack])
        .unwrap();
    registry
        .initialize_embedding_engines(&[&core, &main, &pack])
        .unwrap();
    assert_eq!(calls.hooks.load(Ordering::SeqCst), 1);
    assert_eq!(core.bound_embedding_engine_names().unwrap(), [CUSTOM]);
    assert_eq!(core.default_embedder_name(), CUSTOM);
    assert!(pack.bound_embedding_engine_names().unwrap().is_empty());
    assert!(pack.registered_embedding_model_names().is_empty());
    assert_eq!(calls.builds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn custom_default_text_hybrid_reaches_vector_only_candidate() {
    let runtime = make_runtime(vec![peer(CUSTOM, Some(12))]);
    let calls = Arc::new(Calls::default());
    register(&runtime, CUSTOM, 12, &calls);
    runtime.finalize_embedding_engines().unwrap();
    assert!(runtime.config().embedding_model.is_none());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity = khive_storage::Entity::new("local", "concept", "vector only candidate");
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    runtime
        .vectors(&token)
        .unwrap()
        .insert(
            entity.id,
            khive_types::SubstrateKind::Entity,
            "local",
            "vector only candidate",
            vec![vec![1.0; 12]],
        )
        .await
        .unwrap();
    // No FTS document exists. Only the custom default vector stage can return this ID.
    let hits = runtime
        .hybrid_search(&token, "unrelatedquery", None, 5, None, None, &[], None)
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
        [entity.id]
    );
    assert_eq!(calls.queries.load(Ordering::SeqCst), 1);
    let raw = runtime
        .hybrid_search(
            &token,
            "unrelatedquery",
            Some(vec![1.0; 12]),
            5,
            None,
            None,
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        raw.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
        [entity.id]
    );
    assert_eq!(
        calls.queries.load(Ordering::SeqCst),
        1,
        "raw vector must not build a second query"
    );
    let empty = make_runtime(vec![]);
    let token = empty.authorize(Namespace::local()).unwrap();
    assert!(empty
        .hybrid_search(&token, "unrelatedquery", None, 5, None, None, &[], None)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        empty
            .hybrid_search(
                &token,
                "unrelatedquery",
                Some(vec![1.0; 12]),
                5,
                None,
                None,
                &[],
                None
            )
            .await,
        Err(RuntimeError::Unconfigured(_))
    ));
}

#[tokio::test]
async fn finalization_preserves_existing_vector_spaces_and_metadata() {
    let runtime = make_runtime(vec![peer(CUSTOM, Some(12))]);
    let calls = Arc::new(Calls::default());
    register(&runtime, CUSTOM, 12, &calls);
    register(&runtime, "historical-provider", 8, &calls);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let id = uuid::Uuid::from_u128(73);
    let historical = runtime
        .vectors_for_model(&token, "historical-provider")
        .unwrap();
    historical
        .insert(
            id,
            khive_types::SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0; 8]],
        )
        .await
        .unwrap();
    let before_schema = schema(&runtime).await;
    let before_models =
        serde_json::to_value(runtime.list_embedding_models().await.unwrap()).unwrap();
    runtime.finalize_embedding_engines().unwrap();
    assert_eq!(schema(&runtime).await, before_schema);
    assert_eq!(
        serde_json::to_value(runtime.list_embedding_models().await.unwrap()).unwrap(),
        before_models
    );
    assert_eq!(historical.count().await.unwrap(), 1);
    assert_eq!(
        runtime
            .rerank_in(&token, "historical-provider", &[1.0; 8], &[id], 1)
            .await
            .unwrap()[0]
            .subject_id,
        id
    );
    assert_eq!(calls.builds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn strategy_and_note_search_use_custom_default_with_keyword_and_error_controls() {
    let runtime = make_runtime(vec![peer(CUSTOM, Some(12))]);
    let calls = Arc::new(Calls::default());
    register(&runtime, CUSTOM, 12, &calls);
    runtime.finalize_embedding_engines().unwrap();
    assert!(runtime.vector_arm_selected());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity = khive_storage::Entity::new("local", "concept", "entity candidate");
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    let note = khive_storage::Note::new("local", "observation", "note candidate");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let vectors = runtime.vectors(&token).unwrap();
    for (id, kind) in [
        (entity.id, khive_types::SubstrateKind::Entity),
        (note.id, khive_types::SubstrateKind::Note),
    ] {
        vectors
            .insert(id, kind, "local", "content", vec![vec![1.0; 12]])
            .await
            .unwrap();
    }
    let strategy = runtime
        .hybrid_search_with_strategy(
            &token,
            "unrelatedquery",
            None,
            khive_runtime::FusionStrategy::VectorOnly,
            5,
        )
        .await
        .unwrap();
    assert_eq!(
        strategy.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
        [entity.id]
    );
    let after_vector = calls.queries.load(Ordering::SeqCst);
    assert_eq!(after_vector, 1);
    let keyword = runtime
        .hybrid_search_with_strategy(
            &token,
            "unrelatedquery",
            None,
            khive_runtime::FusionStrategy::KeywordOnly,
            5,
        )
        .await
        .unwrap();
    assert!(keyword.is_empty());
    assert_eq!(calls.queries.load(Ordering::SeqCst), after_vector);
    let notes = runtime
        .search_notes(&token, "unrelatedquery", None, 5, None, true, &[], None)
        .await
        .unwrap();
    assert_eq!(
        notes.iter().map(|hit| hit.note_id).collect::<Vec<_>>(),
        [note.id]
    );
    assert_eq!(calls.queries.load(Ordering::SeqCst), 2);
    assert!(runtime
        .search_notes(
            &token,
            "unrelatedquery",
            Some(vec![1.0; 11]),
            5,
            None,
            true,
            &[],
            None
        )
        .await
        .is_err());
    let empty = make_runtime(vec![]);
    let empty_token = empty.authorize(Namespace::local()).unwrap();
    assert!(!empty.vector_arm_selected());
    assert!(empty
        .search_notes(
            &empty_token,
            "unrelatedquery",
            None,
            5,
            None,
            true,
            &[],
            None
        )
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        empty
            .search_notes(
                &empty_token,
                "unrelatedquery",
                Some(vec![1.0; 12]),
                5,
                None,
                true,
                &[],
                None
            )
            .await,
        Err(RuntimeError::Unconfigured(_))
    ));
    assert!(matches!(
        empty
            .hybrid_search_with_strategy(
                &empty_token,
                "unrelatedquery",
                Some(vec![1.0; 12]),
                khive_runtime::FusionStrategy::VectorOnly,
                5
            )
            .await,
        Err(RuntimeError::Unconfigured(_))
    ));
}
