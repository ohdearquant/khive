use std::collections::BTreeSet;

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
        Self::with_vector_metadata(false)
    }

    fn with_vector_metadata(enabled: bool) -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: khive_runtime::Namespace::local(),
            visible_namespaces: Vec::new(),
            allowed_outbound_namespaces: Vec::new(),
            embedding_model: enabled.then_some(lattice_embed::EmbeddingModel::AllMiniLmL6V2),
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
        if enabled {
            runtime.try_register_embedder(MetadataOnlyProvider).unwrap();
        } else {
            assert!(runtime.default_embedder_name().is_empty());
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("domain-tag-reads-fixture".into()));
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

    async fn execute(&self, query: &str, params: Vec<SqlValue>) -> u64 {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement::new(query, params).labelled("test.domain_tag_reads.fixture"))
            .await
            .unwrap()
    }

    async fn list(&self, params: Value) -> Value {
        self.registry
            .dispatch("knowledge.list", params)
            .await
            .unwrap()
    }

    async fn stats(&self) -> Value {
        self.registry
            .dispatch("knowledge.stats", json!({}))
            .await
            .unwrap()
    }

    async fn walk(&self, mut params: Value) -> Vec<String> {
        let mut ids = Vec::new();
        for _ in 0..32 {
            let page = self.list(params.clone()).await;
            assert_eq!(page["order"], "created_at_asc_id_asc");
            assert!(page.get("total").is_none());
            for row in page["results"].as_array().unwrap() {
                assert_eq!(row.as_object().unwrap().len(), 2);
                let id = row["id"].as_str().unwrap().to_owned();
                assert!(!ids.contains(&id), "cursor repeated {id}");
                ids.push(id);
            }
            match page["next_after"].as_str() {
                Some(next) => params["after"] = json!(next),
                None => return ids,
            }
        }
        panic!("bounded cursor walk did not terminate");
    }
}

fn ids(response: &Value) -> BTreeSet<String> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

fn cursor(after: &str) -> Value {
    json!({"type": "atom", "after": after, "limit": 1, "fields": ["id", "slug"]})
}

#[tokio::test]
async fn imported_near_markers_survive_reads_and_a_deleted_cursor_boundary() {
    let f = Fixture::new();
    let root = TempDir::new().unwrap();
    let cases = [
        ("near-suffix", "type:domain-extra"),
        ("near-prefix", "prefix:type:domain"),
        ("near-case", "TYPE:DOMAIN"),
        ("near-space", " type:domain "),
        ("ordinary", "ordinary"),
    ];
    for (slug, tag) in cases {
        std::fs::write(
            root.path().join(format!("{slug}.md")),
            format!(
                "---\ntags: [\"{tag}\"]\n---\n# {slug}\n\n{CONTENT}.\n\n## Overview\n\n{CONTENT}."
            ),
        )
        .unwrap();
    }
    let imported = f
        .registry
        .dispatch(
            "knowledge.import",
            json!({
                "path": root.path().to_str().unwrap()
            }),
        )
        .await
        .unwrap();
    assert_eq!(imported["imported_atoms"], cases.len());
    assert_eq!(imported["imported_sections"], cases.len());
    let mut expected = BTreeSet::new();
    let mut boundary = String::new();
    for (index, (slug, tag)) in cases.iter().enumerate() {
        let atom = f
            .registry
            .dispatch("knowledge.get", json!({"id": slug}))
            .await
            .unwrap();
        assert_eq!(atom["kind"], "atom");
        assert_eq!(atom["tags"], json!([tag]));
        let id = atom["id"].as_str().unwrap().to_owned();
        // The first near-marker atom must be the first issued cursor, independent of UUIDs.
        assert_eq!(
            f.execute(
                "UPDATE knowledge_atoms SET created_at = ?1, finalized = ?2 WHERE id = ?3",
                vec![
                    SqlValue::Integer(100 + index as i64),
                    SqlValue::Integer(i64::from(index == 0)),
                    SqlValue::Text(id.clone())
                ]
            )
            .await,
            1
        );
        if index == 0 {
            boundary = id.clone();
        }
        expected.insert(id);
    }
    f.registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "true-domain", "name": "True domain", "description": CONTENT
            }]}),
        )
        .await
        .unwrap();
    let domain = f
        .registry
        .dispatch("knowledge.get", json!({"id": "true-domain"}))
        .await
        .unwrap();
    assert_eq!(domain["kind"], "domain");
    f.execute(
        "UPDATE knowledge_atoms SET finalized = 1 WHERE slug = 'true-domain'",
        vec![],
    )
    .await;
    let listed = f.list(json!({"type": "atom", "limit": 100})).await;
    assert_eq!(ids(&listed), expected);
    assert_eq!(listed["total"], 5);
    let stats = f.stats().await;
    assert_eq!(stats["total_atoms"], 5);
    assert_eq!(stats["total_domains"], 1);
    assert!((stats["eval_coverage"].as_f64().unwrap() - 0.2).abs() < 1e-12);
    let first = f.list(cursor("")).await;
    assert_eq!(first["results"][0]["id"], boundary);
    assert_eq!(first["next_after"], boundary);
    assert_eq!(
        f.walk(cursor(""))
            .await
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected
    );
    let refused = f
        .registry
        .dispatch("knowledge.delete_atoms", json!({"ids": [domain["id"]]}))
        .await
        .unwrap_err();
    assert!(matches!(refused, RuntimeError::InvalidInput(_)));
    assert_eq!(
        f.registry
            .dispatch("knowledge.delete_atoms", json!({"ids": [boundary]}))
            .await
            .unwrap(),
        json!({"deleted": 1, "requested": 1})
    );
    assert!(matches!(
        f.registry
            .dispatch("knowledge.get", json!({"id": boundary}))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
    expected.remove(&boundary);
    assert_eq!(
        f.walk(cursor(&boundary))
            .await
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected
    );
    let listed = f.list(json!({"type": "atom", "limit": 100})).await;
    assert_eq!(ids(&listed), expected);
    assert_eq!(listed["total"], 4);
    assert_eq!(f.stats().await["total_atoms"], 4);
    assert_eq!(f.stats().await["total_domains"], 1);
}

#[tokio::test]
async fn list_stats_cursor_and_search_share_decoded_and_legacy_tag_classification() {
    let f = Fixture::new();
    let cases = [
        ("exact", r#"["type:domain"]"#, true),
        ("escaped", r#"["type\u003adomain"]"#, true),
        ("legacy", "broken type:domain", true),
        ("scalar", r#""type:domain""#, true),
        ("object", r#"{"tag":"type:domain"}"#, true),
        ("mixed", r#"["type:domain-extra",7]"#, true),
        ("mixed-escaped", r#"["type\u003adomain",7]"#, false),
        ("suffix", r#"["type:domain-extra"]"#, false),
        ("prefix", r#"["prefix:type:domain"]"#, false),
        ("case", r#"["TYPE:DOMAIN"]"#, false),
        ("space", r#"[" type:domain "]"#, false),
        ("plain", r#"["ordinary"]"#, false),
        ("empty", "", false),
    ];
    let mut atoms = BTreeSet::new();
    let mut mirrors = BTreeSet::new();
    for (slug, tags, mirror) in cases {
        let atom = f.seed(slug, "ordinary").await;
        let id = atom["id"].as_str().unwrap().to_owned();
        assert_eq!(f.execute("UPDATE knowledge_atoms SET tags = ?1, status = 'reviewed', finalized = 1 WHERE id = ?2", vec![SqlValue::Text(tags.into()), SqlValue::Text(id.clone())]).await, 1);
        if mirror {
            mirrors.insert(id);
        } else {
            atoms.insert(id);
        }
    }
    let listed = f.list(json!({"type": "atom", "limit": 100})).await;
    assert_eq!(ids(&listed), atoms);
    assert_eq!(listed["total"], atoms.len());
    assert_eq!(
        f.walk(cursor(""))
            .await
            .into_iter()
            .collect::<BTreeSet<_>>(),
        atoms
    );
    let stats = f.stats().await;
    assert_eq!(stats["total_atoms"], atoms.len());
    assert_eq!(stats["total_domains"], 0); // Raw mirrors do not manufacture domain rows.
    assert_eq!(stats["eval_coverage"], 1.0);
    for id in &mirrors {
        assert!(matches!(
            f.registry.dispatch("knowledge.list", cursor(id)).await,
            Err(RuntimeError::NotFound(_))
        ));
    }
    for (kind, expected) in [("atom", &atoms), ("domain", &mirrors)] {
        let searched = f.registry.dispatch("knowledge.search", json!({
            "query": "gradient", "kind": kind, "limit": 100, "min_score": 0.0, "rerank": false
        })).await.unwrap();
        assert_eq!(ids(&searched), *expected, "public search kind={kind}");
        for row in searched["results"].as_array().unwrap() {
            assert_eq!(row["kind"], kind);
            assert!(row["score"].as_f64().unwrap().is_finite());
        }
    }
}

#[tokio::test]
async fn cursor_tag_filter_preserves_namespace_status_and_tombstones() {
    let f = Fixture::new();
    let near = f.seed("near-reviewed", "prefix:type:domain").await;
    let ordinary = f.seed("plain-reviewed", "ordinary").await;
    let draft = f.seed("near-draft", "type:domain-extra").await;
    let deleted = f.seed("near-deleted", "type:domain-extra").await;
    let mirror = f.seed("mirror-reviewed", "ordinary").await;
    f.registry.dispatch("knowledge.upsert_atoms", json!({"namespace":"foreign", "atoms":[{
        "slug":"foreign-near", "name":"Foreign near", "content":CONTENT, "tags":["type:domain-extra"]
    }]})).await.unwrap();
    let foreign = f
        .registry
        .dispatch(
            "knowledge.get",
            json!({"namespace":"foreign", "id":"foreign-near"}),
        )
        .await
        .unwrap();
    f.execute(
        "UPDATE knowledge_atoms SET status = 'reviewed', created_at = 100",
        vec![],
    )
    .await;
    f.execute(
        "UPDATE knowledge_atoms SET status = 'draft' WHERE id = ?1",
        vec![SqlValue::Text(draft["id"].as_str().unwrap().into())],
    )
    .await;
    f.execute(
        "UPDATE knowledge_atoms SET tags = '[\"type:domain\"]' WHERE id = ?1",
        vec![SqlValue::Text(mirror["id"].as_str().unwrap().into())],
    )
    .await;
    f.registry
        .dispatch("knowledge.delete_atoms", json!({"ids":[deleted["id"]]}))
        .await
        .unwrap();
    let mut expected = vec![
        near["id"].as_str().unwrap().to_owned(),
        ordinary["id"].as_str().unwrap().to_owned(),
    ];
    expected.sort(); // Identical timestamps make the UUID tiebreak load-bearing.
    let mut query = cursor("");
    query["status"] = json!(["reviewed"]);
    assert_eq!(f.walk(query.clone()).await, expected);
    let mut excluded = cursor("");
    excluded["exclude_status"] = json!("draft");
    assert_eq!(f.walk(excluded).await, expected);
    let first = f.list(query.clone()).await;
    assert_eq!(first["next_after"], expected[0]);
    f.registry
        .dispatch("knowledge.delete_atoms", json!({"ids":[expected[0]]}))
        .await
        .unwrap();
    query["after"] = first["next_after"].clone();
    assert_eq!(f.walk(query).await, expected[1..]);
    assert!(matches!(
        f.registry
            .dispatch("knowledge.list", cursor(foreign["id"].as_str().unwrap()))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
    let mut foreign_query = cursor("");
    foreign_query["namespace"] = json!("foreign");
    assert_eq!(
        f.walk(foreign_query).await,
        vec![foreign["id"].as_str().unwrap()]
    );
}

// Coverage reads already stored vectors; any attempt to instantiate a model is a
// fixture failure. The provider supplies only the matching built-in dimensions.
struct MetadataOnlyProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for MetadataOnlyProvider {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }
    fn dimensions(&self) -> usize {
        384
    }
    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        panic!("coverage must not construct an embedding model")
    }
}

#[tokio::test]
async fn embedding_coverage_uses_the_same_atom_population_as_list_and_stats() {
    let f = Fixture::with_vector_metadata(true);
    let near = f.seed("covered-near", "type:domain-extra").await;
    let upper = f.seed("covered-case", "TYPE:DOMAIN").await;
    f.seed("uncovered", "ordinary").await;
    let escaped = f.seed("excluded-escaped", "ordinary").await;
    let legacy = f.seed("excluded-legacy", "ordinary").await;
    for (atom, tags) in [
        (&escaped, r#"["type\u003adomain"]"#),
        (&legacy, "broken type:domain"),
    ] {
        assert_eq!(
            f.execute(
                "UPDATE knowledge_atoms SET tags = ?1 WHERE id = ?2",
                vec![
                    SqlValue::Text(tags.into()),
                    SqlValue::Text(atom["id"].as_str().unwrap().into())
                ]
            )
            .await,
            1
        );
    }
    let token = f
        .runtime
        .authorize(khive_types::Namespace::local())
        .unwrap();
    let vectors = f.runtime.vectors(&token).unwrap();
    for atom in [&near, &upper, &escaped, &legacy] {
        vectors
            .insert(
                uuid::Uuid::parse_str(atom["id"].as_str().unwrap()).unwrap(),
                khive_types::SubstrateKind::Entity,
                "local",
                "knowledge.atom",
                vec![vec![0.0; 384]],
            )
            .await
            .unwrap();
    }
    let stats = f.stats().await;
    assert_eq!(stats["total_atoms"], 3);
    assert_eq!(f.list(json!({"type":"atom"})).await["total"], 3);
    assert!((stats["embedding_coverage"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-12);
}
