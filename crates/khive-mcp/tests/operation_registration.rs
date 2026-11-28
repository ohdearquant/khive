use khive_mcp as _;
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistryBuilder};

#[test]
fn operation_is_linked_for_explicit_selection_and_exposes_its_verbs() {
    assert!(PackRegistry::discovered_names().contains(&"operation"));
    assert!(!RuntimeConfig::built_in_packs().contains(&"operation".to_string()));
    let packs = vec!["operation".to_string()];
    PackRegistry::validate_pack_selection(&packs).expect("explicit operation selection");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: packs.clone(),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime, &mut builder).expect("register operation");
    let registry = builder.build_metadata().expect("operation metadata");
    assert_eq!(registry.pack_names(), vec!["operation"]);
    let verbs: Vec<&str> = registry
        .pack_verbs("operation")
        .expect("operation pack")
        .iter()
        .map(|verb| verb.name)
        .collect();
    assert_eq!(
        verbs,
        vec![
            "operation.define",
            "operation.open",
            "operation.bind",
            "operation.record",
            "operation.frontier",
        ]
    );
}
