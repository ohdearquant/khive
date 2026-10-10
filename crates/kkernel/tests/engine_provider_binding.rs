//! The downstream host seam binds providers from actual extra pack factories.
use async_trait::async_trait;
use khive_runtime::{
    EmbedderProvider, EngineConfig, KhiveRuntime, Namespace, NamespaceToken, PackFactory,
    PackRuntime, RuntimeConfig, RuntimeError, RuntimeResult, VerbRegistry,
};
use khive_types::{HandlerDef, VerbCategory, Visibility};
use kkernel::compose::compose_registry_with_extra_packs;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const NAME: &str = "external-binding-engine";
static HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "external_binding.status",
    description: "Host binding fixture",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[],
}];
struct Provider;
struct Service;
#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        NAME
    }
    fn dimensions(&self) -> usize {
        12
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(Service))
    }
}
#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0; 12]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "external-binding-fixture"
    }
}
struct ExternalPack {
    runtime: KhiveRuntime,
    hooks: AtomicUsize,
}
#[async_trait]
impl PackRuntime for ExternalPack {
    fn name(&self) -> &str {
        "external_binding"
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
        self.hooks.fetch_add(1, Ordering::SeqCst);
        runtime.try_register_embedder(Provider).unwrap();
    }
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> RuntimeResult<Value> {
        Ok(
            json!({ "engines": self.runtime.bound_embedding_engine_names()?,
            "default": self.runtime.default_embedder_name(), "hooks": self.hooks.load(Ordering::SeqCst) }),
        )
    }
}
struct Factory;
impl PackFactory for Factory {
    fn name(&self) -> &'static str {
        "external_binding"
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(ExternalPack {
            runtime,
            hooks: AtomicUsize::new(0),
        })
    }
}
static FACTORY: Factory = Factory;
// Deliberately no inventory::submit!: exercise the public downstream factory seam.
fn runtime(names: &[&str]) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        engines: Some(
            names
                .iter()
                .map(|name| EngineConfig {
                    name: (*name).into(),
                    weight: 1.0,
                    dims: Some(12),
                })
                .collect(),
        ),
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
    })
    .unwrap()
}
#[tokio::test]
async fn extra_factory_binds_default_and_secondary_before_real_dispatch() {
    let main = runtime(&[NAME]);
    let secondary = runtime(&[NAME]);
    let aliases = HashMap::from([
        ("external_binding".into(), secondary.clone()),
        ("same-registry-alias".into(), secondary.clone()),
    ]);
    let names = vec!["external_binding".into()];
    let registry = compose_registry_with_extra_packs(&names, &aliases, &main, &[&FACTORY]).unwrap();
    let reply = registry
        .dispatch("external_binding.status", json!({}))
        .await
        .unwrap();
    assert_eq!(
        reply,
        json!({"engines": [NAME], "default": NAME, "hooks": 2})
    );
    assert_eq!(main.bound_embedding_engine_names().unwrap(), [NAME]);
    assert_eq!(secondary.bound_embedding_engine_names().unwrap(), [NAME]);
    assert_eq!(main.embed_query("main").await.unwrap().len(), 12);
    assert_eq!(secondary.embed_query("secondary").await.unwrap().len(), 12);
    registry
        .initialize_embedding_engines(&[&secondary, &main, &secondary])
        .unwrap();
    assert_eq!(
        registry
            .dispatch("external_binding.status", json!({}))
            .await
            .unwrap()["hooks"],
        2
    );
}
#[tokio::test]
async fn explicitly_empty_pack_registry_stays_empty_while_core_uses_main_binding() {
    let main = runtime(&[NAME]);
    let secondary = runtime(&[]).with_core_embedders_from(&main);
    let aliases = HashMap::from([("external_binding".into(), secondary.clone())]);
    let registry = compose_registry_with_extra_packs(
        &["external_binding".into()],
        &aliases,
        &main,
        &[&FACTORY],
    )
    .unwrap();
    assert_eq!(
        registry
            .dispatch("external_binding.status", json!({}))
            .await
            .unwrap(),
        json!({"engines": [], "default": "", "hooks": 1})
    );
    assert!(secondary.registered_embedding_model_names().is_empty());
    assert!(matches!(
        secondary.embed_query("disabled").await,
        Err(RuntimeError::Unconfigured(_))
    ));
    assert_eq!(
        secondary.core().embed_query("core").await.unwrap().len(),
        12
    );
    assert_eq!(
        secondary.core().bound_embedding_engine_names().unwrap(),
        [NAME]
    );
}
#[test]
fn missing_provider_refuses_host_return_without_promoting_an_extra_provider() {
    let main = runtime(&["missing-external-engine"]);
    let error = match compose_registry_with_extra_packs(
        &["external_binding".into()],
        &HashMap::new(),
        &main,
        &[&FACTORY],
    ) {
        Ok(_) => panic!("host must not return with an unbound requested provider"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, RuntimeError::InvalidInput(message) if message.contains("missing-external-engine"))
    );
    assert_eq!(main.registered_embedding_model_names(), [NAME]);
    assert_eq!(main.default_embedder_name(), "missing-external-engine");
    assert!(matches!(
        main.bound_embedding_engine_names(),
        Err(RuntimeError::Unconfigured(_))
    ));
}
