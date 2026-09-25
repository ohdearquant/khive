use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
const SECRET: &str = "alice:dryrun-fixture-password";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        Self::with_dispatch_audit(false)
    }

    fn with_dispatch_audit(audit: bool) -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            events_split: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("dry-run-fixture".into()));
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new(runtime.clone()));
        if audit {
            builder.with_runtime_event_store(&runtime).unwrap();
        }
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn upsert(&self, params: Value) -> Result<Value, RuntimeError> {
        self.registry
            .dispatch("knowledge.upsert_atoms", params)
            .await
    }

    async fn seed(&self, slug: &str) -> Value {
        self.upsert(json!({"atoms":[atom(slug, CONTENT)]}))
            .await
            .unwrap();
        self.registry
            .dispatch("knowledge.get", json!({"id":slug}))
            .await
            .unwrap()
    }

    async fn snapshot(&self) -> Value {
        self.snapshot_including_audit(true).await
    }

    async fn snapshot_including_audit(&self, include_audit: bool) -> Value {
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.unwrap();
        let mut snapshot = Vec::new();
        for query in [
            "SELECT * FROM knowledge_atoms ORDER BY id",
            "SELECT * FROM knowledge_domains ORDER BY id",
            "SELECT * FROM fts_knowledge_data ORDER BY id",
            "SELECT * FROM fts_knowledge_idx ORDER BY segid, term",
            if include_audit {
                "SELECT * FROM events ORDER BY id"
            } else {
                "SELECT * FROM events WHERE kind != 'audit' ORDER BY id"
            },
        ] {
            let rows = reader
                .query_all(SqlStatement {
                    sql: query.into(),
                    params: vec![],
                    label: Some("test.atom_dry_run.snapshot".into()),
                })
                .await
                .unwrap();
            snapshot.push(serde_json::to_value(rows).unwrap());
        }
        json!(snapshot)
    }

    async fn audit_payloads(&self, outcome: &str) -> Value {
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.unwrap();
        let rows = reader.query_all(SqlStatement {
            sql: "SELECT payload FROM events WHERE kind = 'audit' AND verb = 'knowledge.upsert_atoms' AND outcome = ?1 ORDER BY id".into(),
            params: vec![SqlValue::Text(outcome.to_owned())],
            label: Some("test.atom_dry_run.audit_payloads".into()),
        }).await.unwrap();
        serde_json::to_value(rows).unwrap()
    }

    async fn refusals(&self) -> Vec<Value> {
        self.registry
            .dispatch(
                "list",
                json!({
                    "kind":"event","event_kind":"refusal","verb":"knowledge.upsert_atoms",
                    "namespace":"local","limit":100
                }),
            )
            .await
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .clone()
    }
}

fn atom(slug: &str, content: &str) -> Value {
    json!({"slug":slug,"name":"Safe atom name","content":content})
}

fn mixed() -> Vec<Value> {
    vec![
        atom("clean-new", CONTENT),
        atom(
            "secret-existing",
            &format!("{CONTENT} https://{SECRET}@example.invalid/path"),
        ),
        atom("invalid-new", "too short"),
    ]
}

#[tokio::test]
async fn dry_run_mixed_batch_returns_every_verdict_without_store_or_event_changes() {
    let f = Fixture::new();
    let old = f.seed("secret-existing").await;
    let before = f.snapshot().await;
    let events = f.refusals().await;
    let writers = f.runtime.backend().pool().writer_acquisition_snapshot();
    let result = f.upsert(json!({"atoms":mixed(),"dry_run":true})).await;
    // A subsequent event-list call constructs its store through a writer-backed
    // schema check, so measure the dry run before making that independent read.
    assert_eq!(
        f.runtime.backend().pool().writer_acquisition_snapshot(),
        writers,
        "DRY_RUN_NO_WRITER"
    );
    assert_eq!(f.refusals().await, events, "DRY_RUN_NO_REFUSAL_EVENTS");
    assert_eq!(f.snapshot().await, before, "DRY_RUN_STORE_UNCHANGED");
    let result = result.unwrap();
    assert_eq!(result["would_refuse_batch"], true);
    let verdicts = result["results"].as_array().unwrap();
    assert_eq!(verdicts.len(), 3, "DRY_RUN_WHOLE_BATCH");
    for (index, slug) in ["clean-new", "secret-existing", "invalid-new"]
        .iter()
        .enumerate()
    {
        assert_eq!(verdicts[index]["index"], index);
        assert_eq!(verdicts[index]["slug"], *slug);
        assert_eq!(verdicts[index]["identity_masked"], false);
    }
    assert_eq!(verdicts[0]["would_refuse"], false);
    assert_eq!(verdicts[1]["would_refuse"], true, "DRY_RUN_SECRET_VERDICT");
    assert_eq!(verdicts[1]["detector"], "url-userinfo");
    assert!(verdicts[1].get("trigger").is_some());
    assert!(verdicts[1]["masked"].is_string());
    assert_eq!(verdicts[1]["location"], "atoms[1].content");
    assert!(verdicts[1]["message"]
        .as_str()
        .unwrap()
        .contains("url-userinfo"));
    assert_eq!(verdicts[2]["would_refuse"], true);
    assert_eq!(verdicts[2]["reason"], "invalid_input");
    assert!(verdicts[2]["message"]
        .as_str()
        .unwrap()
        .contains("at least 20 words"));
    let after = f
        .registry
        .dispatch("knowledge.get", json!({"id":"secret-existing"}))
        .await
        .unwrap();
    assert_eq!(after["content"], old["content"]);
    assert_eq!(after["updated_at"], old["updated_at"]);
}

#[tokio::test]
async fn dry_run_absent_and_false_preserve_mixed_batch_refusal_events() {
    let f = Fixture::new();
    let old = f.seed("secret-existing").await;
    for explicit_false in [false, true] {
        let before = f.refusals().await.len();
        let mut params = json!({"atoms":mixed()});
        if explicit_false {
            params["dry_run"] = json!(false);
        }
        let error = f.upsert(params).await.unwrap_err();
        let RuntimeError::SecretDetected(found) = error.refusal_source() else {
            panic!("expected original secret refusal: {error:?}");
        };
        assert_eq!(found.detector, "url-userinfo");
        assert_eq!(found.location.as_deref(), Some("atoms[1].content"));
        let events = f.refusals().await;
        assert_eq!(events.len(), before + 1, "DRY_RUN_WRITE_REFUSAL_CONTROL");
        assert!(events.iter().all(|event| event["target_id"] == old["id"]));
        let after = f
            .registry
            .dispatch("knowledge.get", json!({"id":"secret-existing"}))
            .await
            .unwrap();
        assert_eq!(after["content"], old["content"]);
        assert_eq!(after["updated_at"], old["updated_at"]);
    }
}

#[tokio::test]
async fn dry_run_clean_batch_is_read_only_and_write_control_creates_every_atom() {
    for explicit_false in [false, true] {
        let f = Fixture::new();
        let atoms = vec![atom(" first ", CONTENT), atom("second", CONTENT)];
        let before = f.snapshot().await;
        let writers = f.runtime.backend().pool().writer_acquisition_snapshot();
        let preview = f
            .upsert(json!({"atoms":atoms,"dry_run":true,"chunk_size":1}))
            .await
            .unwrap();
        assert_eq!(preview["would_refuse_batch"], false);
        assert_eq!(preview["results"].as_array().unwrap().len(), 2);
        assert_eq!(preview["results"][0]["slug"], "first");
        assert!(preview["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["would_refuse"] == false));
        assert_eq!(f.snapshot().await, before);
        assert_eq!(
            f.runtime.backend().pool().writer_acquisition_snapshot(),
            writers,
            "DRY_RUN_CLEAN_NO_WRITER"
        );
        let mut params = json!({"atoms":atoms,"chunk_size":1});
        if explicit_false {
            params["dry_run"] = json!(false);
        }
        assert_eq!(
            f.upsert(params).await.unwrap(),
            json!({"created":2,"updated":0,"total":2})
        );
        assert!(
            f.runtime
                .backend()
                .pool()
                .writer_acquisition_snapshot()
                .acquisitions
                > writers.acquisitions,
            "DRY_RUN_WRITER_EFFECT_CONTROL"
        );
        for slug in ["first", "second"] {
            let row = f
                .registry
                .dispatch("knowledge.get", json!({"id":slug}))
                .await
                .unwrap();
            assert_eq!(row["content"], CONTENT);
        }
    }
}

#[tokio::test]
async fn dry_run_verdicts_agree_with_single_writes_including_target_refusals() {
    let f = Fixture::new();
    let live = f.seed("live").await;
    let deleted = f.seed("deleted").await;
    f.registry
        .dispatch("knowledge.delete_atoms", json!({"ids":[deleted["id"]]}))
        .await
        .unwrap();
    f.registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains":[{
                "slug":"domain","name":"Domain","description":CONTENT
            }]}),
        )
        .await
        .unwrap();
    let domain = f
        .registry
        .dispatch("knowledge.get", json!({"id":"domain"}))
        .await
        .unwrap();
    let mut mirror = atom("tagged-mirror", CONTENT);
    mirror["tags"] = json!(["type:domain"]);
    f.upsert(json!({"atoms":[mirror]})).await.unwrap();
    let mirror = f
        .registry
        .dispatch("knowledge.get", json!({"id":"tagged-mirror"}))
        .await
        .unwrap();
    let items = vec![
        atom("ordinary-new", CONTENT),
        atom(
            "secret-new",
            &format!("{CONTENT} https://{SECRET}@example.invalid/path"),
        ),
        atom("invalid-new", "too short"),
        json!({"id":uuid::Uuid::new_v4(),"properties":{}}),
        json!({"id":domain["id"],"properties":{}}),
        json!({"id":mirror["id"],"properties":{}}),
        json!({"id":deleted["id"],"properties":{}}),
        atom("domain", CONTENT),
        atom("deleted", CONTENT),
        json!({"id":live["id"],"properties":{"revised":true}}),
    ];
    let before = f.snapshot().await;
    let preview = f
        .upsert(json!({"atoms":items,"dry_run":true}))
        .await
        .unwrap();
    assert_eq!(preview["results"].as_array().unwrap().len(), items.len());
    assert_eq!(
        f.snapshot().await,
        before,
        "DRY_RUN_TARGET_CHECKS_READ_ONLY"
    );
    for (index, input) in items.into_iter().enumerate() {
        let write = f.upsert(json!({"atoms":[input]})).await;
        let verdict = &preview["results"][index];
        assert_eq!(
            verdict["would_refuse"],
            write.is_err(),
            "DRY_RUN_AGREEMENT item {index}"
        );
        if let Err(error) = write {
            if !matches!(error.refusal_source(), RuntimeError::SecretDetected(_)) {
                assert_eq!(verdict["message"], error.refusal_source().to_string());
            }
        }
    }
}

#[tokio::test]
async fn dry_run_never_echoes_raw_secrets_in_content_or_slug() {
    let f = Fixture::new();
    let slug_secret = "charlie:slug-fixture-password";
    let slug = format!("https://{slug_secret}@example.invalid/atom");
    let result = f
        .upsert(json!({"dry_run":true,"atoms":[
            atom("content-secret", &format!("{CONTENT} https://{SECRET}@example.invalid/path")),
            atom(&slug, CONTENT)
        ]}))
        .await
        .unwrap();
    let serialized = result.to_string();
    for raw in [SECRET, slug_secret] {
        assert!(!serialized.contains(raw), "DRY_RUN_SECRET_NOT_ECHOED");
    }
    let verdict = &result["results"][1];
    assert_eq!(verdict["identity_masked"], true);
    assert_eq!(verdict["would_refuse"], true);
    assert_eq!(verdict["location"], "atoms[1].slug");
    assert_ne!(verdict["slug"], slug);
}

#[tokio::test]
async fn dry_run_keeps_batch_shape_errors_and_publishes_boolean_flag() {
    let f = Fixture::new();
    let before = f.snapshot().await;
    for params in [
        json!({"atoms":[atom("clean",CONTENT)],"dry_run":"yes"}),
        json!({"atoms":[atom("clean",CONTENT)],"dry_run":null}),
        json!({"atoms":[atom("clean",CONTENT)],"dry_run":true,"unknown_field":true}),
        json!({"atoms":[{"slug":"clean","name":"Clean","content":CONTENT,"unknown_field":true}],"dry_run":true}),
        json!({"atoms":[],"dry_run":true}),
        json!({"atoms":vec![atom("clean",CONTENT);5001],"dry_run":true}),
    ] {
        assert!(f.upsert(params).await.is_err(), "DRY_RUN_BATCH_BOUNDARY");
    }
    assert_eq!(f.snapshot().await, before);
    let help = f.registry.describe_verb("knowledge.upsert_atoms").unwrap();
    assert_eq!(
        help["input_schema"]["properties"]["dry_run"]["type"],
        "boolean"
    );
    assert!(!help["input_schema"]["required"]
        .as_array()
        .unwrap()
        .contains(&json!("dry_run")));
}

#[tokio::test]
async fn dry_run_dispatch_audit_omits_secrets_and_preserves_non_audit_state() {
    let f = Fixture::with_dispatch_audit(true);
    f.seed("secret-existing").await;
    let slug_secret = "charlie:audit-slug-fixture-password";
    let atoms = vec![
        atom(
            "secret-existing",
            &format!("{CONTENT} https://{SECRET}@example.invalid/path"),
        ),
        atom(
            &format!("https://{slug_secret}@example.invalid/atom"),
            CONTENT,
        ),
    ];
    let before = f.snapshot_including_audit(false).await;
    let success_count = f.audit_payloads("success").await.as_array().unwrap().len();
    let preview = f
        .upsert(json!({"atoms":atoms,"dry_run":true}))
        .await
        .unwrap();
    assert_eq!(preview["would_refuse_batch"], true);
    assert_eq!(
        f.snapshot_including_audit(false).await,
        before,
        "DRY_RUN_NON_AUDIT_STATE_UNCHANGED"
    );
    let success = f.audit_payloads("success").await;
    assert_eq!(
        success.as_array().unwrap().len(),
        success_count + 1,
        "DRY_RUN_DISPATCH_AUDIT_PRESERVED"
    );
    assert!(f.upsert(json!({"atoms":atoms})).await.is_err());
    let failed = f.audit_payloads("error").await;
    assert_eq!(
        failed.as_array().unwrap().len(),
        1,
        "DRY_RUN_REFUSED_DISPATCH_AUDIT_CONTROL"
    );
    for serialized in [preview.to_string(), success.to_string(), failed.to_string()] {
        for raw in [SECRET, slug_secret] {
            assert!(
                !serialized.contains(raw),
                "DRY_RUN_DISPATCH_AUDIT_NO_SECRET"
            );
        }
    }
}

#[tokio::test]
async fn dry_run_duplicate_slugs_preserve_order_and_write_coalescing() {
    let f = Fixture::new();
    let mut second = atom(" shared ", CONTENT);
    second["name"] = json!("Second value wins");
    let atoms = vec![atom("shared", CONTENT), second];
    let preview = f
        .upsert(json!({"atoms":atoms,"dry_run":true}))
        .await
        .unwrap();
    assert_eq!(preview["would_refuse_batch"], false);
    assert_eq!(preview["results"].as_array().unwrap().len(), 2);
    assert_eq!(
        f.upsert(json!({"atoms":atoms})).await.unwrap(),
        json!({"created":1,"updated":1,"total":2})
    );
    let row = f
        .registry
        .dispatch("knowledge.get", json!({"id":"shared"}))
        .await
        .unwrap();
    assert_eq!(row["name"], "Second value wins");
}
