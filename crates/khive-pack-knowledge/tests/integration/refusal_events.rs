//! Refusal traces through real knowledge dispatch; all databases are in memory.

use super::Fixture;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, PackRegistry, RuntimeConfig,
    RuntimeError, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const ACTOR: &str = "lambda:refusal-events-fixture";
const CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

fn fixture() -> (KhiveRuntime, Fixture) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some(ACTOR.into()),
        brain_profile: None,
        events_split: None,
        packs: vec!["kg".into(), "knowledge".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("explicit in-memory runtime without embedders");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.with_default_namespace("local");
    PackRegistry::register_packs(
        &["kg".into(), "knowledge".into()],
        runtime.clone(),
        &mut builder,
    )
    .expect("register kg and knowledge");
    let registry = builder.build().expect("registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, Fixture { registry })
}

fn credential(last: char) -> String {
    format!("ghp_FakeGitHubToken000000000000000000{last}")
}

fn candidate(slug: &str, content: &str) -> Value {
    json!({"slug": slug, "name": "Safe candidate name", "content": content})
}

async fn seed(fixture: &Fixture, slug: &str) -> Value {
    fixture
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [candidate(slug, CONTENT)]}),
        )
        .await
        .expect("safe seed");
    fixture
        .dispatch("knowledge.get", json!({"id": slug}))
        .await
        .expect("seed readback")
}

async fn atom_rows(runtime: &KhiveRuntime) -> Value {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("fixture reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM knowledge_atoms ORDER BY id".into(),
            params: vec![],
            label: Some("test.refusal_events.atom_snapshot".into()),
        })
        .await
        .expect("all atom columns");
    serde_json::to_value(rows).expect("snapshot JSON")
}

async fn events(fixture: &Fixture, target: Option<&str>) -> Vec<Value> {
    let mut params = json!({
        "kind": "event", "event_kind": "refusal", "verb": "knowledge.upsert_atoms",
        "namespace": "local", "limit": 100,
    });
    if let Some(target) = target {
        params["target_id"] = json!(target);
    }
    fixture
        .dispatch("list", params)
        .await
        .expect("public refusal event readback")["items"]
        .as_array()
        .expect("event items")
        .clone()
}

fn secret_projection(error: RuntimeError, location: &str) -> Value {
    let RuntimeError::SecretDetected(found) = error.refusal_source() else {
        panic!("original typed secret refusal was replaced: {error:?}");
    };
    assert_eq!(found.location.as_deref(), Some(location));
    let message = error.to_string();
    assert!(error.retryable_failure_context().is_none());
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["code"], "secret_detected");
    assert_eq!(value["message"], message);
    assert_eq!(value["location"], location);
    value
}

#[tokio::test]
async fn refusal_events_trace_existing_batch_members_without_mutating_atoms() {
    let (runtime, fixture) = fixture();
    let first = seed(&fixture, "refusal-first").await;
    let sibling = seed(&fixture, "refusal-sibling").await;
    let before = atom_rows(&runtime).await;
    let secret = credential('0');
    let rejected = candidate("refusal-first", &format!("{CONTENT} {secret}"));
    let valid = candidate("refusal-sibling", &format!("{CONTENT} revised safely"));
    let error = fixture
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [rejected, valid]}),
        )
        .await
        .expect_err("whole batch refuses");
    let projection = secret_projection(error, "atoms[0].content");
    assert_eq!(projection["refusal_recorded"], true);
    assert_eq!(projection["refusal_events"].as_array().unwrap().len(), 2);
    assert_eq!(projection["refusal_events"][0]["subject"], first["id"]);
    assert_eq!(projection["refusal_events"][1]["subject"], sibling["id"]);
    assert_eq!(
        atom_rows(&runtime).await,
        before,
        "all columns remain intact"
    );

    for (index, target, reason, outcome) in [
        (0, &first, "secret_detected", "denied"),
        (1, &sibling, "batch_refused", "error"),
    ] {
        let rows = events(&fixture, target["id"].as_str()).await;
        assert_eq!(rows.len(), 1);
        let event = &rows[0];
        assert_eq!(event["id"], projection["refusal_events"][index]["event_id"]);
        assert_eq!(event["target_id"], target["id"]);
        assert_eq!(event["namespace"], "local");
        assert_eq!(event["actor"], format!("actor:{ACTOR}"));
        assert_eq!(event["kind"], "refusal");
        assert_eq!(event["outcome"], outcome);
        assert_eq!(event["payload"]["subject_kind"], "knowledge_atom");
        assert_eq!(event["payload"]["item_index"], index);
        assert_eq!(event["payload"]["reason"], reason);
        assert_eq!(event["payload"]["digest_input"], "masked_submitted_atom_v1");
        let digest = event["payload"]["rejected_digest"].as_str().unwrap();
        assert_eq!(digest.len(), "blake3:".len() + 64);
        assert!(digest.starts_with("blake3:"));
        assert!(!event.to_string().contains(&secret));
        assert!(!event["payload"].to_string().contains("Safe candidate name"));
        if index == 0 {
            assert_eq!(event["payload"]["detector"], projection["detector"]);
            assert_eq!(event["payload"]["location"], "atoms[0].content");
        } else {
            assert_eq!(event["payload"]["first_refusing_item_index"], 0);
            assert!(event["payload"].get("detector").is_none());
        }
    }
}

#[tokio::test]
async fn refusal_events_digest_masks_credentials_but_distinguishes_safe_candidate_changes() {
    let (runtime, fixture) = fixture();
    let old = seed(&fixture, "refusal-digest").await;
    let before = atom_rows(&runtime).await;
    let mut digests = Vec::new();
    let mut event_ids = Vec::new();
    for (last, name) in [
        ('0', "Same name"),
        ('0', "Same name"),
        ('1', "Same name"),
        ('1', "Changed name"),
    ] {
        let secret = credential(last);
        let mut input = candidate("refusal-digest", &format!("{CONTENT} {secret}"));
        input["name"] = json!(name);
        let error = fixture
            .dispatch("knowledge.upsert_atoms", json!({"atoms": [input]}))
            .await
            .expect_err("secret candidate refuses");
        let projection = secret_projection(error, "atoms[0].content");
        assert_eq!(projection["refusal_recorded"], true);
        let event_id = projection["refusal_events"][0]["event_id"].clone();
        assert!(
            !event_ids.contains(&event_id),
            "each refusal has its own trace"
        );
        let rows = events(&fixture, old["id"].as_str()).await;
        let event = rows.iter().find(|event| event["id"] == event_id).unwrap();
        digests.push(event["payload"]["rejected_digest"].clone());
        event_ids.push(event_id);
        assert!(!event.to_string().contains(&secret));
        assert_eq!(atom_rows(&runtime).await, before);
    }
    assert_eq!(digests[0], digests[1], "identical submitted candidate");
    assert_eq!(
        digests[1], digests[2],
        "masked credential bytes are absent from the hash"
    );
    assert_ne!(
        digests[2], digests[3],
        "safe changed fields remain in the hash"
    );
}

#[tokio::test]
async fn refusal_events_validation_refusal_keeps_original_error_and_old_row() {
    let (runtime, fixture) = fixture();
    let old = seed(&fixture, "refusal-validation").await;
    let before = atom_rows(&runtime).await;
    let error = fixture
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [candidate("refusal-validation", "too short")]}),
        )
        .await
        .expect_err("content validation refuses");
    assert!(matches!(
        error.refusal_source(),
        RuntimeError::InvalidInput(_)
    ));
    let message = error.to_string();
    let projection = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(projection["message"], message);
    assert_eq!(projection["refusal_recorded"], true);
    let rows = events(&fixture, old["id"].as_str()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["outcome"], "error");
    assert_eq!(rows[0]["payload"]["reason"], "validation_refused");
    assert_eq!(atom_rows(&runtime).await, before);
}

#[tokio::test]
async fn refusal_events_new_missing_and_invalid_targets_remain_unadorned() {
    let (runtime, fixture) = fixture();
    seed(&fixture, "unrelated-existing").await;
    let before = atom_rows(&runtime).await;
    let secret = credential('0');
    for input in [
        candidate("brand-new-refused", &format!("{CONTENT} {secret}")),
        json!({"id": "a82d4e7f-f16c-469d-9c27-3cd7fb2a6ad1", "properties": {"nested": secret}}),
        json!({"id": "not-a-uuid", "properties": {"nested": secret}}),
    ] {
        let error = fixture
            .dispatch("knowledge.upsert_atoms", json!({"atoms": [input]}))
            .await
            .expect_err("ineligible target refuses");
        assert!(!matches!(&error, RuntimeError::RefusedWithEvents { .. }));
        let projection = runtime_error_value(error, DomainDisposition::Unknown);
        assert!(projection.get("refusal_recorded").is_none());
        assert!(projection.get("refusal_events").is_none());
        assert!(!projection.to_string().contains(&secret));
        assert!(events(&fixture, None).await.is_empty());
        assert_eq!(atom_rows(&runtime).await, before);
    }
}

#[tokio::test]
async fn refusal_events_append_failure_is_additive_and_sibling_trace_can_succeed() {
    let (runtime, fixture) = fixture();
    let old = seed(&fixture, "refusal-append-fails").await;
    let sibling = seed(&fixture, "refusal-append-succeeds").await;
    let before = atom_rows(&runtime).await;
    let target = uuid::Uuid::parse_str(old["id"].as_str().unwrap()).unwrap();
    {
        let sql = runtime.sql();
        let mut writer = sql.writer().await.expect("fixture trigger writer");
        writer
            .execute(SqlStatement {
                sql: format!(
                    "CREATE TRIGGER reject_one_refusal BEFORE INSERT ON events \
                     WHEN NEW.kind = 'refusal' AND NEW.target_id = '{target}' \
                     BEGIN SELECT RAISE(ABORT, 'PRIVATE-APPEND-FAILURE'); END"
                ),
                params: vec![],
                label: Some("test.refusal_events.reject_one_append".into()),
            })
            .await
            .expect("install trigger only in private in-memory fixture");
    }
    let secret = credential('0');
    let error = fixture
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [
                candidate("refusal-append-fails", &format!("{CONTENT} {secret}")),
                candidate("refusal-append-succeeds", &format!("{CONTENT} revised safely")),
            ]}),
        )
        .await
        .expect_err("original refusal survives a failed event append");
    let projection = secret_projection(error, "atoms[0].content");
    assert_eq!(projection["refusal_recorded"], false);
    assert_eq!(projection["refusal_events"].as_array().unwrap().len(), 2);
    assert_eq!(projection["refusal_events"][0]["subject"], old["id"]);
    assert_eq!(
        projection["refusal_events"][0]["error_class"],
        "event_append_failed"
    );
    assert!(projection["refusal_events"][0].get("event_id").is_none());
    assert_eq!(projection["refusal_events"][1]["subject"], sibling["id"]);
    assert!(!projection.to_string().contains("PRIVATE-APPEND-FAILURE"));
    assert!(!projection.to_string().contains(&secret));
    assert!(events(&fixture, old["id"].as_str()).await.is_empty());
    let rows = events(&fixture, sibling["id"].as_str()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], projection["refusal_events"][1]["event_id"]);
    assert_eq!(atom_rows(&runtime).await, before);
}

#[tokio::test]
async fn refusal_events_properties_only_deprecated_foreign_target_uses_caller_namespace() {
    let (runtime, fixture) = fixture();
    let old = seed(&fixture, "foreign-deprecated-refusal").await;
    {
        let sql = runtime.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer.execute(SqlStatement {
            sql: "UPDATE knowledge_atoms SET namespace = 'other', status = 'deprecated' WHERE id = ?1".into(),
            params: vec![SqlValue::Text(old["id"].as_str().unwrap().into())],
            label: Some("test.refusal_events.foreign_deprecated_target".into()),
        }).await.expect("private legacy target fixture");
    }
    let before = atom_rows(&runtime).await;
    let error = fixture
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{"id": old["id"], "properties": {"nested": credential('0')}}],
            }),
        )
        .await
        .expect_err("properties-only candidate refuses");
    let projection = secret_projection(error, "atoms[0].properties");
    assert_eq!(projection["refusal_recorded"], true);
    let rows = events(&fixture, old["id"].as_str()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["namespace"], "local");
    assert_eq!(rows[0]["target_id"], old["id"]);
    assert_eq!(rows[0]["actor"], format!("actor:{ACTOR}"));
    let foreign = fixture.dispatch("list", json!({
        "kind": "event", "event_kind": "refusal", "namespace": "other", "target_id": old["id"],
    })).await.expect("foreign namespace readback");
    assert!(foreign["items"].as_array().unwrap().is_empty());
    assert_eq!(atom_rows(&runtime).await, before);
}

#[tokio::test]
async fn refusal_events_deleted_and_domain_targets_do_not_acquire_atom_traces() {
    let (runtime, fixture) = fixture();
    let deleted = seed(&fixture, "deleted-refusal-target").await;
    fixture
        .dispatch("knowledge.delete_atoms", json!({"ids": [deleted["id"]]}))
        .await
        .expect("soft-delete fixture atom");
    fixture
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "domain-refusal-target", "name": "Domain target", "description": CONTENT,
            }]}),
        )
        .await
        .expect("create domain fixture");
    let domain = fixture
        .dispatch("knowledge.get", json!({"id": "domain-refusal-target"}))
        .await
        .expect("domain UUID");
    let before = atom_rows(&runtime).await;
    for target in [&deleted["id"], &domain["id"]] {
        let error = fixture
            .dispatch(
                "knowledge.upsert_atoms",
                json!({
                    "atoms": [{"id": target, "properties": {"nested": credential('0')}}],
                }),
            )
            .await
            .expect_err("ineligible target refuses");
        let projection = secret_projection(error, "atoms[0].properties");
        assert!(projection.get("refusal_recorded").is_none());
        assert!(projection.get("refusal_events").is_none());
        assert!(events(&fixture, None).await.is_empty());
        assert_eq!(atom_rows(&runtime).await, before);
    }
}
