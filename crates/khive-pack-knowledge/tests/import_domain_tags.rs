use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use tempfile::TempDir;

const CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

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
        builder.with_actor_id(Some("import-tags-fixture".into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn seed(&self, slug: &str, tag: &str) -> Value {
        let response = self
            .registry
            .dispatch(
                "knowledge.upsert_atoms",
                json!({
                    "atoms": [{"slug": slug, "name": slug, "content": CONTENT, "tags": [tag]}]
                }),
            )
            .await
            .expect("create ordinary atom");
        assert_eq!(response, json!({"created": 1, "updated": 0, "total": 1}));
        let atom = self
            .registry
            .dispatch("knowledge.get", json!({"id": slug}))
            .await
            .unwrap();
        assert_eq!(atom["kind"], "atom");
        assert_eq!(atom["tags"], json!([tag]));
        atom
    }

    async fn snapshot(&self) -> Value {
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.unwrap();
        let mut snapshot = Vec::new();
        for query in [
            "SELECT * FROM knowledge_atoms ORDER BY id",
            "SELECT * FROM knowledge_domains ORDER BY id",
            "SELECT * FROM knowledge_sections ORDER BY id",
            "SELECT * FROM fts_knowledge_data ORDER BY id",
            "SELECT * FROM fts_knowledge_idx ORDER BY segid, term",
            "SELECT * FROM fts_sections_data ORDER BY id",
            "SELECT * FROM fts_sections_idx ORDER BY segid, term",
        ] {
            snapshot.push(
                serde_json::to_value(
                    reader
                        .query_all(
                            SqlStatement::new(query, vec![]).labelled("test.import_tags.snapshot"),
                        )
                        .await
                        .unwrap(),
                )
                .unwrap(),
            );
        }
        json!(snapshot)
    }

    async fn replace_tags(&self, id: &str, tags: &str) {
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.unwrap();
        assert_eq!(
            writer
                .execute(
                    SqlStatement::new(
                        "UPDATE knowledge_atoms SET tags = ?1 WHERE id = ?2",
                        vec![SqlValue::Text(tags.into()), SqlValue::Text(id.into())],
                    )
                    .labelled("test.import_tags.raw_tags")
                )
                .await
                .unwrap(),
            1
        );
    }
}

fn markdown(title: &str, canonical: bool, tag: &str) -> String {
    let identity = if canonical {
        "id: Reserved.Target\n"
    } else {
        ""
    };
    format!("---\n{identity}tags: [\"{tag}\"]\n---\n# {title}\n\nUpdated imported {CONTENT}.\n\n## Overview\n\n{CONTENT}.")
}

#[tokio::test]
async fn import_updates_ordinary_near_marker_tags_with_path_or_canonical_identity() {
    for tag in [
        "type:domain-extra",
        "prefix:type:domain",
        "TYPE:DOMAIN",
        "ordinary",
    ] {
        for canonical in [false, true] {
            let fixture = Fixture::new();
            let slug = if canonical {
                "reserved-target"
            } else {
                "z-target"
            };
            let original = fixture.seed(slug, tag).await;
            let root = TempDir::new().unwrap();
            std::fs::write(
                root.path().join("a-valid.md"),
                markdown("First import", false, "ordinary"),
            )
            .unwrap();
            std::fs::write(
                root.path().join("z-target.md"),
                markdown("Updated target", canonical, tag),
            )
            .unwrap();
            let result = fixture
                .registry
                .dispatch(
                    "knowledge.import",
                    json!({
                        "path": root.path().to_str().unwrap()
                    }),
                )
                .await
                .expect("ordinary tags do not reserve a domain identity");
            assert_eq!(result["imported_atoms"], 2);
            assert_eq!(result["imported_sections"], 2);
            let updated = fixture
                .registry
                .dispatch("knowledge.get", json!({"id": slug}))
                .await
                .unwrap();
            assert_eq!(updated["id"], original["id"]);
            assert_eq!(updated["name"], "Updated target");
            assert_ne!(updated["content"], original["content"]);
            assert!(updated["content"]
                .as_str()
                .unwrap()
                .contains("Updated imported"));
            assert_eq!(updated["tags"], json!([tag]));
            fixture
                .registry
                .dispatch("knowledge.get", json!({"id": "a-valid"}))
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn import_preflights_exact_escaped_and_legacy_markers_before_update_or_insert() {
    for tags in [
        r#"["type:domain"]"#,
        r#"["type\u003adomain"]"#,
        "broken type:domain",
        r#""type:domain""#,
        r#"{"tag":"type:domain"}"#,
        r#"["type:domain-extra",7]"#,
    ] {
        for canonical in [false, true] {
            let fixture = Fixture::new();
            let earlier = fixture.seed("a-existing", "ordinary").await;
            let slug = if canonical {
                "reserved-target"
            } else {
                "z-target"
            };
            let target = fixture.seed(slug, "ordinary").await;
            fixture
                .replace_tags(target["id"].as_str().unwrap(), tags)
                .await;
            let root = TempDir::new().unwrap();
            std::fs::write(
                root.path().join("a-existing.md"),
                markdown("Earlier update", false, "ordinary"),
            )
            .unwrap();
            std::fs::write(
                root.path().join("m-new.md"),
                markdown("Earlier insert", false, "ordinary"),
            )
            .unwrap();
            std::fs::write(
                root.path().join("z-target.md"),
                markdown("Protected target", canonical, "ordinary"),
            )
            .unwrap();
            let request = json!({"path": root.path().to_str().unwrap()});
            let before = fixture.snapshot().await;
            let writers = fixture
                .runtime
                .backend()
                .pool()
                .writer_acquisition_snapshot();
            let error = fixture
                .registry
                .dispatch("knowledge.import", request.clone())
                .await
                .expect_err("protected target must refuse before the first write");
            let expected = format!(
                "atom slug {slug:?} collides with a domain mirror; use upsert_domains instead"
            );
            assert!(
                matches!(error, RuntimeError::InvalidInput(ref message) if message == &expected),
                "{error:?}"
            );
            assert_eq!(fixture.snapshot().await, before);
            assert_eq!(
                fixture
                    .runtime
                    .backend()
                    .pool()
                    .writer_acquisition_snapshot(),
                writers
            );
            assert!(matches!(
                fixture
                    .registry
                    .dispatch("knowledge.get", json!({"id": "m-new"}))
                    .await,
                Err(RuntimeError::NotFound(_))
            ));

            // The same prepared files must be valid once the stored target is ordinary.
            fixture
                .replace_tags(target["id"].as_str().unwrap(), r#"["ordinary"]"#)
                .await;
            let accepted = fixture
                .registry
                .dispatch("knowledge.import", request)
                .await
                .unwrap();
            assert_eq!(accepted["imported_atoms"], 3);
            assert_eq!(accepted["imported_sections"], 3);
            let updated = fixture
                .registry
                .dispatch("knowledge.get", json!({"id": "a-existing"}))
                .await
                .unwrap();
            assert_eq!(updated["id"], earlier["id"]);
            assert_eq!(updated["name"], "Earlier update");
            assert_ne!(updated["content"], earlier["content"]);
            fixture
                .registry
                .dispatch("knowledge.get", json!({"id": "m-new"}))
                .await
                .unwrap();
        }
    }
}
