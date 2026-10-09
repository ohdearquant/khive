use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use tempfile::TempDir;

const ATOM_CONTENT: &str = "This atom has enough ordinary words to satisfy the existing content minimum while the section length tests independently exercise Unicode character boundaries and preserved batch validation.";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: khive_runtime::Namespace::local(),
            visible_namespaces: Vec::new(),
            allowed_outbound_namespaces: Vec::new(),
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            credentials: Vec::new(),
            visibility_receipts: None,
            packs: vec!["kg".into(), "knowledge".into()],
            actor_id: None,
            brain_profile: None,
            brain: Default::default(),
            blob: Default::default(),
            mounts: Vec::new(),
            events_split: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private memory runtime");
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("section-content-length-fixture".into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }
}

fn character_cases() -> Vec<(String, usize)> {
    vec![
        ("a".repeat(79), 79),
        ("a".repeat(80), 80),
        ("a".repeat(81), 81),
        ("界".repeat(27), 27),
        ("界".repeat(79), 79),
        ("界".repeat(80), 80),
        ("界".repeat(81), 81),
        ("🦀".repeat(20), 20),
        ("🦀".repeat(79), 79),
        ("🦀".repeat(80), 80),
        ("🦀".repeat(81), 81),
        (format!("{}界", "a".repeat(78)), 79),
        (format!("{}界", "a".repeat(79)), 80),
    ]
}

#[tokio::test]
async fn edit_counts_characters_and_refuses_short_suffix_before_any_write() {
    let f = Fixture::new();
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [{"slug": "character-boundary", "name": "Character boundary", "content": ATOM_CONTENT}]}),
    ).await.expect("seed atom");
    f.dispatch(
        "knowledge.edit",
        json!({"id": "character-boundary", "sections": [{
            "section_type": "overview", "heading": "Original heading", "content": ATOM_CONTENT
        }]}),
    )
    .await
    .expect("seed section");

    for (content, characters) in character_cases() {
        let args = json!({"id": "character-boundary", "sections": [
            {"section_type": "overview", "heading": "Changed heading", "content": ATOM_CONTENT},
            {"section_type": "examples", "content": content}
        ]});
        let get = json!({"id": "character-boundary", "include_sections": true});
        let before = f.dispatch("knowledge.get", get.clone()).await.unwrap();
        let writers = f.runtime.backend().pool().writer_acquisition_snapshot();
        let result = f.dispatch("knowledge.edit", args).await;
        if characters < 80 {
            let error = result.expect_err("short Unicode section must refuse");
            let expected =
                format!("section content must be at least 80 characters (got {characters})");
            assert!(
                matches!(&error, RuntimeError::InvalidInput(message) if message == &expected),
                "{error:?}"
            );
            assert_eq!(
                f.runtime.backend().pool().writer_acquisition_snapshot(),
                writers
            );
            assert_eq!(f.dispatch("knowledge.get", get).await.unwrap(), before);
        } else {
            assert_eq!(result.expect("80 or more characters")["upserted"], 2);
            let after = f.dispatch("knowledge.get", get).await.unwrap();
            assert!(after["sections"]
                .as_array()
                .unwrap()
                .iter()
                .any(|section| section["content"] == content));
        }
    }
}

#[tokio::test]
async fn import_skips_short_unicode_sections_but_keeps_boundary_and_longer_sections() {
    let f = Fixture::new();
    let dir = TempDir::new().unwrap();
    for (index, (content, characters)) in character_cases().into_iter().enumerate() {
        let slug = format!("character-{index}");
        let path = dir.path().join(format!("{slug}.md"));
        std::fs::write(
            &path,
            format!(
                "# Character boundary

{ATOM_CONTENT}

## Overview
{content}"
            ),
        )
        .unwrap();
        let result = f
            .dispatch("knowledge.import", json!({"path": path.to_str().unwrap()}))
            .await
            .expect("import retains its skip policy");
        assert_eq!(result["imported_atoms"], 1);
        assert_eq!(result["sections_discovered"], 1);
        assert_eq!(result["sections_skipped"], usize::from(characters < 80));
        assert_eq!(result["imported_sections"], usize::from(characters >= 80));
        let atom = f
            .dispatch(
                "knowledge.get",
                json!({"id": slug, "include_sections": true}),
            )
            .await
            .unwrap();
        let sections = atom["sections"].as_array().unwrap();
        if characters < 80 {
            assert!(sections.is_empty());
        } else {
            assert_eq!(sections.len(), 1);
            assert_eq!(sections[0]["content"], content);
        }
    }
}
