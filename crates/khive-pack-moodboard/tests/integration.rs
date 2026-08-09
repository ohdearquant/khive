//! Checkpoint-free pack registration and contract tests.

use khive_pack_moodboard::MoodboardPack;
use khive_runtime::{KhiveRuntime, PackRegistration, VerbRegistry, VerbRegistryBuilder};
use khive_types::{EntityKind, Pack};

fn registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(MoodboardPack::new(runtime));
    builder.build().expect("registry builds")
}

#[test]
fn pack_identity_dependencies_and_inventory_are_stable() {
    assert_eq!(MoodboardPack::NAME, "moodboard");
    assert_eq!(MoodboardPack::REQUIRES, &["kg"]);
    assert!(MoodboardPack::NOTE_KINDS.is_empty());
    assert!(MoodboardPack::ENTITY_KINDS.is_empty());
    assert!(inventory::iter::<PackRegistration>
        .into_iter()
        .any(|registration| registration.0.name() == "moodboard"));
}

#[test]
fn pack_contributes_only_additive_artifact_subtypes() {
    let types = MoodboardPack::ENTITY_TYPES;
    assert_eq!(types.len(), 3);
    assert!(types
        .iter()
        .all(|definition| definition.kind == EntityKind::Artifact));
    assert_eq!(types[0].type_name, "visual_asset");
    assert_eq!(types[1].type_name, "moodboard");
    assert_eq!(types[2].type_name, "moodboard_model");
}

#[tokio::test]
async fn registry_exposes_exact_v1_handler_names() {
    let registry = registry();
    let expected = ["moodboard.model", "moodboard.ingest", "moodboard.search"];
    assert_eq!(
        MoodboardPack::HANDLERS
            .iter()
            .map(|handler| handler.name)
            .collect::<Vec<_>>(),
        expected
    );
    for verb in expected {
        let help = registry
            .dispatch(verb, serde_json::json!({"help": true}))
            .await
            .expect("help dispatch");
        assert!(help["params"].is_array());
    }
}

#[tokio::test]
async fn ingest_rejects_missing_bytes_before_touching_optional_substrates() {
    let error = registry()
        .dispatch("moodboard.ingest", serde_json::json!({}))
        .await
        .expect_err("missing base64 must fail");
    assert!(error.to_string().contains("image_base64"));
}

#[tokio::test]
async fn search_requires_a_bare_canonical_uuid() {
    let error = registry()
        .dispatch(
            "moodboard.search",
            serde_json::json!({"asset_id": "entity:not-a-uuid"}),
        )
        .await
        .expect_err("namespaced identifier must fail");
    assert!(error.to_string().contains("UUID"));
}
