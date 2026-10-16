use khive_runtime::pack::PackRegistry;
use khive_runtime::{KhiveRuntime, RuntimeConfig, StorageBackend, VerbRegistryBuilder};
use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};
use kkernel as _;
use std::sync::Arc;

#[test]
fn default_pack_rules_retain_gtd_ownership_and_flattened_rule_order() {
    let names = RuntimeConfig::built_in_packs();
    let config = RuntimeConfig {
        packs: names.clone(),
        brain_profile: None,
        actor_id: None,
        events_split: None,
        mounts: Vec::new(),
        ..RuntimeConfig::no_embeddings().for_metadata_registry()
    };
    let runtime = KhiveRuntime::from_backend(
        Arc::new(StorageBackend::memory().expect("private memory backend")),
        config,
    );
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&names, runtime, &mut builder).expect("default packs");
    let registry = builder.build_metadata().expect("default pack metadata");
    let attributed = registry.all_edge_rules_with_packs();
    let flat = registry.all_edge_rules();
    assert_eq!(attributed.len(), flat.len());
    assert_eq!(
        attributed.iter().map(|(_, rule)| *rule).collect::<Vec<_>>(),
        flat
    );
    assert!(attributed.contains(&(
        "gtd",
        EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source: EndpointKind::NoteOfKind("task"),
            target: EndpointKind::NoteOfKind("task"),
        }
    )));
    assert!(attributed
        .iter()
        .all(|(owner, _)| names.iter().any(|name| name.as_str() == *owner)));
}
