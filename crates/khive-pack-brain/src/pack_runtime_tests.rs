//! Preventive coverage: today's concrete BrainPack still shares the omitted defaults.
use super::BrainPackRuntime;
use khive_runtime::mounted_verb::MountedVerb;
use khive_runtime::{
    BackendId, ChannelIngestCapability, KhiveRuntime, Namespace, NamespaceToken, PackRuntime,
    RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};
use std::any::Any;
use std::sync::{Arc, Mutex};

// A test-time source-shape check, not a Rust parser or a compile-time trait check.
// Inventory actual method declarations; refuse block comments, raw/multiline strings,
// macros and qualified method headers rather than silently miss a new default method.
fn surface(source: &str, anchor: &str) -> std::collections::BTreeSet<String> {
    let body = source
        .split_once(anchor)
        .expect("source anchor")
        .1
        .split_once("\n}\n")
        .expect("scope end")
        .0;
    let mut methods = std::collections::BTreeSet::new();
    for line in body
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
    {
        assert!(
            !line.contains("/*") && !line.contains("r#") && !line.contains("r\""),
            "unsupported lexical form"
        );
        let (mut quoted, mut escaped) = (false, false);
        let mut code = String::new();
        for c in line.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            if quoted && c == '\\' {
                escaped = true;
                continue;
            }
            if c == '"' {
                quoted = !quoted;
                continue;
            }
            if !quoted {
                code.push(c);
            }
        }
        assert!(!quoted, "multiline string requires inventory review");
        let code = code
            .split_once("//")
            .map_or(code.as_str(), |(head, _)| head);
        let Some(header) = code.strip_prefix("    ").filter(|s| !s.starts_with(' ')) else {
            continue;
        };
        let name = header
            .strip_prefix("fn ")
            .or_else(|| header.strip_prefix("async fn "));
        if let Some(name) = name {
            let name = name.split_once('(').expect("method header").0;
            assert!(
                name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                "unsupported method spelling"
            );
            assert!(methods.insert(name.to_owned()), "duplicate method");
        } else {
            assert!(
                !header.contains("fn ") && !header.contains('!'),
                "unsupported method header"
            );
        }
    }
    assert!(!methods.is_empty(), "empty inventory");
    methods
}
const TRAIT_ANCHOR: &str = "pub trait PackRuntime: Send + Sync {";
const WRAPPER_ANCHOR: &str = "impl<P: khive_runtime::pack::PackRuntime>";

#[test]
fn forwarder_surface_tracks_the_actual_trait() {
    let contract = include_str!("../../khive-runtime/src/pack/traits.rs");
    let wrapper = include_str!("pack.rs");
    let expected = surface(contract, TRAIT_ANCHOR);
    assert_eq!(expected, surface(wrapper, WRAPPER_ANCHOR));
    let added = contract.replace(
        "    fn host_state(",
        "    fn added_default(&self) {}\n    fn host_state(",
    );
    assert_ne!(added, contract);
    assert_ne!(surface(&added, TRAIT_ANCHOR), expected);
    let missing = wrapper.replace(
        "    async fn warm(&self) {\n        self.0.warm().await\n    }\n",
        "",
    );
    assert_ne!(missing, wrapper);
    assert_ne!(surface(&missing, WRAPPER_ANCHOR), expected);
    let renamed = wrapper.replace("fn host_state(", "fn other_host_state(");
    assert_ne!(renamed, wrapper);
    assert_ne!(surface(&renamed, WRAPPER_ANCHOR), expected);
    let noise = "    // fn fake(&self) {}\n    /// fn documented(&self) {}\n        let text = \"fn fake(&self)\";\n    impl Inner {\n        fn nested(&self) {}\n    }\n";
    assert_eq!(
        surface(
            &wrapper.replace("    fn name", &format!("{noise}    fn name")),
            WRAPPER_ANCHOR
        ),
        expected
    );
    for unsupported in [
        "    unsafe fn extra(&self) {}\n",
        "    extern \"C\" fn extra(&self) {}\n",
        "    generated_methods!();\n",
        "    /*\n    fn fake(&self) {}\n    */\n",
        "    let s = \"multiline\n    fn fake(&self) {}\n    \";\n",
        "    let s = r#\"text\"#;\n",
        "    fn name(&self) {}\n",
    ] {
        let changed = wrapper.replace("    fn name", &format!("{unsupported}    fn name"));
        assert!(std::panic::catch_unwind(|| surface(&changed, WRAPPER_ANCHOR)).is_err());
    }
}

struct Probe<'a> {
    calls: Mutex<Vec<&'static str>>,
    state: Arc<u32>,
    runtime: &'a KhiveRuntime,
    registry: &'a VerbRegistry,
    token: &'a NamespaceToken,
}
impl Probe<'_> {
    fn hit(&self, name: &'static str) {
        self.calls.lock().unwrap().push(name);
    }
    fn runtime(&self, name: &'static str, runtime: &KhiveRuntime) {
        assert!(std::ptr::eq(runtime, self.runtime));
        self.hit(name);
    }
    fn request(&self, verb: &str, params: &Value, registry: &VerbRegistry, token: &NamespaceToken) {
        assert_eq!(verb, "probe.verb");
        assert_eq!(params, &json!({"argument": 7}));
        assert!(std::ptr::eq(registry, self.registry));
        assert!(std::ptr::eq(token, self.token));
    }
}
macro_rules! metadata_probes {
    ($($name:ident -> $ty:ty = $value:expr;)*) => {
        $(fn $name(&self) -> $ty { self.hit(stringify!($name)); $value })*
    };
}
macro_rules! registration_probes {
    ($($name:ident),*) => {
        $(fn $name(&self, runtime: &KhiveRuntime) { self.runtime(stringify!($name), runtime); })*
    };
}
#[async_trait::async_trait]
impl PackRuntime for Probe<'_> {
    metadata_probes! {
        name -> &str = "probe";
        note_kinds -> &'static [&'static str] = &["probe-note"];
        entity_kinds -> &'static [&'static str] = &["probe-entity"];
        brain_consumer_kinds -> &'static [&'static str] = &["probe-consumer"];
        handlers -> &'static [khive_runtime::HandlerDef] = &[];
        edge_rules -> &'static [khive_types::EdgeEndpointRule] = &[];
        entity_types -> &'static [khive_types::EntityTypeDef] = &[];
        requires -> &'static [&'static str] = &["probe-required"];
        note_kind_specs -> &'static [khive_runtime::NoteKindSpec] = &[];
        note_embedding_policies -> &'static [khive_types::NoteEmbeddingPolicySpec] = &[];
        schema_column_additions -> &'static [khive_types::PackColumnAddition] = &[];
        validation_rules -> &'static [khive_runtime::ValidationRule] = &[];
        registered_embedding_model_names -> Vec<String> = vec!["probe-model".into()];
        mounted_namespace -> Option<&str> = Some("probe-mount");
    }
    registration_probes! { register_embedders, register_entity_type_validator,
    register_note_mutation_hook, register_note_search_ann_provider, register_note_write_validator }
    fn host_state(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.hit("host_state");
        Some(self.state.clone())
    }
    fn validate_config(&self) -> Result<(), RuntimeError> {
        self.hit("validate_config");
        Err(RuntimeError::InvalidInput("probe config".into()))
    }
    fn input_schema(&self, verb: &str) -> Option<Value> {
        self.hit("input_schema");
        Some(json!({"verb": verb}))
    }
    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn khive_runtime::KindHook>> {
        self.hit("kind_hook");
        assert_eq!(kind, "probe-kind");
        None
    }
    fn accept_channel_ingest_capability(&self, _capability: ChannelIngestCapability) {
        self.hit("accept_channel_ingest_capability");
    }
    fn schema_plan(&self) -> khive_runtime::SchemaPlan {
        self.hit("schema_plan");
        khive_runtime::SchemaPlan {
            pack: "probe",
            statements: &["probe ddl"],
        }
    }
    fn register_entity_type_validator_with_types(
        &self,
        runtime: &KhiveRuntime,
        types: &[khive_types::EntityTypeDef],
    ) {
        self.runtime("register_entity_type_validator_with_types", runtime);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].type_name, "probe-type");
    }
    async fn warm(&self) {
        self.hit("warm");
    }
    async fn apply_profile_section_feedback(
        &self,
        token: &NamespaceToken,
        profile: &str,
        signals: Value,
        attribution: Option<String>,
    ) -> Result<Value, RuntimeError> {
        self.hit("apply_profile_section_feedback");
        assert!(std::ptr::eq(token, self.token));
        assert_eq!(profile, "profile");
        assert_eq!(signals, json!({"section": 0.5}));
        assert_eq!(attribution.as_deref(), Some("target"));
        Ok(json!({"feedback": "probe"}))
    }
    fn mounted_catalog_snapshot(&self) -> Vec<MountedVerb> {
        self.hit("mounted_catalog_snapshot");
        vec![mounted()]
    }
    async fn mounted_catalog(&self) -> Result<Vec<MountedVerb>, RuntimeError> {
        self.hit("mounted_catalog");
        Err(RuntimeError::InvalidInput("probe catalog".into()))
    }
    async fn dispatch_mounted(
        &self,
        definition: &MountedVerb,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.hit("dispatch_mounted");
        self.request(verb, &params, registry, token);
        assert_eq!(definition.digest, "probe digest");
        Err(RuntimeError::InvalidInput("probe mounted dispatch".into()))
    }
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.hit("dispatch");
        self.request(verb, &params, registry, token);
        Ok(json!({"dispatch": "probe"}))
    }
}
fn mounted() -> MountedVerb {
    MountedVerb {
        name: "probe.verb".into(),
        description: None,
        input_schema: json!({}),
        output_schema: None,
        effect: khive_runtime::mount_config::MountEffect::Read,
        digest: "probe digest".into(),
        generation: 7,
    }
}
fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        credentials: vec![],
        visibility_receipts: None,
        mounts: vec![],
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: khive_db::DiskGuardEnvironment {
            reserve: None,
            legacy_reserve: None,
            deadline: None,
        },
        disk_guard_config: None,
        volume_lock_dir: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(khive_runtime::AllowAllGate),
        packs: vec![],
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some("forwarding-probe".into()),
        brain: Default::default(),
        git_write: Default::default(),
        blob: Default::default(),
        exec: Default::default(),
        telemetry: Default::default(),
        display_timezone: chrono_tz::UTC,
        events_split: None,
        web: Default::default(),
    })
    .expect("isolated in-memory runtime")
}

#[tokio::test]
async fn forwards_every_runtime_method_to_the_same_inner_instance() {
    let runtime = runtime();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let inner = Arc::new(Probe {
        calls: Mutex::new(vec![]),
        state: Arc::new(71),
        runtime: &runtime,
        registry: &registry,
        token: &token,
    });
    let wrapper = BrainPackRuntime(Arc::clone(&inner));
    macro_rules! called {
        ($name:ident, $value:expr) => {{
            let result = $value;
            assert_eq!(
                std::mem::take(&mut *inner.calls.lock().unwrap()),
                [stringify!($name)]
            );
            result
        }};
    }
    assert_eq!(called!(name, wrapper.name()), "probe");
    let state = called!(host_state, wrapper.host_state())
        .unwrap()
        .downcast::<u32>()
        .unwrap();
    assert!(Arc::ptr_eq(&state, &inner.state));
    assert!(
        matches!(called!(validate_config, wrapper.validate_config()), Err(RuntimeError::InvalidInput(s)) if s == "probe config")
    );
    assert_eq!(called!(note_kinds, wrapper.note_kinds()), &["probe-note"]);
    assert_eq!(
        called!(entity_kinds, wrapper.entity_kinds()),
        &["probe-entity"]
    );
    assert_eq!(
        called!(brain_consumer_kinds, wrapper.brain_consumer_kinds()),
        &["probe-consumer"]
    );
    assert_eq!(
        called!(
            apply_profile_section_feedback,
            wrapper
                .apply_profile_section_feedback(
                    &token,
                    "profile",
                    json!({"section": 0.5}),
                    Some("target".into())
                )
                .await
        )
        .unwrap(),
        json!({"feedback": "probe"})
    );
    assert!(called!(handlers, wrapper.handlers()).is_empty());
    assert_eq!(
        called!(input_schema, wrapper.input_schema("probe.input")),
        Some(json!({"verb": "probe.input"}))
    );
    assert!(called!(edge_rules, wrapper.edge_rules()).is_empty());
    assert!(called!(entity_types, wrapper.entity_types()).is_empty());
    assert_eq!(called!(requires, wrapper.requires()), &["probe-required"]);
    assert!(called!(note_kind_specs, wrapper.note_kind_specs()).is_empty());
    assert!(called!(note_embedding_policies, wrapper.note_embedding_policies()).is_empty());
    assert!(called!(kind_hook, wrapper.kind_hook("probe-kind")).is_none());
    called!(
        accept_channel_ingest_capability,
        wrapper.accept_channel_ingest_capability(
            ChannelIngestCapability::grant_for_direct_composition()
        )
    );
    let plan = called!(schema_plan, wrapper.schema_plan());
    assert_eq!(plan.pack, "probe");
    assert_eq!(plan.statements, &["probe ddl"]);
    assert!(called!(schema_column_additions, wrapper.schema_column_additions()).is_empty());
    assert!(called!(validation_rules, wrapper.validation_rules()).is_empty());
    called!(register_embedders, wrapper.register_embedders(&runtime));
    called!(
        register_entity_type_validator,
        wrapper.register_entity_type_validator(&runtime)
    );
    let types = [khive_types::EntityTypeDef {
        kind: khive_types::EntityKind::Document,
        type_name: "probe-type",
        aliases: &[],
    }];
    called!(
        register_entity_type_validator_with_types,
        wrapper.register_entity_type_validator_with_types(&runtime, &types)
    );
    called!(
        register_note_mutation_hook,
        wrapper.register_note_mutation_hook(&runtime)
    );
    called!(
        register_note_search_ann_provider,
        wrapper.register_note_search_ann_provider(&runtime)
    );
    called!(
        register_note_write_validator,
        wrapper.register_note_write_validator(&runtime)
    );
    called!(warm, wrapper.warm().await);
    assert_eq!(
        called!(
            registered_embedding_model_names,
            wrapper.registered_embedding_model_names()
        ),
        ["probe-model"]
    );
    assert_eq!(
        called!(mounted_namespace, wrapper.mounted_namespace()),
        Some("probe-mount")
    );
    assert_eq!(
        called!(mounted_catalog_snapshot, wrapper.mounted_catalog_snapshot())[0].digest,
        "probe digest"
    );
    assert!(
        matches!(called!(mounted_catalog, wrapper.mounted_catalog().await), Err(RuntimeError::InvalidInput(s)) if s == "probe catalog")
    );
    assert!(
        matches!(called!(dispatch_mounted, wrapper.dispatch_mounted(&mounted(), "probe.verb", json!({"argument": 7}), &registry, &token).await), Err(RuntimeError::InvalidInput(s)) if s == "probe mounted dispatch")
    );
    assert_eq!(
        called!(
            dispatch,
            wrapper
                .dispatch("probe.verb", json!({"argument": 7}), &registry, &token)
                .await
        )
        .unwrap(),
        json!({"dispatch": "probe"})
    );
}
