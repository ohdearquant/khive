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
    async fn new() -> Self {
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
        let runtime = KhiveRuntime::new(config).expect("private memory runtime");
        assert!(runtime.config().db_path.is_none());
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("domain-marker-fixture".into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        let fixture = Self { runtime, registry };
        // Initialize event-read schema before taking no-write snapshots/counters.
        assert!(fixture.events().await.is_empty());
        fixture
    }

    async fn upsert(&self, params: Value) -> Result<Value, RuntimeError> {
        self.registry
            .dispatch("knowledge.upsert_atoms", params)
            .await
    }

    async fn seed(&self, slug: &str, tag: &str) -> Value {
        let response = self
            .upsert(json!({"atoms": [{
                "slug": slug, "name": "Original name", "content": CONTENT,
                "tags": [tag], "properties": {"original": true}, "finalized": true,
                "source_uri": "https://example.invalid/original", "source_type": "manual"
            }]}))
            .await
            .expect("valid tagged atom creation");
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

    async fn stored_atom(&self, id: &str) -> Value {
        let access = self.runtime.sql();
        let mut reader = access.reader().await.unwrap();
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE id = ?1".into(),
                params: vec![SqlValue::Text(id.into())],
                label: Some("test.domain_marker.atom".into()),
            })
            .await
            .unwrap()
            .expect("stored atom");
        Value::Object(
            row.columns
                .into_iter()
                .map(|column| (column.name, serde_json::to_value(column.value).unwrap()))
                .collect(),
        )
    }

    async fn snapshot(&self, include_events: bool) -> Value {
        let access = self.runtime.sql();
        let mut reader = access.reader().await.unwrap();
        let mut values = Vec::new();
        for query in [
            "SELECT * FROM knowledge_atoms ORDER BY id",
            "SELECT * FROM knowledge_domains ORDER BY id",
            "SELECT * FROM fts_knowledge_data ORDER BY id",
            "SELECT * FROM fts_knowledge_idx ORDER BY segid, term",
        ]
        .into_iter()
        .chain(include_events.then_some("SELECT * FROM events ORDER BY id"))
        {
            let rows = reader
                .query_all(SqlStatement {
                    sql: query.into(),
                    params: vec![],
                    label: Some("test.domain_marker.snapshot".into()),
                })
                .await
                .unwrap();
            values.push(serde_json::to_value(rows).unwrap());
        }
        json!(values)
    }

    async fn events(&self) -> Vec<Value> {
        self.registry
            .dispatch(
                "list",
                json!({
                    "kind": "event", "event_kind": "refusal", "verb": "knowledge.upsert_atoms",
                    "namespace": "local", "limit": 100
                }),
            )
            .await
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn clean_dry_run(&self, atoms: Value) {
        let before = self.snapshot(true).await;
        let writers = self.runtime.backend().pool().writer_acquisition_snapshot();
        let response = self
            .upsert(json!({"atoms": atoms, "dry_run": true}))
            .await
            .unwrap();
        assert_eq!(
            self.runtime.backend().pool().writer_acquisition_snapshot(),
            writers
        );
        assert_eq!(response["dry_run"], true);
        assert_eq!(response["would_refuse_batch"], false);
        let results = response["results"].as_array().unwrap();
        assert_eq!(results.len(), atoms.as_array().unwrap().len());
        for result in results {
            assert_eq!(result["would_refuse"], false);
            assert_eq!(result["reason"], Value::Null);
        }
        assert_eq!(self.snapshot(true).await, before);
    }
}

fn assert_unchanged_except(before: &Value, after: &Value, changed: &[&str]) {
    let mut expected = before.as_object().unwrap().clone();
    let mut actual = after.as_object().unwrap().clone();
    for field in changed {
        assert!(expected.remove(*field).is_some());
        assert!(actual.remove(*field).is_some());
    }
    assert_eq!(
        actual, expected,
        "all other stored columns must remain exact"
    );
}

#[tokio::test]
async fn ordinary_marker_substrings_allow_both_update_forms_and_read_only_dry_runs() {
    let fixture = Fixture::new().await;
    for (index, tag) in [
        "type:domain-extra",
        "prefix:type:domain",
        "ordinary",
        "TYPE:DOMAIN",
    ]
    .into_iter()
    .enumerate()
    {
        let slug = format!("ordinary-marker-{index}");
        let atom = fixture.seed(&slug, tag).await;
        let id = atom["id"].as_str().unwrap();
        let before = fixture.stored_atom(id).await;
        let content = format!("{CONTENT} revised");
        let atoms = json!([{"slug": slug, "name": "Updated name", "content": content,
            "tags": [tag], "properties": {"content_update": true}}]);
        fixture.clean_dry_run(atoms.clone()).await;
        assert_eq!(
            fixture.upsert(json!({"atoms": atoms})).await.unwrap(),
            json!({"created": 0, "updated": 1, "total": 1})
        );
        let after = fixture.stored_atom(id).await;
        assert_unchanged_except(
            &before,
            &after,
            &["name", "content", "properties", "updated_at"],
        );
        assert_eq!(
            after["name"],
            serde_json::to_value(SqlValue::Text("Updated name".into())).unwrap()
        );
        assert_eq!(
            after["content"],
            serde_json::to_value(SqlValue::Text(content)).unwrap()
        );
        assert_eq!(
            after["properties"],
            serde_json::to_value(SqlValue::Text(json!({"content_update": true}).to_string()))
                .unwrap()
        );
        let properties = json!({"property_update": index});
        let atoms = json!([{"id": id, "properties": properties}]);
        fixture.clean_dry_run(atoms.clone()).await;
        assert_eq!(
            fixture.upsert(json!({"atoms": atoms})).await.unwrap(),
            json!({"created": 0, "updated": 1, "total": 1})
        );
        let updated = fixture.stored_atom(id).await;
        assert_unchanged_except(&after, &updated, &["properties", "updated_at"]);
        assert_eq!(
            updated["properties"],
            serde_json::to_value(SqlValue::Text(properties.to_string())).unwrap()
        );
    }
    assert!(fixture.events().await.is_empty());
}

#[tokio::test]
async fn near_marker_properties_updates_preserve_short_legacy_content() {
    let fixture = Fixture::new().await;
    for (index, content) in ["", "short legacy body"].into_iter().enumerate() {
        let atom = fixture
            .seed(&format!("legacy-marker-{index}"), "type:domain-extra")
            .await;
        let id = atom["id"].as_str().unwrap();
        let access = fixture.runtime.sql();
        let mut writer = access.writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET content = ?1 WHERE id = ?2".into(),
                params: vec![SqlValue::Text(content.into()), SqlValue::Text(id.into())],
                label: Some("test.domain_marker.legacy_content".into()),
            })
            .await
            .unwrap();
        drop(writer);
        let before = fixture.stored_atom(id).await;
        let atoms = json!([{"id": id, "properties": null}]);
        fixture.clean_dry_run(atoms.clone()).await;
        assert_eq!(
            fixture.upsert(json!({"atoms": atoms})).await.unwrap(),
            json!({"created": 0, "updated": 1, "total": 1})
        );
        let after = fixture.stored_atom(id).await;
        assert_unchanged_except(&before, &after, &["properties", "updated_at"]);
        assert_eq!(
            after["properties"],
            serde_json::to_value(SqlValue::Null).unwrap()
        );
        assert_eq!(
            after["content"],
            serde_json::to_value(SqlValue::Text(content.into())).unwrap()
        );
    }
}

#[tokio::test]
async fn independent_input_refusal_records_near_marker_subjects_without_atom_writes() {
    let fixture = Fixture::new().await;
    let first = fixture.seed("refused-marker", "type:domain-extra").await;
    let sibling = fixture.seed("refused-sibling", "prefix:type:domain").await;
    let before = fixture.snapshot(false).await;
    let error = fixture
        .upsert(json!({"atoms": [
            {"slug": "refused-marker", "name": "Rejected name", "content": "too short"},
            {"id": sibling["id"], "properties": {"must_not_commit": true}}
        ]}))
        .await
        .expect_err("short content independently refuses before target checks");
    assert!(
        matches!(error.refusal_source(), RuntimeError::InvalidInput(message) if message.contains("20 words")),
        "{error:?}"
    );
    assert_eq!(fixture.snapshot(false).await, before);
    let events = fixture.events().await;
    assert_eq!(events.len(), 2);
    for (index, atom, reason) in [
        (0, &first, "validation_refused"),
        (1, &sibling, "batch_refused"),
    ] {
        let matching = events
            .iter()
            .filter(|event| event["target_id"] == atom["id"])
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        let event = matching[0];
        assert_eq!(event["namespace"], "local");
        assert_eq!(event["payload"]["subject_kind"], "knowledge_atom");
        assert_eq!(event["payload"]["item_index"], index);
        assert_eq!(event["payload"]["reason"], reason);
        assert!(!event["payload"].to_string().contains("too short"));
        if index == 1 {
            assert_eq!(event["payload"]["first_refusing_item_index"], 0);
        }
    }
}

#[tokio::test]
async fn exact_marker_and_real_domain_still_refuse_atom_updates_atomically() {
    let fixture = Fixture::new().await;
    let marker = fixture.seed("standalone-marker", "type:domain").await;
    let response = fixture
        .registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "real-domain", "name": "Real domain", "description": CONTENT
            }]}),
        )
        .await
        .unwrap();
    assert_eq!(response, json!({"created": 1, "updated": 0, "total": 1}));
    let domain = fixture
        .registry
        .dispatch("knowledge.get", json!({"id": "real-domain"}))
        .await
        .unwrap();
    assert_eq!(domain["kind"], "domain");
    {
        let access = fixture.runtime.sql();
        let mut reader = access.reader().await.unwrap();
        assert!(
            reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_domains WHERE id = ?1".into(),
                    params: vec![SqlValue::Text(marker["id"].as_str().unwrap().into())],
                    label: Some("test.domain_marker.no_domain_row".into()),
                })
                .await
                .unwrap()
                .is_none(),
            "standalone exact marker must exercise the tag guard"
        );
    }
    for (index, target) in [marker, domain].into_iter().enumerate() {
        for by_id in [false, true] {
            let protected = if by_id {
                json!({"id": target["id"], "properties": {"must_not_commit": true}})
            } else {
                json!({"slug": target["slug"], "name": "Must not replace", "content": CONTENT})
            };
            let atoms = json!([
                {"slug": format!("unwritten-prefix-{index}-{by_id}"), "name": "Valid sibling", "content": CONTENT},
                protected
            ]);
            let before = fixture.snapshot(true).await;
            let writers = fixture
                .runtime
                .backend()
                .pool()
                .writer_acquisition_snapshot();
            let dry = fixture
                .upsert(json!({"atoms": atoms, "dry_run": true}))
                .await
                .unwrap();
            assert_eq!(
                fixture
                    .runtime
                    .backend()
                    .pool()
                    .writer_acquisition_snapshot(),
                writers
            );
            assert_eq!(dry["would_refuse_batch"], true);
            assert_eq!(dry["results"][0]["would_refuse"], false);
            assert_eq!(dry["results"][1]["reason"], "invalid_input");
            assert_eq!(fixture.snapshot(true).await, before);
            let error = fixture
                .upsert(json!({"atoms": atoms}))
                .await
                .expect_err("protected mirror refuses complete batch");
            assert!(
                matches!(error.refusal_source(), RuntimeError::InvalidInput(message) if message.contains("domain")),
                "{error:?}"
            );
            assert_eq!(fixture.snapshot(true).await, before);
        }
    }
    assert!(fixture.events().await.is_empty());
}
