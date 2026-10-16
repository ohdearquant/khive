use khive_mcp as _;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistryBuilder, WalCeilingSource,
};

#[test]
fn charter_is_linked_for_explicit_selection_without_exposing_verbs() {
    assert!(PackRegistry::discovered_names().contains(&"charter"));
    assert!(!RuntimeConfig::built_in_packs().contains(&"charter".to_string()));
    let packs = vec!["charter".to_string()];
    PackRegistry::validate_pack_selection(&packs).expect("explicit charter selection");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        packs: packs.clone(),
        credentials: Vec::new(),
        mounts: Vec::new(),
        events_split: None,
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        visibility_receipts: None,
        blob: Default::default(),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime, &mut builder).expect("register charter");
    let registry = builder.build_metadata().expect("charter metadata");
    assert_eq!(registry.pack_names(), vec!["charter"]);
    assert!(registry
        .pack_verbs("charter")
        .expect("charter pack")
        .is_empty());
}
