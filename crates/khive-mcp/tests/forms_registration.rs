use khive_mcp as _;
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistryBuilder};

#[test]
fn forms_is_linked_for_explicit_selection_and_exposes_its_verbs() {
    assert!(PackRegistry::discovered_names().contains(&"forms"));
    assert!(!RuntimeConfig::built_in_packs().contains(&"forms".to_string()));
    let packs = vec!["kg".to_string(), "forms".to_string()];
    PackRegistry::validate_pack_selection(&packs).expect("explicit forms selection beside kg");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: packs.clone(),
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime, &mut builder).expect("register forms");
    let registry = builder.build_metadata().expect("forms metadata");
    assert_eq!(registry.pack_names(), vec!["kg", "forms"]);
    let verbs: Vec<&str> = registry
        .pack_verbs("forms")
        .expect("forms pack")
        .iter()
        .map(|verb| verb.name)
        .collect();
    assert_eq!(
        verbs,
        vec![
            "forms.load",
            "forms.add_item",
            "forms.same",
            "forms.split",
            "forms.resolve",
            "forms.neighbourhood",
            "forms.terms",
            "forms.subjects",
            "forms.check",
            "forms.lint",
            "forms.export",
            "forms.replay",
        ]
    );
}
