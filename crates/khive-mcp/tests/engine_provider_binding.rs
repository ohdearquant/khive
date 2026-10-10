use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use khive_mcp::serve::build_registry_for_multi_backend;
use khive_mcp::server::{KhiveMcpServer, PackRegFailure};
use khive_mcp::tools::request::RequestParams;
use khive_runtime::embedder_registry::EmbedderProvider;
use khive_runtime::engine_config::PackConfig;
use khive_runtime::{
    BackendConfig, BackendKind, EngineConfig, KhiveConfig, KhiveRuntime, Namespace, NamespaceToken,
    PackFactory, PackRegistration, PackRuntime, RuntimeConfig, RuntimeError, RuntimeResult,
    VerbRegistry, WalCeilingSource,
};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};

const PACK: &str = "bindingprobe";
const CUSTOM: &str = "mcp-binding-custom";
const ZERO: &str = "mcp-binding-zero";
const BUILTIN: &str = "all-minilm-l6-v2";

#[derive(Default)]
struct Counts {
    hooks: AtomicUsize,
    custom_builds: AtomicUsize,
    builtin_builds: AtomicUsize,
    custom_calls: AtomicUsize,
    builtin_calls: AtomicUsize,
    stages: Mutex<Vec<&'static str>>,
}

impl Counts {
    fn snapshot(&self) -> Value {
        json!({
            "hooks": self.hooks.load(Ordering::SeqCst),
            "custom_builds": self.custom_builds.load(Ordering::SeqCst),
            "builtin_builds": self.builtin_builds.load(Ordering::SeqCst),
            "custom_calls": self.custom_calls.load(Ordering::SeqCst),
            "builtin_calls": self.builtin_calls.load(Ordering::SeqCst),
            "stages": *self.stages.lock().unwrap(),
        })
    }
}

struct Provider {
    name: &'static str,
    dimensions: usize,
    counts: Arc<Counts>,
}

#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        assert_ne!(
            self.name, ZERO,
            "invalid metadata must fail before building"
        );
        let counter = if self.name == BUILTIN {
            &self.counts.builtin_builds
        } else {
            &self.counts.custom_builds
        };
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Service {
            name: self.name,
            dimensions: self.dimensions,
            counts: Arc::clone(&self.counts),
        }))
    }
}

struct Service {
    name: &'static str,
    dimensions: usize,
    counts: Arc<Counts>,
}

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let (counter, value) = if self.name == BUILTIN {
            (&self.counts.builtin_calls, 0.25)
        } else {
            (&self.counts.custom_calls, 0.75)
        };
        counter.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![value; self.dimensions]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct ProbePack(Arc<Counts>);

impl Pack for ProbePack {
    const NAME: &'static str = PACK;
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "bindingprobe.status",
        description: "Read fixture counters",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for ProbePack {
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

    fn host_state(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.0.clone())
    }

    fn register_embedders(&self, runtime: &KhiveRuntime) {
        assert!(matches!(
            runtime.bound_embedding_engine_names(),
            Err(RuntimeError::Unconfigured(_))
        ));
        self.0.hooks.fetch_add(1, Ordering::SeqCst);
        self.0.stages.lock().unwrap().push("providers");
        for (name, dimensions) in [(CUSTOM, 2), (BUILTIN, 384), (ZERO, 0)] {
            runtime.register_embedder(Provider {
                name,
                dimensions,
                counts: Arc::clone(&self.0),
            });
        }
    }

    fn register_entity_type_validator(&self, runtime: &KhiveRuntime) {
        let _ = runtime.bound_embedding_engine_names().unwrap();
        self.0.stages.lock().unwrap().push("entity-validator");
    }

    fn register_note_write_validator(&self, runtime: &KhiveRuntime) {
        let _ = runtime.bound_embedding_engine_names().unwrap();
        self.0.stages.lock().unwrap().push("note-validator");
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> RuntimeResult<Value> {
        assert_eq!(verb, "bindingprobe.status");
        Ok(self.0.snapshot())
    }
}

struct Factory;

impl PackFactory for Factory {
    fn name(&self) -> &'static str {
        PACK
    }

    fn create(&self, _runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(ProbePack(Arc::new(Counts::default())))
    }
}

inventory::submit! { PackRegistration(&Factory) }

fn isolated() -> bool {
    let directory = tempfile::tempdir().unwrap();
    khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_MCP_ENGINE_BINDING_CHILD",
        false,
        |command| {
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env_remove("LATTICE_MODEL_CACHE")
                .current_dir(directory.path())
                .env("HOME", directory.path())
                .env("LATTICE_MODEL_CACHE", directory.path().join("models"))
                .env("KHIVE_BLOB_ROOT", directory.path().join("blobs"))
                .env("KHIVE_VOLUME_LOCK_DIR", directory.path().join("locks"));
        },
    )
}

fn engine(name: &str, dims: Option<i64>) -> EngineConfig {
    EngineConfig {
        name: name.into(),
        weight: 1.0,
        dims,
    }
}

fn configured(engines: Vec<EngineConfig>) -> RuntimeConfig {
    RuntimeConfig {
        db_path: None,
        engines: Some(engines),
        default_namespace: Namespace::parse("engine-binding-test").unwrap(),
        actor_id: Some("engine-binding-test".into()),
        packs: vec![PACK.into()],
        credentials: Vec::new(),
        mounts: Vec::new(),
        events_split: None,
        visibility_receipts: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        ..RuntimeConfig::no_embeddings()
    }
}

fn peers() -> Vec<EngineConfig> {
    vec![engine(CUSTOM, Some(2)), engine(BUILTIN, Some(384))]
}

fn topology(backend: &str, no_embed: bool) -> KhiveConfig {
    let backends = ["main", "secondary"]
        .into_iter()
        .map(|name| BackendConfig {
            name: name.into(),
            kind: BackendKind::Memory,
            path: None,
            cache_mb: None,
            journal_mode: None,
            served_kinds: None,
            read_only: false,
            wal_ceiling_bytes: Some(0),
            disk_reserve_bytes: Some(0),
            disk_guard_deadline_ms: None,
        })
        .collect();
    KhiveConfig {
        backends,
        packs: [(
            PACK.into(),
            PackConfig {
                backend: backend.into(),
                verbs_disabled: Vec::new(),
                no_embed,
            },
        )]
        .into_iter()
        .collect(),
        ..KhiveConfig::default()
    }
}

async fn status(server: &KhiveMcpServer) -> Value {
    let response = server
        .dispatch_request_local(RequestParams {
            ops: json!([{"tool": "bindingprobe.status", "args": {}}]).to_string(),
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..RequestParams::default()
        })
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["ok"], true, "{body}");
    body["results"][0]["result"].clone()
}

async fn assert_named_calls(runtime: &KhiveRuntime) {
    assert_eq!(
        runtime
            .embed_query_with_model(CUSTOM, "custom")
            .await
            .unwrap(),
        vec![0.75; 2]
    );
    assert_eq!(
        runtime
            .embed_query_with_model(BUILTIN, "builtin")
            .await
            .unwrap(),
        vec![0.25; 384]
    );
}

#[tokio::test]
async fn single_startup_binds_custom_first_without_building_and_preserves_hook_order() {
    if isolated() {
        return;
    }
    let runtime = KhiveRuntime::new(configured(peers())).unwrap();
    assert!(!runtime.backend().is_file_backed());
    assert!(matches!(
        runtime.bound_embedding_engine_names(),
        Err(RuntimeError::Unconfigured(_))
    ));
    let server = KhiveMcpServer::with_packs(runtime.clone(), &[PACK.into()]).unwrap();
    assert!(server.event_store().is_some());
    assert_eq!(
        runtime.bound_embedding_engine_names().unwrap(),
        [CUSTOM, BUILTIN]
    );
    assert_eq!(runtime.default_embedder_name(), CUSTOM);
    assert!(runtime.config().embedding_model.is_none());
    let before = status(&server).await;
    assert_eq!(before["hooks"], 1);
    assert_eq!(before["custom_builds"], 0);
    assert_eq!(before["builtin_builds"], 0);
    assert_eq!(
        before["stages"],
        json!(["providers", "entity-validator", "note-validator"])
    );
    assert_named_calls(&runtime).await;
    assert_named_calls(&runtime.core()).await;
    assert_eq!(runtime.embed_query("default").await.unwrap(), vec![0.75; 2]);
    let after = status(&server).await;
    assert_eq!(after["custom_builds"], 1);
    assert_eq!(after["builtin_builds"], 1);
    assert_eq!(after["custom_calls"], 3);
    assert_eq!(after["builtin_calls"], 2);

    let repeated = KhiveMcpServer::with_packs(runtime.clone(), &[PACK.into()]).unwrap();
    let repeated_state = status(&repeated).await;
    assert_eq!(repeated_state["hooks"], 0);
    assert_eq!(
        runtime.bound_embedding_engine_names().unwrap(),
        [CUSTOM, BUILTIN]
    );
    assert_named_calls(&runtime).await;
    assert_eq!(status(&server).await["custom_builds"], 1);
    assert_eq!(status(&server).await["custom_calls"], 4);
}

#[tokio::test]
async fn single_startup_refuses_missing_or_invalid_provider_before_returning_server() {
    if isolated() {
        return;
    }
    for (name, dims) in [
        ("mcp-binding-absent", None),
        (ZERO, None),
        (CUSTOM, Some(3)),
    ] {
        let runtime = KhiveRuntime::new(configured(vec![engine(name, dims)])).unwrap();
        let error = match KhiveMcpServer::with_packs(runtime, &[PACK.into()]) {
            Ok(_) => panic!("startup accepted invalid engine {name}"),
            Err(error) => error,
        };
        assert!(
            matches!(&error.failure, PackRegFailure::Registry(RuntimeError::InvalidInput(message)) if message.contains(name))
        );
        if name == ZERO {
            assert!(error.to_string().contains("invalid dimensions 0"));
        }
        assert!(matches!(
            error.runtime.bound_embedding_engine_names(),
            Err(RuntimeError::Unconfigured(_))
        ));
    }
    let runtime = KhiveRuntime::new(configured(peers())).unwrap();
    let healthy = KhiveMcpServer::with_packs(runtime, &[PACK.into()]).unwrap();
    assert_eq!(status(&healthy).await["custom_builds"], 0);
}

#[tokio::test]
async fn multi_startup_binds_each_registry_once_and_repeated_aliases_do_not_build() {
    if isolated() {
        return;
    }
    let built =
        build_registry_for_multi_backend(configured(peers()), &topology("secondary", false), None)
            .await
            .unwrap();
    let state = built.registry.pack_host_state::<Counts>(PACK).unwrap();
    let pack = &built.per_pack_runtimes[PACK];
    let core = pack.core();
    assert!(!built.main_backend.is_file_backed());
    assert!(!pack.backend().is_file_backed());
    assert!(!std::ptr::eq(built.main_backend.as_ref(), pack.backend()));
    assert!(std::ptr::eq(built.main_backend.as_ref(), core.backend()));
    for runtime in [&built.default_runtime, pack.as_ref(), &core] {
        assert_eq!(
            runtime.bound_embedding_engine_names().unwrap(),
            [CUSTOM, BUILTIN]
        );
        assert_eq!(runtime.default_embedder_name(), CUSTOM);
    }
    assert_eq!(state.snapshot()["hooks"], 2);
    assert_eq!(state.snapshot()["custom_builds"], 0);
    assert_eq!(state.snapshot()["builtin_builds"], 0);
    built
        .registry
        .initialize_embedding_engines(&[&core, &built.default_runtime, pack, &core, pack])
        .unwrap();
    built
        .registry
        .initialize_embedding_engines(&[pack, &built.default_runtime])
        .unwrap();
    assert_eq!(state.snapshot()["hooks"], 2);
    assert_named_calls(&built.default_runtime).await;
    assert_named_calls(pack).await;
    assert_named_calls(&core).await;
    assert_eq!(state.snapshot()["custom_builds"], 2);
    assert_eq!(state.snapshot()["builtin_builds"], 2);
    assert_eq!(state.snapshot()["custom_calls"], 3);
    assert_eq!(state.snapshot()["builtin_calls"], 3);
}

#[tokio::test]
async fn multi_startup_refuses_missing_or_zero_dimensional_provider() {
    if isolated() {
        return;
    }
    for name in ["mcp-binding-absent", ZERO] {
        let error = match build_registry_for_multi_backend(
            configured(vec![engine(name, None)]),
            &topology("secondary", false),
            None,
        )
        .await
        {
            Ok(_) => panic!("multi-backend startup accepted {name}"),
            Err(error) => error,
        };
        assert!(
            matches!(error.downcast_ref::<RuntimeError>(), Some(RuntimeError::InvalidInput(message)) if message.contains(name))
        );
        if name == ZERO {
            assert!(error.to_string().contains("invalid dimensions 0"));
        }
    }
    let healthy =
        build_registry_for_multi_backend(configured(peers()), &topology("secondary", false), None)
            .await
            .unwrap();
    assert_eq!(
        healthy
            .default_runtime
            .bound_embedding_engine_names()
            .unwrap(),
        [CUSTOM, BUILTIN]
    );
}

#[tokio::test]
async fn no_embed_pack_keeps_main_core_binding_for_both_backend_routes() {
    if isolated() {
        return;
    }
    for assigned in ["main", "secondary"] {
        let built =
            build_registry_for_multi_backend(configured(peers()), &topology(assigned, true), None)
                .await
                .unwrap();
        let state = built.registry.pack_host_state::<Counts>(PACK).unwrap();
        let pack = &built.per_pack_runtimes[PACK];
        let core = pack.core();
        assert!(pack.bound_embedding_engine_names().unwrap().is_empty());
        assert!(pack.registered_embedding_model_names().is_empty());
        assert_eq!(pack.default_embedder_name(), "");
        assert_eq!(
            core.bound_embedding_engine_names().unwrap(),
            [CUSTOM, BUILTIN]
        );
        assert_eq!(core.default_embedder_name(), CUSTOM);
        assert_eq!(state.snapshot()["hooks"], 1);
        assert_eq!(state.snapshot()["custom_builds"], 0);
        assert!(
            matches!(pack.embedder(CUSTOM).await, Err(RuntimeError::UnknownModel(name)) if name == CUSTOM)
        );
        built
            .registry
            .initialize_embedding_engines(&[pack, &core, &built.default_runtime, &core])
            .unwrap();
        assert_eq!(state.snapshot()["hooks"], 1);
        assert_named_calls(&core).await;
        assert_named_calls(&built.default_runtime).await;
        assert_eq!(state.snapshot()["custom_builds"], 1);
        assert_eq!(state.snapshot()["builtin_builds"], 1);
        assert_eq!(state.snapshot()["custom_calls"], 2);
        assert_eq!(state.snapshot()["builtin_calls"], 2);
    }
}

#[tokio::test]
async fn explicitly_empty_hosts_skip_provider_hooks_and_return_empty_bindings() {
    if isolated() {
        return;
    }
    let runtime = KhiveRuntime::new(configured(Vec::new())).unwrap();
    let server = KhiveMcpServer::with_packs(runtime.clone(), &[PACK.into()]).unwrap();
    assert!(runtime.bound_embedding_engine_names().unwrap().is_empty());
    assert!(runtime.registered_embedding_model_names().is_empty());
    assert_eq!(status(&server).await["hooks"], 0);
    let manual = Arc::new(Counts::default());
    runtime
        .try_register_embedder(Provider {
            name: CUSTOM,
            dimensions: 2,
            counts: Arc::clone(&manual),
        })
        .unwrap();
    assert_eq!(
        runtime
            .embed_query_with_model(CUSTOM, "manual")
            .await
            .unwrap(),
        vec![0.75; 2]
    );
    assert!(runtime.bound_embedding_engine_names().unwrap().is_empty());
    assert_eq!(manual.snapshot()["custom_builds"], 1);
    assert_eq!(manual.snapshot()["custom_calls"], 1);
    let built = build_registry_for_multi_backend(
        configured(Vec::new()),
        &topology("secondary", false),
        None,
    )
    .await
    .unwrap();
    let state = built.registry.pack_host_state::<Counts>(PACK).unwrap();
    let pack = &built.per_pack_runtimes[PACK];
    for runtime in [&built.default_runtime, pack.as_ref(), &pack.core()] {
        assert!(runtime.bound_embedding_engine_names().unwrap().is_empty());
        assert!(runtime.registered_embedding_model_names().is_empty());
    }
    assert_eq!(state.snapshot()["hooks"], 0);
    assert_eq!(state.snapshot()["custom_builds"], 0);
    assert_eq!(state.snapshot()["builtin_builds"], 0);
}
