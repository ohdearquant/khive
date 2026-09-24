use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlRow, SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

const CURRENT: i64 = 1_790_208_000_000_000;

async fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "gtd".into()],
        actor_id: Some(Namespace::LOCAL.into()),
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(Namespace::LOCAL.into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    khive_pack_gtd::handlers::ensure_audit_schema(&runtime).await;
    (runtime, registry)
}

struct Seed {
    namespace: &'static str,
    kind: &'static str,
    created: SqlValue,
    updated: SqlValue,
    properties: String,
    deleted: bool,
}

impl Default for Seed {
    fn default() -> Self {
        Self {
            namespace: "local",
            kind: "task",
            created: SqlValue::Integer(0),
            updated: SqlValue::Integer(CURRENT),
            properties:
                r#"{"status":"archived","archived_at":1772323200000,"unrelated":"retain me"}"#
                    .into(),
            deleted: false,
        }
    }
}

async fn seed(runtime: &KhiveRuntime, seed: Seed) -> String {
    let id = Uuid::new_v4().to_string();
    runtime.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "INSERT INTO notes (id,namespace,kind,name,content,properties,created_at,updated_at,deleted_at) VALUES (?1,?2,?3,'repair fixture','preserve body',?4,?5,?6,?7)".into(),
        params: vec![
            SqlValue::Text(id.clone()), SqlValue::Text(seed.namespace.into()),
            SqlValue::Text(seed.kind.into()), SqlValue::Text(seed.properties),
            seed.created, seed.updated,
            if seed.deleted { SqlValue::Integer(19) } else { SqlValue::Null },
        ],
        label: Some("seed-repair-fixture".into()),
    }).await.unwrap();
    id
}

async fn row(runtime: &KhiveRuntime, id: &str) -> SqlRow {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_row(SqlStatement {
            sql:
                "SELECT *, properties -> '$.archived_at' AS archived_source FROM notes WHERE id=?1"
                    .into(),
            params: vec![SqlValue::Text(id.into())],
            label: None,
        })
        .await
        .unwrap()
        .unwrap()
}

async fn snapshot(runtime: &KhiveRuntime, id: &str) -> Vec<u8> {
    serde_json::to_vec(&row(runtime, id).await).unwrap()
}

fn text<'a>(row: &'a SqlRow, field: &str) -> &'a str {
    match row.get(field) {
        Some(SqlValue::Text(value)) => value,
        other => panic!("expected SQL text for {field}, got {other:?}"),
    }
}

fn integer(row: &SqlRow, field: &str) -> i64 {
    match row.get(field) {
        Some(SqlValue::Integer(value)) => *value,
        other => panic!("expected SQL integer for {field}, got {other:?}"),
    }
}

fn properties(row: &SqlRow) -> Value {
    serde_json::from_str(text(row, "properties")).unwrap()
}

async fn audit(runtime: &KhiveRuntime, id: &str) -> Vec<SqlRow> {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT * FROM gtd_lifecycle_audit WHERE note_id=?1 ORDER BY at".into(),
            params: vec![SqlValue::Text(id.into())],
            label: None,
        })
        .await
        .unwrap()
}

async fn candidates(registry: &VerbRegistry) -> Vec<Value> {
    registry
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap()["candidates"]["rows"]
        .as_array()
        .unwrap()
        .clone()
}

fn item(id: &str, field: &str, observed: Value, value: Value) -> Value {
    let mut changes = serde_json::Map::new();
    changes.insert(field.into(), json!({"observed": observed, "value": value}));
    json!({"id": id, "changes": changes})
}

#[tokio::test]
async fn repair_defaults_to_dry_run_and_preserves_raw_rows() {
    let (runtime, registry) = fixture().await;
    let id = seed(
        &runtime,
        Seed {
            created: SqlValue::Text("not-a-time".into()),
            ..Seed::default()
        },
    )
    .await;
    let observed = candidates(&registry).await[0]["raw"]["created_at"].clone();
    assert_eq!(observed, json!(r#""not-a-time""#));
    let before = snapshot(&runtime, &id).await;
    for explicit_false in [false, true] {
        let mut request =
            json!({"items": [item(&id, "created_at", observed.clone(), json!(CURRENT))]});
        if explicit_false {
            request["apply"] = json!(false);
        }
        let reply = registry.dispatch("gtd.repair", request).await.unwrap();
        assert_eq!(reply["apply"], false);
        assert_eq!(reply["results"][0]["accepted"], true);
        assert_eq!(
            reply["results"][0]["applied"], false,
            "REPAIR_DRY_RUN_WRITES_NOTHING"
        );
        assert_eq!(reply["results"][0]["stored"]["created_at"], observed);
        assert_eq!(reply["results"][0]["proposed"]["created_at"], CURRENT);
        assert_eq!(
            snapshot(&runtime, &id).await,
            before,
            "REPAIR_DRY_RUN_WRITES_NOTHING"
        );
        assert!(
            audit(&runtime, &id).await.is_empty(),
            "dry run must append no audit"
        );
    }
}

#[tokio::test]
async fn repair_apply_preserves_originals_and_removes_only_repaired_census_candidate() {
    let (runtime, registry) = fixture().await;
    let id = seed(
        &runtime,
        Seed {
            updated: SqlValue::Integer(17),
            ..Seed::default()
        },
    )
    .await;
    let control = seed(
        &runtime,
        Seed {
            created: SqlValue::Integer(CURRENT),
            properties: r#"{"status":"next"}"#.into(),
            ..Seed::default()
        },
    )
    .await;
    let control_before = snapshot(&runtime, &control).await;
    let census = candidates(&registry).await;
    assert_eq!(census.len(), 1);
    assert_eq!(census[0]["id"], id);
    let reply = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [{
                "id": id, "changes": {
                    "created_at": {"observed": census[0]["raw"]["created_at"], "value": CURRENT},
                    "updated_at": {"observed": census[0]["raw"]["updated_at"], "value": CURRENT + 1}
                }
            }]}),
        )
        .await
        .unwrap();
    assert_eq!(reply["applied"], 1);
    let after = row(&runtime, &id).await;
    assert_eq!(integer(&after, "created_at"), CURRENT);
    assert_eq!(integer(&after, "updated_at"), CURRENT + 1);
    let props = properties(&after);
    for (field, original) in [("created_at", "0"), ("updated_at", "17")] {
        let preserved = &props["gtd_repair"]["originals"][field];
        assert_eq!(
            preserved["value"], original,
            "REPAIR_FIRST_ORIGINALS_RETAINED"
        );
        assert_eq!(preserved["actor"], "local");
        assert!(preserved["at"].as_i64().unwrap() > 0);
    }
    assert_eq!(props["unrelated"], "retain me");
    assert_eq!(text(&after, "content"), "preserve body");
    assert!(
        candidates(&registry).await.is_empty(),
        "only repaired candidate must leave census"
    );
    assert_eq!(snapshot(&runtime, &control).await, control_before);
    let records = audit(&runtime, &id).await;
    assert_eq!(records.len(), 1);
    let note: Value = serde_json::from_str(text(&records[0], "note")).unwrap();
    assert_eq!(note["operation"], "gtd.repair");
    assert_eq!(note["actor"], "local");
    assert_eq!(note["changes"]["created_at"]["observed"], "0");
    assert_eq!(note["changes"]["updated_at"]["value"], CURRENT + 1);
}

#[tokio::test]
async fn repair_refuses_stale_observation_and_replayed_old_observation() {
    let (runtime, registry) = fixture().await;
    let id = seed(&runtime, Seed::default()).await;
    let observed = candidates(&registry).await[0]["raw"]["created_at"].clone();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "UPDATE notes SET created_at=17 WHERE id=?1".into(),
            params: vec![SqlValue::Text(id.clone())],
            label: None,
        })
        .await
        .unwrap();
    let before = snapshot(&runtime, &id).await;
    let stale = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&id, "created_at", observed, json!(CURRENT))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(
        stale["results"][0]["reason"], "stale_observed",
        "REPAIR_STALE_OBSERVATION_REFUSED"
    );
    assert_eq!(snapshot(&runtime, &id).await, before);
    assert!(audit(&runtime, &id).await.is_empty());
    let request =
        json!({"apply": true, "items": [item(&id, "created_at", json!("17"), json!(CURRENT))]});
    assert_eq!(
        registry
            .dispatch("gtd.repair", request.clone())
            .await
            .unwrap()["applied"],
        1
    );
    let applied = snapshot(&runtime, &id).await;
    let replay = registry.dispatch("gtd.repair", request).await.unwrap();
    assert_eq!(
        replay["results"][0]["reason"], "stale_observed",
        "REPAIR_STALE_OBSERVATION_REFUSED"
    );
    assert_eq!(snapshot(&runtime, &id).await, applied);
    assert_eq!(audit(&runtime, &id).await.len(), 1);
}

#[tokio::test]
async fn repair_legacy_status_preserves_archive_and_is_listed_as_cancelled() {
    let (runtime, registry) = fixture().await;
    let archive = "1234567890.123456789012345678901234567890";
    let id = seed(
        &runtime,
        Seed {
            properties: format!(
                r#"{{"status":"archived","archived_at":{archive},"unrelated":"retain me"}}"#
            ),
            ..Seed::default()
        },
    )
    .await;
    let source = candidates(&registry).await[0]["stored_status"].clone();
    let reply = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&id, "status", source.clone(), json!("cancelled"))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(reply["applied"], 1);
    let after = row(&runtime, &id).await;
    assert_eq!(
        text(&after, "archived_source"),
        archive,
        "REPAIR_LEGACY_STATUS_PRESERVES_ARCHIVE"
    );
    let props = properties(&after);
    assert_eq!(props["status"], "cancelled");
    assert_eq!(props["gtd_repair"]["originals"]["status"]["value"], source);
    assert_eq!(
        integer(&after, "updated_at"),
        CURRENT,
        "status repair cannot change updated_at"
    );
    let listed = registry
        .dispatch("gtd.tasks", json!({"status": "cancelled"}))
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["full_id"], id);
}

#[tokio::test]
async fn repair_refuses_noncanonical_targets_and_canonical_status_changes() {
    let (runtime, registry) = fixture().await;
    let id = seed(&runtime, Seed::default()).await;
    let before = snapshot(&runtime, &id).await;
    for target in [
        json!("next"),
        json!("archived"),
        json!("finished"),
        json!("DONE"),
        json!(4),
    ] {
        let reply = registry
            .dispatch(
                "gtd.repair",
                json!({"apply": true, "items": [
                    item(&id, "status", json!(r#""archived""#), target)
                ]}),
            )
            .await
            .unwrap();
        assert_eq!(
            reply["results"][0]["reason"], "invalid_target",
            "REPAIR_TERMINAL_TARGET_ONLY"
        );
        assert_eq!(snapshot(&runtime, &id).await, before);
    }
    let canonical = seed(
        &runtime,
        Seed {
            properties: r#"{"status":"inbox"}"#.into(),
            ..Seed::default()
        },
    )
    .await;
    let canonical_before = snapshot(&runtime, &canonical).await;
    let reply = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&canonical, "status", json!(r#""inbox""#), json!("cancelled"))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(reply["results"][0]["reason"], "canonical_status");
    assert_eq!(snapshot(&runtime, &canonical).await, canonical_before);
    assert!(audit(&runtime, &id).await.is_empty());
    assert!(audit(&runtime, &canonical).await.is_empty());
}

#[tokio::test]
async fn repair_keeps_fallback_statuses_on_lifecycle_path_but_repairs_their_timestamps() {
    let (runtime, registry) = fixture().await;
    for (props, observed) in [
        ("{}", Value::Null),
        (r#"{"status":null}"#, json!("null")),
        (r#"{"status":4}"#, json!("4")),
    ] {
        let id = seed(
            &runtime,
            Seed {
                properties: props.into(),
                ..Seed::default()
            },
        )
        .await;
        let before = snapshot(&runtime, &id).await;
        let reply = registry
            .dispatch(
                "gtd.repair",
                json!({"apply": true, "items": [
                    item(&id, "status", observed, json!("cancelled"))
                ]}),
            )
            .await
            .unwrap();
        assert_eq!(reply["results"][0]["reason"], "lifecycle_status");
        let message = reply["results"][0]["message"].as_str().unwrap();
        assert!(message.contains("gtd.transition") && message.contains("gtd.complete"));
        assert_eq!(snapshot(&runtime, &id).await, before);
        let repaired = registry
            .dispatch(
                "gtd.repair",
                json!({"apply": true, "items": [
                    item(&id, "created_at", json!("0"), json!(CURRENT))
                ]}),
            )
            .await
            .unwrap();
        assert_eq!(
            repaired["applied"], 1,
            "timestamp repair is independent of fallback status"
        );
        let after_props = properties(&row(&runtime, &id).await);
        let original_props: Value = serde_json::from_str(props).unwrap();
        assert_eq!(after_props.get("status"), original_props.get("status"));
    }
}

#[tokio::test]
async fn repair_commits_valid_rows_independently_of_refused_rows() {
    let (runtime, registry) = fixture().await;
    let valid = seed(&runtime, Seed::default()).await;
    let non_task = seed(
        &runtime,
        Seed {
            kind: "observation",
            ..Seed::default()
        },
    )
    .await;
    let deleted = seed(
        &runtime,
        Seed {
            deleted: true,
            ..Seed::default()
        },
    )
    .await;
    let missing = Uuid::new_v4().to_string();
    let wrong_before = snapshot(&runtime, &non_task).await;
    let deleted_before = snapshot(&runtime, &deleted).await;
    let items: Vec<_> = [&valid, &non_task, &deleted, &missing]
        .into_iter()
        .map(|id| item(id, "created_at", json!("0"), json!(CURRENT)))
        .collect();
    let reply = registry
        .dispatch("gtd.repair", json!({"apply": true, "items": items}))
        .await
        .unwrap();
    assert_eq!(reply["accepted"], 1);
    assert_eq!(reply["applied"], 1);
    assert_eq!(reply["refused"], 3);
    for (index, reason) in [(1, "not_task"), (2, "deleted"), (3, "not_found")] {
        assert_eq!(reply["results"][index]["reason"], reason);
        assert_eq!(reply["results"][index]["applied"], false);
    }
    assert_eq!(snapshot(&runtime, &non_task).await, wrong_before);
    assert_eq!(snapshot(&runtime, &deleted).await, deleted_before);
    assert_eq!(integer(&row(&runtime, &valid).await, "created_at"), CURRENT);
    assert_eq!(audit(&runtime, &valid).await.len(), 1);
    for id in [&non_task, &deleted, &missing] {
        assert!(audit(&runtime, id).await.is_empty());
    }
}

#[tokio::test]
async fn repair_rejects_duplicate_oversized_and_malformed_requests_before_effects() {
    let (runtime, registry) = fixture().await;
    let id = seed(&runtime, Seed::default()).await;
    let change = item(&id, "created_at", json!("0"), json!(CURRENT));
    let before = snapshot(&runtime, &id).await;
    let oversized: Vec<_> = std::iter::once(change.clone())
        .chain((0..100).map(|_| {
            item(
                &Uuid::new_v4().to_string(),
                "created_at",
                json!("0"),
                json!(CURRENT),
            )
        }))
        .collect();
    for request in [
        json!({"apply": true, "items": [change.clone(), change.clone()]}),
        json!({"apply": true, "items": oversized}),
        json!({"apply": true, "items": []}),
        json!({"apply": true, "items": [change, {"id": Uuid::new_v4().to_string(), "changes": {"created_at": {"value": CURRENT}}}]}),
    ] {
        assert!(registry.dispatch("gtd.repair", request).await.is_err());
        assert_eq!(snapshot(&runtime, &id).await, before);
        assert!(audit(&runtime, &id).await.is_empty());
    }
    let unsupported = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&id, "archived_at", json!("1772323200000"), json!(CURRENT))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(unsupported["results"][0]["reason"], "unsupported_field");
    assert!(unsupported["results"][0]
        .to_string()
        .contains("archived_at"));
    assert_eq!(snapshot(&runtime, &id).await, before);
}

#[tokio::test]
async fn repair_is_namespace_agnostic_and_retains_first_original_without_touching_updated_at() {
    let (runtime, registry) = fixture().await;
    let id = seed(
        &runtime,
        Seed {
            namespace: "foreign",
            created: SqlValue::Float(1_000_000_000.5),
            updated: SqlValue::Text("legacy-updated".into()),
            ..Seed::default()
        },
    )
    .await;
    assert!(
        candidates(&registry).await.is_empty(),
        "foreign discovery remains scoped"
    );
    let before = snapshot(&runtime, &id).await;
    let rounded_spelling = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&id, "created_at", json!("1000000000.50"), json!(CURRENT))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(
        rounded_spelling["results"][0]["reason"], "stale_observed",
        "REPAIR_COMPARES_EXACT_SOURCE_TEXT"
    );
    assert_eq!(snapshot(&runtime, &id).await, before);
    let first = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&id, "created_at", json!("1000000000.5"), json!(CURRENT))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(first["applied"], 1, "REPAIR_BY_ID_IS_NAMESPACE_AGNOSTIC");
    let first_row = row(&runtime, &id).await;
    assert!(
        matches!(first_row.get("updated_at"), Some(SqlValue::Text(value)) if value == "legacy-updated"),
        "REPAIR_LEAVES_UNREQUESTED_TIMESTAMP"
    );
    assert_eq!(text(&first_row, "namespace"), "foreign");
    let original = properties(&first_row)["gtd_repair"]["originals"]["created_at"].clone();
    assert_eq!(original["value"], "1000000000.5");
    assert_eq!(original["actor"], "local");
    let second = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&id, "created_at", json!(CURRENT.to_string()), json!(CURRENT + 2))
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(second["applied"], 1);
    let second_row = row(&runtime, &id).await;
    let second_props = properties(&second_row);
    assert_eq!(
        second_props["gtd_repair"]["originals"]["created_at"], original,
        "REPAIR_FIRST_ORIGINALS_RETAINED"
    );
    assert_eq!(
        second_props["gtd_repair"]["last"]["changes"]["created_at"]["observed"],
        CURRENT.to_string()
    );
    assert!(
        matches!(second_row.get("updated_at"), Some(SqlValue::Text(value)) if value == "legacy-updated"),
        "REPAIR_LEAVES_UNREQUESTED_TIMESTAMP"
    );
    assert_eq!(audit(&runtime, &id).await.len(), 2);
}

#[tokio::test]
async fn repair_audit_failure_rolls_back_the_row_and_preserves_the_storage_error() {
    let (runtime, registry) = fixture().await;
    let earlier = seed(&runtime, Seed::default()).await;
    let id = seed(&runtime, Seed::default()).await;
    runtime.sql().writer().await.unwrap().execute_script(
        format!("CREATE TRIGGER reject_repair_audit BEFORE INSERT ON gtd_lifecycle_audit WHEN NEW.note_id = '{id}' BEGIN SELECT RAISE(ABORT, 'repair audit fixture refused'); END;"),
    ).await.unwrap();
    let before = snapshot(&runtime, &id).await;
    let error = registry
        .dispatch(
            "gtd.repair",
            json!({"apply": true, "items": [
                item(&earlier, "created_at", json!("0"), json!(CURRENT)),
                item(&id, "created_at", json!("0"), json!(CURRENT))
            ]}),
        )
        .await
        .expect_err("REPAIR_AUDIT_FAILURE_MUST_REFUSE");
    assert!(
        error.to_string().contains("repair audit fixture refused"),
        "original audit failure must remain visible: {error}"
    );
    assert_eq!(
        snapshot(&runtime, &id).await,
        before,
        "REPAIR_AUDIT_FAILURE_ROLLS_BACK_ROW"
    );
    assert!(audit(&runtime, &id).await.is_empty());
    assert_eq!(
        integer(&row(&runtime, &earlier).await, "created_at"),
        CURRENT
    );
    assert_eq!(
        audit(&runtime, &earlier).await.len(),
        1,
        "earlier per-row commits survive a later storage failure"
    );
}
