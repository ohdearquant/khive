use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

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
        builder.with_actor_id(Some("delete-tags-fixture".into()));
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
            "SELECT * FROM fts_knowledge_data ORDER BY id",
            "SELECT * FROM fts_knowledge_idx ORDER BY segid, term",
        ] {
            snapshot.push(
                serde_json::to_value(
                    reader
                        .query_all(
                            SqlStatement::new(query, vec![]).labelled("test.delete_tags.snapshot"),
                        )
                        .await
                        .unwrap(),
                )
                .unwrap(),
            );
        }
        json!(snapshot)
    }

    async fn assert_refused_without_writes(&self, ids: Value, expected: &str) {
        let before = self.snapshot().await;
        let writers = self.runtime.backend().pool().writer_acquisition_snapshot();
        let error = self
            .registry
            .dispatch("knowledge.delete_atoms", json!({"ids": ids}))
            .await
            .expect_err("domain protection must refuse");
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message == expected),
            "{error:?}"
        );
        assert_eq!(self.snapshot().await, before);
        assert_eq!(
            self.runtime.backend().pool().writer_acquisition_snapshot(),
            writers
        );
    }
}

#[tokio::test]
async fn near_marker_and_ordinary_tags_allow_deletion_by_slug_and_uuid() {
    let fixture = Fixture::new();
    for (index, tag) in [
        "type:domain-extra",
        "prefix:type:domain",
        "TYPE:DOMAIN",
        "ordinary",
    ]
    .into_iter()
    .enumerate()
    {
        for by_id in [false, true] {
            let slug = format!("delete-tag-{index}-{by_id}");
            let atom = fixture.seed(&slug, tag).await;
            let id = atom["id"].as_str().unwrap();
            let key = if by_id { id } else { &slug };
            let response = fixture
                .registry
                .dispatch("knowledge.delete_atoms", json!({"ids": [key]}))
                .await
                .expect("ordinary tagged atom can be deleted");
            assert_eq!(response, json!({"deleted": 1, "requested": 1}));
            for lookup in [id, slug.as_str()] {
                assert!(matches!(
                    fixture
                        .registry
                        .dispatch("knowledge.get", json!({"id": lookup}))
                        .await,
                    Err(RuntimeError::NotFound(_))
                ));
            }
            let sql = fixture.runtime.sql();
            let mut reader = sql.reader().await.unwrap();
            let row = reader.query_row(SqlStatement::new(
                "SELECT deleted_at FROM knowledge_atoms WHERE id = ?1 AND namespace = 'local'",
                vec![SqlValue::Text(id.into())]
            ).labelled("test.delete_tags.tombstone"))
                .await.unwrap().expect("soft-deleted row remains stored");
            assert!(matches!(row.get("deleted_at"), Some(SqlValue::Integer(_))));
            drop(reader);
            assert_eq!(
                fixture
                    .registry
                    .dispatch("knowledge.delete_atoms", json!({"ids": [key]}))
                    .await
                    .unwrap(),
                json!({"deleted": 0, "requested": 1})
            );
        }
    }
}

#[tokio::test]
async fn exact_escaped_and_legacy_markers_refuse_the_whole_batch() {
    let fixture = Fixture::new();
    let sibling = fixture.seed("protected-sibling", "ordinary").await;
    for (index, tags) in [
        r#"["type:domain"]"#,
        r#"["type\u003adomain"]"#,
        "broken type:domain",
        r#""type:domain""#,
        r#"{"tag":"type:domain"}"#,
        r#"["type:domain-extra",7]"#,
    ]
    .into_iter()
    .enumerate()
    {
        let slug = format!("protected-marker-{index}");
        let atom = fixture.seed(&slug, "ordinary").await;
        let id = atom["id"].as_str().unwrap();
        let sql = fixture.runtime.sql();
        {
            let mut writer = sql.writer().await.unwrap();
            assert_eq!(
                writer
                    .execute(
                        SqlStatement::new(
                            "UPDATE knowledge_atoms SET tags = ?1 WHERE id = ?2",
                            vec![SqlValue::Text(tags.into()), SqlValue::Text(id.into())]
                        )
                        .labelled("test.delete_tags.raw_marker")
                    )
                    .await
                    .unwrap(),
                1
            );
        }
        {
            let mut reader = sql.reader().await.unwrap();
            assert!(reader
                .query_row(
                    SqlStatement::new(
                        "SELECT id FROM knowledge_domains WHERE id = ?1 OR slug = ?2",
                        vec![SqlValue::Text(id.into()), SqlValue::Text(slug.clone())]
                    )
                    .labelled("test.delete_tags.no_domain")
                )
                .await
                .unwrap()
                .is_none());
        }
        for key in [slug.as_str(), id] {
            fixture.assert_refused_without_writes(json!([sibling["id"], key]), &format!(
                "knowledge.delete_atoms cannot delete domain mirror {key:?}; use the generic delete verb by domain UUID"
            )).await;
        }
    }
}

#[tokio::test]
async fn real_domains_remain_protected_before_any_atom_is_deleted() {
    let fixture = Fixture::new();
    let sibling = fixture.seed("domain-sibling", "type:domain-extra").await;
    assert_eq!(
        fixture
            .registry
            .dispatch(
                "knowledge.upsert_domains",
                json!({"domains": [{
                    "slug": "real-domain", "name": "Real domain", "description": CONTENT
                }]})
            )
            .await
            .unwrap(),
        json!({"created": 1, "updated": 0, "total": 1})
    );
    let domain = fixture
        .registry
        .dispatch("knowledge.get", json!({"id": "real-domain"}))
        .await
        .unwrap();
    assert_eq!(domain["kind"], "domain");
    for key in ["real-domain", domain["id"].as_str().unwrap()] {
        fixture.assert_refused_without_writes(json!([sibling["id"], key]), &format!(
            "knowledge.delete_atoms cannot delete domain {key:?}; use the generic delete verb by domain UUID"
        )).await;
    }
}
