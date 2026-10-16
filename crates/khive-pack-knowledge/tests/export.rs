use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let config = RuntimeConfig {
            db_path: None,
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
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(runtime.backend());
        Self { runtime, registry }
    }

    async fn atom(&self, namespace: &str, slug: &str) -> String {
        self.registry.dispatch("knowledge.upsert_atoms", json!({
            "namespace": namespace,
            "atoms": [{"slug": slug, "name": slug, "content": CONTENT,
                "tags": ["type:domain-extra", "α"], "properties": {"nested": {"z": 2, "a": 1}}}]
        })).await.unwrap();
        let result = self
            .registry
            .dispatch(
                "knowledge.get",
                json!({
                    "namespace": namespace, "id": slug,
                }),
            )
            .await
            .unwrap();
        result["id"].as_str().unwrap().to_owned()
    }

    async fn domain(&self, namespace: &str, slug: &str, member: &str) -> String {
        self.registry
            .dispatch(
                "knowledge.upsert_domains",
                json!({
                    "namespace": namespace,
                    "domains": [{"slug": slug, "name": slug, "description": CONTENT,
                        "members": [member], "tags": ["domain"]}]
                }),
            )
            .await
            .unwrap();
        let result = self
            .registry
            .dispatch(
                "knowledge.get",
                json!({
                    "namespace": namespace, "id": slug,
                }),
            )
            .await
            .unwrap();
        result["id"].as_str().unwrap().to_owned()
    }

    async fn execute(&self, sql: &str, params: Vec<&str>) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: sql.into(),
                params: params
                    .into_iter()
                    .map(|s| SqlValue::Text(s.to_owned()))
                    .collect(),
                label: Some("test.knowledge_export.seed".into()),
            })
            .await
            .unwrap();
    }

    async fn section(&self, id: &str, atom: &str, namespace: &str, section_type: &str) {
        self.execute(
            "INSERT INTO knowledge_sections \
             (id, atom_id, namespace, section_type, heading, content, content_hash, tokens, sort_order, status, embedding, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'A heading', ?5, ?1, 27, 9, 'disputed', X'00000000', 100, 200)",
            vec![id, atom, namespace, section_type, CONTENT],
        ).await;
    }

    async fn export(&self, namespace: Option<&str>) -> Value {
        let params = namespace.map_or_else(|| json!({}), |ns| json!({"namespace": ns}));
        self.registry
            .dispatch("knowledge.export", params)
            .await
            .unwrap()
    }

    async fn corpus(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut tables = Vec::new();
        for sql in [
            "SELECT * FROM knowledge_atoms ORDER BY id",
            "SELECT * FROM knowledge_domains ORDER BY id",
            "SELECT * FROM knowledge_sections ORDER BY id",
        ] {
            tables.push(
                serde_json::to_value(
                    reader
                        .query_all(SqlStatement {
                            sql: sql.into(),
                            params: vec![],
                            label: Some("test.knowledge_export.snapshot".into()),
                        })
                        .await
                        .unwrap(),
                )
                .unwrap(),
            );
        }
        json!(tables)
    }
}

fn records(export: &Value) -> Vec<Value> {
    export["data"]
        .as_str()
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn export_is_byte_stable_complete_and_does_not_change_the_corpus() {
    let f = Fixture::new();
    let first = f.atom("local", "first").await;
    let second = f.atom("local", "second").await;
    let domain = f.domain("local", "collection", "first").await;
    let section = "00000000-0000-4000-8000-000000000001";
    f.section(section, &first, "local", "overview").await;
    let properties = r#"{"z":[{"z":0,"a":1},false,null],"a":{"z":2,"a":9007199254740993}}"#;
    f.execute("UPDATE knowledge_atoms SET properties = ?1, content = ?2, finalized = 1, source_uri = 'https://example.invalid/source', source_type = 'manual' WHERE id = ?3", vec![
        properties, "  preserved α\n\"quoted\"\n", first.as_str(),
    ]).await;
    f.execute(
        "UPDATE knowledge_atoms SET properties = NULL WHERE id = ?1",
        vec![second.as_str()],
    )
    .await;
    let before = f.corpus().await;
    let first_export = f.export(None).await;
    assert_eq!(first_export, f.export(Some("local")).await);
    assert_eq!(
        first_export["counts"],
        json!({"atoms": 3, "domains": 1, "sections": 1})
    );
    assert_eq!(first_export["format"], "jsonl");
    assert_eq!(first_export["namespace"], "local");
    assert_eq!(before, f.corpus().await);

    let rows = records(&first_export);
    let mut expected = vec![
        (first.as_str(), "atom"),
        (second.as_str(), "atom"),
        (domain.as_str(), "atom"),
        (domain.as_str(), "domain"),
        (section, "section"),
    ];
    expected.sort_unstable();
    assert_eq!(
        rows.iter()
            .map(|row| (row["id"].as_str().unwrap(), row["type"].as_str().unwrap(),))
            .collect::<Vec<_>>(),
        expected
    );
    let atom = rows.iter().find(|row| row["id"] == first).unwrap();
    assert_eq!(atom["content"], "  preserved α\n\"quoted\"\n");
    assert_eq!(atom["tags"], json!(["type:domain-extra", "α"]));
    assert_eq!(
        atom["properties"],
        serde_json::from_str::<Value>(properties).unwrap()
    );
    assert_eq!(atom["finalized"], true);
    assert_eq!(atom["source_uri"], "https://example.invalid/source");
    assert_eq!(atom["source_type"], "manual");
    let second_atom = rows.iter().find(|row| row["id"] == second).unwrap();
    assert_eq!(second_atom.get("properties"), Some(&Value::Null));
    assert_eq!(second_atom["finalized"], false);
    assert!(atom["created_at"].is_i64());
    assert_eq!(atom.get("deleted_at"), Some(&Value::Null));
    assert_eq!(
        rows.iter().find(|row| row["type"] == "domain").unwrap()["members"],
        json!(["first"])
    );
    let exported_section = rows.iter().find(|row| row["type"] == "section").unwrap();
    assert_eq!(exported_section["atom_id"], first);
    assert_eq!(exported_section["sort_order"], 9);
    assert!(exported_section.get("embedding").is_none());
    assert!(first_export["data"].as_str().unwrap().contains(
        r#""properties":{"a":{"a":9007199254740993,"z":2},"z":[{"a":1,"z":0},false,null]}"#,
    ));

    // Reordering stored object keys changes neither values nor export bytes.
    f.execute(
        "UPDATE knowledge_atoms SET properties = ?1 WHERE id = ?2",
        vec![
            r#"{"a":{"a":9007199254740993,"z":2},"z":[{"a":1,"z":0},false,null]}"#,
            first.as_str(),
        ],
    )
    .await;
    assert_eq!(first_export, f.export(None).await);
}

#[tokio::test]
async fn export_selects_one_namespace_and_keeps_live_lifecycle_and_retired_rows() {
    let f = Fixture::new();
    let draft = f.atom("local", "draft").await;
    let deprecated = f.atom("local", "deprecated").await;
    let deleted = f.atom("local", "deleted").await;
    let foreign = f.atom("foreign", "foreign").await;
    let deleted_domain = f.domain("local", "deleted-domain", "draft").await;
    let foreign_domain = f.domain("foreign", "foreign-domain", "foreign").await;
    f.execute(
        "UPDATE knowledge_atoms SET status = 'deprecated' WHERE id = ?1",
        vec![deprecated.as_str()],
    )
    .await;
    f.execute(
        "UPDATE knowledge_atoms SET deleted_at = 300 WHERE id IN (?1, ?2)",
        vec![deleted.as_str(), deleted_domain.as_str()],
    )
    .await;
    f.execute(
        "UPDATE knowledge_domains SET deleted_at = 300 WHERE id = ?1",
        vec![deleted_domain.as_str()],
    )
    .await;
    let retired = "00000000-0000-4000-8000-000000000010";
    let ordinary = "00000000-0000-4000-8000-000000000011";
    f.section(retired, &draft, "local", " See Also ").await;
    f.section(ordinary, &deprecated, "local", "overview").await;
    f.section(
        "00000000-0000-4000-8000-000000000012",
        &deleted,
        "local",
        "overview",
    )
    .await;
    f.section(
        "00000000-0000-4000-8000-000000000013",
        &foreign,
        "foreign",
        "overview",
    )
    .await;
    // A corrupt namespace/parent association must not leak a foreign section.
    f.section(
        "00000000-0000-4000-8000-000000000014",
        &draft,
        "foreign",
        "overview",
    )
    .await;
    let before = f.corpus().await;
    let result = f.export(None).await;
    assert_eq!(
        result["counts"],
        json!({"atoms": 2, "domains": 0, "sections": 2})
    );
    let rows = records(&result);
    assert!(rows.iter().all(|row| row["namespace"] == "local"));
    assert_eq!(
        rows.iter().find(|row| row["id"] == draft).unwrap()["status"],
        "draft"
    );
    assert_eq!(
        rows.iter().find(|row| row["id"] == deprecated).unwrap()["status"],
        "deprecated"
    );
    let retired_row = rows.iter().find(|row| row["id"] == retired).unwrap();
    assert_eq!(retired_row["section_type"], " See Also ");
    assert_eq!(retired_row["retired"], true);
    assert_eq!(retired_row["status"], "disputed");
    assert!(rows
        .iter()
        .find(|row| row["id"] == ordinary)
        .unwrap()
        .get("retired")
        .is_none());
    let foreign_result = f.export(Some("foreign")).await;
    assert_eq!(
        foreign_result["counts"],
        json!({"atoms": 2, "domains": 1, "sections": 1})
    );
    assert!(records(&foreign_result)
        .iter()
        .all(|row| row["namespace"] == "foreign"));
    assert!(records(&foreign_result)
        .iter()
        .any(|row| row["id"] == foreign_domain && row["type"] == "domain"));
    assert_eq!(before, f.corpus().await);
}

#[tokio::test]
async fn empty_export_and_invalid_options_have_explicit_results() {
    let f = Fixture::new();
    assert_eq!(
        f.export(None).await,
        json!({
            "format": "jsonl", "namespace": "local", "data": "",
            "counts": {"atoms": 0, "domains": 0, "sections": 0},
        })
    );
    assert_eq!(f.export(Some("empty")).await["data"], "");
    for params in [
        json!({"format": "json"}),
        json!({"format": null}),
        json!({"format": 1}),
        json!({"path": "/tmp/not-written.jsonl"}),
    ] {
        assert!(f
            .registry
            .dispatch("knowledge.export", params)
            .await
            .is_err());
    }
    let help = f
        .registry
        .dispatch("knowledge.export", json!({"help": true}))
        .await
        .unwrap();
    assert_eq!(help["verb"], "knowledge.export");
}

#[tokio::test]
async fn corrupt_json_refuses_the_dump_instead_of_silently_dropping_data() {
    let f = Fixture::new();
    let id = f.atom("local", "malformed").await;
    f.execute(
        "UPDATE knowledge_atoms SET properties = '{' WHERE id = ?1",
        vec![id.as_str()],
    )
    .await;
    let before = f.corpus().await;
    assert!(f
        .registry
        .dispatch("knowledge.export", json!({}))
        .await
        .is_err());
    assert_eq!(before, f.corpus().await);
}
