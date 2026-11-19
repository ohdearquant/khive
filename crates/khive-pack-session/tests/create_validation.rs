//! Public creation routes share session validation without normalizing metadata.

use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn fixture() -> (TempDir, VerbRegistry) {
    let (dir, _runtime, registry) = fixture_with_runtime();
    (dir, registry)
}

fn fixture_with_runtime() -> (TempDir, KhiveRuntime, VerbRegistry) {
    let dir = TempDir::new().expect("tempdir");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        telemetry: Default::default(),
        web: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(dir.path().join("session-validation.db")),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".into(), "session".into()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and session registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (dir, runtime, registry)
}

fn generic_args(explicit_note_kind: bool, fields: Value) -> Value {
    let mut args = if explicit_note_kind {
        json!({"kind": "note", "note_kind": "session", "content": "body"})
    } else {
        json!({"kind": "session", "content": "body"})
    };
    args.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    args
}

async fn refused(registry: &VerbRegistry, verb: &str, args: Value) -> String {
    match registry.dispatch(verb, args.clone()).await {
        Err(RuntimeError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput for {verb}({args}), got {other:?}"),
    }
}

async fn assert_note_count(registry: &VerbRegistry, expected: usize) {
    let notes = registry
        .dispatch("list", json!({"kind": "note", "limit": 100}))
        .await
        .expect("list notes");
    assert_eq!(notes["items"].as_array().unwrap().len(), expected);
}

async fn resume(registry: &VerbRegistry, id: &Value) -> Value {
    let result = registry
        .dispatch("session.resume", json!({"id": id}))
        .await
        .expect("resume created session");
    assert_eq!(result["ok"], true);
    result["session"].clone()
}

include!("create_validation_tests/invalid_session_fields_tests.rs");

include!("create_validation_tests/effective_create_tags_tests.rs");

include!("create_validation_tests/session_metadata_tests.rs");

include!("create_validation_tests/stream_and_observation_tests.rs");
