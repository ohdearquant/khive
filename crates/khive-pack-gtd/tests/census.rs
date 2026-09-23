use std::collections::BTreeMap;

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

fn memory_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "gtd".into()],
        brain_profile: None,
        actor_id: Some(Namespace::LOCAL.into()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("explicit in-memory census fixture")
}

fn registry(runtime: &KhiveRuntime, extra_namespace: bool) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    // Keep explicit attribution without adding an actor-private namespace to
    // the local/foreign scope that these census fixtures are measuring.
    builder.with_actor_id(Some(Namespace::LOCAL.into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    if extra_namespace {
        builder.with_visible_namespaces(vec![Namespace::parse("foreign").unwrap()]);
    }
    builder.build().unwrap()
}

async fn seed(
    runtime: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    deleted: bool,
    created: SqlValue,
    archived: Option<Value>,
) {
    let mut properties = json!({
        "status": "archived",
        "transition_history": [{"from": "active", "to": "archived"}],
        "unrelated": "preserve the imported evidence",
    });
    if let Some(archived) = archived {
        properties["archived_at"] = archived;
    }
    runtime.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "INSERT INTO notes (id,namespace,kind,content,properties,created_at,updated_at,deleted_at) VALUES (?1,?2,?3,'legacy fixture',?4,?5,17,?6)".into(),
        params: vec![
            SqlValue::Text(Uuid::new_v4().to_string()),
            SqlValue::Text(namespace.into()),
            SqlValue::Text(kind.into()),
            SqlValue::Text(properties.to_string()),
            created,
            if deleted { SqlValue::Integer(19) } else { SqlValue::Null },
        ],
        label: Some("seed-legacy-census-row".into()),
    }).await.unwrap();
}

async fn snapshot(runtime: &KhiveRuntime) -> Value {
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT * FROM notes ORDER BY id".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    serde_json::to_value(rows).unwrap()
}

fn empty_histogram() -> BTreeMap<&'static str, u64> {
    [
        "null",
        "nonnumeric",
        "epoch_zero",
        "magnitude_10_digits",
        "magnitude_13_digits",
        "magnitude_16_digits",
        "other",
    ]
    .into_iter()
    .map(|key| (key, 0))
    .collect()
}

#[tokio::test]
async fn census_counts_exact_magnitudes_without_rewriting_legacy_rows() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    let mut created = empty_histogram();
    let mut archived = empty_histogram();
    let boundaries = [
        (0, "epoch_zero"),
        (999_999_999, "other"),
        (1_000_000_000, "magnitude_10_digits"),
        (9_999_999_999, "magnitude_10_digits"),
        (10_000_000_000, "other"),
        (999_999_999_999, "other"),
        (1_000_000_000_000, "magnitude_13_digits"),
        (9_999_999_999_999, "magnitude_13_digits"),
        (10_000_000_000_000, "other"),
        (999_999_999_999_999, "other"),
        (1_000_000_000_000_000, "magnitude_16_digits"),
        (9_999_999_999_999_999, "magnitude_16_digits"),
        (10_000_000_000_000_000, "other"),
        (i64::MAX, "other"),
        (i64::MIN, "other"),
    ];
    for (value, bucket) in boundaries {
        let values = if value > 0 {
            vec![value, -value]
        } else {
            vec![value]
        };
        for value in values {
            seed(
                &runtime,
                "local",
                "task",
                false,
                SqlValue::Integer(value),
                Some(json!(value)),
            )
            .await;
            *created.get_mut(bucket).unwrap() += 1;
            *archived.get_mut(bucket).unwrap() += 1;
        }
    }
    seed(
        &runtime,
        "local",
        "task",
        false,
        SqlValue::Float(1_000_000_000.5),
        Some(json!(1_000_000_000.5)),
    )
    .await;
    *created.get_mut("magnitude_10_digits").unwrap() += 1;
    *archived.get_mut("magnitude_10_digits").unwrap() += 1;
    for value in [
        json!("1700000000000"),
        json!(true),
        json!([1]),
        json!({"timestamp": 1}),
    ] {
        seed(
            &runtime,
            "local",
            "task",
            false,
            SqlValue::Text("not-a-timestamp".into()),
            Some(value),
        )
        .await;
        *created.get_mut("nonnumeric").unwrap() += 1;
        *archived.get_mut("nonnumeric").unwrap() += 1;
    }
    for value in [None, Some(Value::Null)] {
        seed(
            &runtime,
            "local",
            "task",
            false,
            SqlValue::Integer(1),
            value,
        )
        .await;
        *created.get_mut("other").unwrap() += 1;
        *archived.get_mut("null").unwrap() += 1;
    }
    // Numerically greater across different magnitudes is not a date ordering claim.
    seed(
        &runtime,
        "local",
        "task",
        false,
        SqlValue::Integer(1_000_000_000_000_000),
        Some(json!(1_000_000_000_000_i64)),
    )
    .await;
    *created.get_mut("magnitude_16_digits").unwrap() += 1;
    *archived.get_mut("magnitude_13_digits").unwrap() += 1;

    let before = snapshot(&runtime).await;
    let result = registry.dispatch("gtd.census", json!({})).await.unwrap();
    assert_eq!(result["created_at"], json!(created));
    assert_eq!(result["archived_at"], json!(archived));
    assert_eq!(result["total_tasks"], created.values().sum::<u64>());
    assert_eq!(result["created_at_gt_archived_at_raw"], 1);
    assert_eq!(result["schema_version"], 1);
    assert_eq!(
        result["scope"],
        json!({"kind": "task", "rows": "live_only", "namespaces": ["local"]})
    );
    assert!(result["interpretation"]
        .as_str()
        .unwrap()
        .contains("NOT temporal ordering"));
    assert!(result["interpretation"]
        .as_str()
        .unwrap()
        .contains("do not establish timestamp units"));
    assert!(result.get("tasks").is_none());
    assert!(result.get("samples").is_none());
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_obeys_namespace_kind_and_live_row_scope() {
    let runtime = memory_runtime();
    for (namespace, kind, deleted) in [
        ("local", "task", false),
        ("foreign", "task", false),
        ("hidden", "task", false),
        ("local", "observation", false),
        ("local", "task", true),
    ] {
        seed(
            &runtime,
            namespace,
            kind,
            deleted,
            SqlValue::Integer(0),
            Some(json!(0)),
        )
        .await;
    }
    let before = snapshot(&runtime).await;
    let local = registry(&runtime, false);
    let visible = registry(&runtime, true);
    let result = local.dispatch("gtd.census", json!({})).await.unwrap();
    assert_eq!(result["total_tasks"], 1);
    let result = visible.dispatch("gtd.census", json!({})).await.unwrap();
    assert_eq!(result["total_tasks"], 2);
    assert_eq!(result["scope"]["namespaces"], json!(["foreign", "local"]));
    let result = visible
        .dispatch("gtd.census", json!({"namespace": "hidden"}))
        .await
        .unwrap();
    assert_eq!(result["total_tasks"], 1);
    assert_eq!(result["scope"]["namespaces"], json!(["hidden"]));
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_empty_store_and_unknown_options_are_explicit() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    let before = snapshot(&runtime).await;
    let result = registry.dispatch("gtd.census", json!({})).await.unwrap();
    assert_eq!(result["total_tasks"], 0);
    assert_eq!(result["created_at"], json!(empty_histogram()));
    assert_eq!(result["archived_at"], json!(empty_histogram()));
    assert_eq!(result["created_at_gt_archived_at_raw"], 0);
    for args in [
        json!({"unit": "milliseconds"}),
        json!({"repair": true}),
        json!({"limit": 1}),
        Value::Null,
    ] {
        assert!(registry.dispatch("gtd.census", args).await.is_err());
    }
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_sql_classifies_nullable_legacy_created_at_without_schema_changes() {
    // Current notes.created_at is NOT NULL. A synthetic relation exercises the
    // same production query on legacy nulls without weakening that schema.
    let runtime = memory_runtime();
    let production = include_str!("../sql/task-timestamp-census.sql");
    let query = format!(
        "WITH notes(namespace,kind,deleted_at,created_at,properties) AS (VALUES \
         ('local','task',NULL,NULL,'{{\"archived_at\":0}}'), \
         ('local','task',NULL,0,'{{\"archived_at\":null}}')), {}",
        production
            .strip_prefix("WITH ")
            .expect("production CTE query")
    );
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: query,
            params: vec![SqlValue::Text("[\"local\"]".into())],
            label: Some("synthetic-legacy-census".into()),
        })
        .await
        .unwrap();
    let count = |field: &str, bucket: &str| {
        rows.iter()
            .find_map(
                |row| match (row.get("field"), row.get("bucket"), row.get("count")) {
                    (
                        Some(SqlValue::Text(actual_field)),
                        Some(SqlValue::Text(actual_bucket)),
                        Some(SqlValue::Integer(count)),
                    ) if actual_field == field && actual_bucket == bucket => Some(*count),
                    _ => None,
                },
            )
            .unwrap_or(0)
    };
    assert_eq!(count("created_at", "null"), 1);
    assert_eq!(count("created_at", "epoch_zero"), 1);
    assert_eq!(count("archived_at", "null"), 1);
    assert_eq!(count("archived_at", "epoch_zero"), 1);
    assert_eq!(count("comparison", "created_at_gt_archived_at_raw"), 0);
}

const CURRENT_MAGNITUDE: i64 = 1_700_000_000_000_000;

struct CandidateSeed<'a> {
    namespace: &'a str,
    id: Uuid,
    kind: &'a str,
    deleted: bool,
    created: SqlValue,
    updated: SqlValue,
    status: Option<Value>,
    archived: Option<Value>,
}

impl<'a> CandidateSeed<'a> {
    fn new(namespace: &'a str, ordinal: u128, created: SqlValue, updated: SqlValue) -> Self {
        Self {
            namespace,
            id: Uuid::from_u128(ordinal),
            kind: "task",
            deleted: false,
            created,
            updated,
            status: Some(json!("archived")),
            archived: None,
        }
    }
}

async fn seed_candidate(runtime: &KhiveRuntime, row: CandidateSeed<'_>) -> String {
    let mut properties = json!({"unrelated": "retain original evidence"});
    if let Some(status) = row.status {
        properties["status"] = status;
    }
    if let Some(archived) = row.archived {
        properties["archived_at"] = archived;
    }
    let id = row.id.to_string();
    runtime.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "INSERT INTO notes (id,namespace,kind,content,properties,created_at,updated_at,deleted_at) VALUES (?1,?2,?3,'candidate fixture',?4,?5,?6,?7)".into(),
        params: vec![
            SqlValue::Text(id.clone()),
            SqlValue::Text(row.namespace.into()),
            SqlValue::Text(row.kind.into()),
            SqlValue::Text(properties.to_string()),
            row.created,
            row.updated,
            if row.deleted { SqlValue::Integer(19) } else { SqlValue::Null },
        ],
        label: Some("seed-census-candidate".into()),
    }).await.unwrap();
    id
}

fn candidate_rows(result: &Value) -> &[Value] {
    result["candidates"]["rows"]
        .as_array()
        .expect("candidate rows")
}

#[tokio::test]
async fn census_candidates_are_opt_in_and_preserve_schema_one_aggregates() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    seed_candidate(
        &runtime,
        CandidateSeed::new(
            "local",
            1,
            SqlValue::Integer(0),
            SqlValue::Integer(CURRENT_MAGNITUDE),
        ),
    )
    .await;
    let before = snapshot(&runtime).await;
    let default = registry.dispatch("gtd.census", json!({})).await.unwrap();
    let disabled = registry
        .dispatch("gtd.census", json!({"include_candidates": false}))
        .await
        .unwrap();
    assert_eq!(default, disabled);
    assert_eq!(default["schema_version"], 1);
    assert_eq!(
        default
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "schema_version",
            "scope",
            "total_tasks",
            "created_at",
            "archived_at",
            "created_at_gt_archived_at_raw",
            "interpretation"
        ]
        .into_iter()
        .collect()
    );
    let mut included = registry
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap();
    assert_eq!(
        included["candidates"]["expected_buckets"],
        json!({
            "created_at": "magnitude_16_digits", "updated_at": "magnitude_16_digits"
        })
    );
    assert_eq!(candidate_rows(&included).len(), 1);
    assert_eq!(included["candidates"]["next_cursor"], Value::Null);
    included.as_object_mut().unwrap().remove("candidates");
    assert_eq!(
        included, default,
        "candidate paging must not change the aggregate census"
    );
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidates_preserve_raw_values_and_classify_each_timestamp() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    let cases = [
        (SqlValue::Integer(0), json!(0), "epoch_zero"),
        (
            SqlValue::Integer(1_700_000_000),
            json!(1_700_000_000),
            "magnitude_10_digits",
        ),
        (
            SqlValue::Integer(-1_700_000_000_000),
            json!(-1_700_000_000_000_i64),
            "magnitude_13_digits",
        ),
        (
            SqlValue::Float(1_000_000_000.5),
            json!(1_000_000_000.5),
            "magnitude_10_digits",
        ),
        (
            SqlValue::Text("not-a-time".into()),
            json!("not-a-time"),
            "nonnumeric",
        ),
        (SqlValue::Integer(17), json!(17), "other"),
        (SqlValue::Integer(i64::MIN), json!(i64::MIN), "other"),
        (
            SqlValue::Integer(10_000_000_000_000_000),
            json!(10_000_000_000_000_000_i64),
            "other",
        ),
    ];
    let archives = [
        (None, Value::Null, "null"),
        (Some(Value::Null), Value::Null, "null"),
        (Some(json!(0)), json!(0), "epoch_zero"),
        (
            Some(json!(1_700_000_000)),
            json!(1_700_000_000),
            "magnitude_10_digits",
        ),
        (
            Some(json!(1_700_000_000_000_i64)),
            json!(1_700_000_000_000_i64),
            "magnitude_13_digits",
        ),
        (
            Some(json!(CURRENT_MAGNITUDE)),
            json!(CURRENT_MAGNITUDE),
            "magnitude_16_digits",
        ),
        (
            Some(json!({"source": [1, true]})),
            json!({"source": [1, true]}),
            "nonnumeric",
        ),
        (Some(json!(17)), json!(17), "other"),
    ];
    let statuses = [
        Some(json!("archived")),
        Some(json!("done")),
        Some(json!("unknown-import-state")),
        Some(json!(true)),
        Some(json!({"original": "retained"})),
        None,
        Some(Value::Null),
        Some(json!(17)),
    ];
    assert_eq!(cases.len(), archives.len());
    assert_eq!(cases.len(), statuses.len());
    let mut expected = Vec::new();
    for (index, ((stored, raw, bucket), (archived, raw_archived, archive_bucket))) in
        cases.into_iter().zip(archives).enumerate()
    {
        for updated_anomaly in [false, true] {
            let (created, updated, raw_created, raw_updated, created_bucket, updated_bucket) =
                if updated_anomaly {
                    (
                        SqlValue::Integer(CURRENT_MAGNITUDE),
                        stored.clone(),
                        json!(CURRENT_MAGNITUDE),
                        raw.clone(),
                        "magnitude_16_digits",
                        bucket,
                    )
                } else {
                    (
                        stored.clone(),
                        SqlValue::Integer(CURRENT_MAGNITUDE),
                        raw.clone(),
                        json!(CURRENT_MAGNITUDE),
                        bucket,
                        "magnitude_16_digits",
                    )
                };
            let mut row =
                CandidateSeed::new("local", 100 + expected.len() as u128, created, updated);
            row.archived = archived.clone();
            row.status = statuses[index].clone();
            let id = seed_candidate(&runtime, row).await;
            expected.push(json!({
                "id": id, "namespace": "local", "stored_status": statuses[index].as_ref().map(Value::to_string),
                "raw": {"created_at": raw_created.to_string(), "updated_at": raw_updated.to_string(), "archived_at": archived.as_ref().map(|_| raw_archived.to_string())},
                "buckets": {"created_at": created_bucket, "updated_at": updated_bucket, "archived_at": archive_bucket}
            }));
        }
    }
    let before = snapshot(&runtime).await;
    let result = registry
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap();
    assert_eq!(candidate_rows(&result), expected);
    assert_eq!(result["candidates"]["next_cursor"], Value::Null);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidates_exclude_expected_magnitudes_even_with_archival_anomalies() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    for (index, archived) in [
        None,
        Some(Value::Null),
        Some(json!(0)),
        Some(json!(1_700_000_000)),
        Some(json!("1700000000000")),
        Some(json!(true)),
        Some(json!([1])),
    ]
    .into_iter()
    .enumerate()
    {
        let created = if index % 2 == 0 {
            1_000_000_000_000_000
        } else {
            -9_999_999_999_999_999
        };
        let mut row = CandidateSeed::new(
            "local",
            index as u128 + 1,
            SqlValue::Integer(created),
            SqlValue::Integer(CURRENT_MAGNITUDE),
        );
        row.archived = archived;
        seed_candidate(&runtime, row).await;
    }
    let before = snapshot(&runtime).await;
    let result = registry
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap();
    assert_eq!(result["total_tasks"], 7);
    assert!(
        candidate_rows(&result).is_empty(),
        "archive evidence has no approved expected unit"
    );
    assert_eq!(result["candidates"]["next_cursor"], Value::Null);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidates_keyset_pages_use_current_namespace_kind_and_live_scope() {
    let runtime = memory_runtime();
    let mut expected = Vec::new();
    for (ordinal, namespace, kind, deleted) in [
        (10, "local", "task", false),
        (20, "foreign", "task", false),
        (30, "local", "task", false),
        (40, "foreign", "task", false),
        (50, "hidden", "task", false),
        (60, "local", "observation", false),
        (70, "local", "task", true),
    ] {
        let mut row = CandidateSeed::new(
            namespace,
            ordinal,
            SqlValue::Integer(0),
            SqlValue::Integer(CURRENT_MAGNITUDE),
        );
        row.kind = kind;
        row.deleted = deleted;
        let id = seed_candidate(&runtime, row).await;
        if !deleted && kind == "task" && namespace != "hidden" {
            expected.push((namespace.to_owned(), id));
        }
    }
    expected.sort();
    let before = snapshot(&runtime).await;
    let visible = registry(&runtime, true);
    let first = visible
        .dispatch(
            "gtd.census",
            json!({"include_candidates": true, "limit": 2}),
        )
        .await
        .unwrap();
    assert_eq!(candidate_rows(&first).len(), 2);
    let cursor = first["candidates"]["next_cursor"].clone();
    assert_eq!(
        cursor,
        json!({"namespace": expected[1].0, "id": expected[1].1})
    );
    let second = visible
        .dispatch(
            "gtd.census",
            json!({"include_candidates": true, "limit": 2, "cursor": cursor}),
        )
        .await
        .unwrap();
    assert_eq!(candidate_rows(&second).len(), 2);
    assert_eq!(second["candidates"]["next_cursor"], Value::Null);
    let actual: Vec<_> = candidate_rows(&first)
        .iter()
        .chain(candidate_rows(&second))
        .map(|row| {
            (
                row["namespace"].as_str().unwrap().to_owned(),
                row["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        actual, expected,
        "strict tuple ordering must neither repeat nor skip rows"
    );
    assert_eq!(first["total_tasks"], second["total_tasks"]);

    let local = registry(&runtime, false)
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap();
    assert_eq!(candidate_rows(&local).len(), 2);
    assert!(candidate_rows(&local)
        .iter()
        .all(|row| row["namespace"] == "local"));
    let explicit = visible
        .dispatch(
            "gtd.census",
            json!({"namespace": "hidden", "include_candidates": true}),
        )
        .await
        .unwrap();
    assert_eq!(candidate_rows(&explicit).len(), 1);
    assert_eq!(candidate_rows(&explicit)[0]["namespace"], "hidden");
    assert!(
        visible
            .dispatch(
                "gtd.census",
                json!({"namespace": "local", "include_candidates": true, "cursor": cursor})
            )
            .await
            .is_err(),
        "a cursor must belong to the current narrowed scope"
    );
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidates_default_and_maximum_page_sizes_are_bounded() {
    let runtime = memory_runtime();
    for ordinal in 1..=205 {
        seed_candidate(
            &runtime,
            CandidateSeed::new(
                "local",
                ordinal,
                SqlValue::Integer(0),
                SqlValue::Integer(CURRENT_MAGNITUDE),
            ),
        )
        .await;
    }
    let registry = registry(&runtime, false);
    let before = snapshot(&runtime).await;
    let default = registry
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap();
    assert_eq!(candidate_rows(&default).len(), 100);
    let maximum = registry
        .dispatch(
            "gtd.census",
            json!({"include_candidates": true, "limit": 200}),
        )
        .await
        .unwrap();
    assert_eq!(candidate_rows(&maximum).len(), 200);
    let last = candidate_rows(&maximum).last().unwrap();
    assert_eq!(
        maximum["candidates"]["next_cursor"],
        json!({"namespace": last["namespace"], "id": last["id"]})
    );
    let tail = registry.dispatch("gtd.census", json!({"include_candidates": true, "limit": 200, "cursor": maximum["candidates"]["next_cursor"]})).await.unwrap();
    assert_eq!(candidate_rows(&tail).len(), 5);
    assert_eq!(tail["candidates"]["next_cursor"], Value::Null);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidates_reject_invalid_limits_cursors_and_non_opt_in_paging() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let cursor = json!({"namespace": "local", "id": id});
    let before = snapshot(&runtime).await;
    for args in [
        json!({"limit": 1}),
        json!({"cursor": cursor}),
        json!({"include_candidates": false, "limit": 1}),
        json!({"include_candidates": false, "cursor": cursor}),
        json!({"include_candidates": "true"}),
        json!({"include_candidates": null}),
        json!({"include_candidates": true, "limit": 0}),
        json!({"include_candidates": true, "limit": 201}),
        json!({"include_candidates": true, "limit": -1}),
        json!({"include_candidates": true, "limit": 1.5}),
        json!({"include_candidates": true, "limit": "1"}),
        json!({"include_candidates": true, "limit": null}),
        json!({"include_candidates": true, "cursor": null}),
        json!({"include_candidates": true, "cursor": {"namespace": "local", "id": "aaaaaaaa"}}),
        json!({"include_candidates": true, "cursor": {"namespace": "local", "id": id.replace('-', "")}}),
        json!({"include_candidates": true, "cursor": {"namespace": "local", "id": id.to_uppercase()}}),
        json!({"include_candidates": true, "cursor": {"namespace": "hidden", "id": id}}),
        json!({"include_candidates": true, "cursor": {"namespace": "local"}}),
        json!({"include_candidates": true, "cursor": {"id": id}}),
        json!({"include_candidates": true, "cursor": {"namespace": "local", "id": id, "offset": 1}}),
        json!({"include_candidates": true, "offset": 1}),
        json!({"include_candidates": true, "unit": "microseconds"}),
        json!({"include_candidates": true, "repair": true}),
    ] {
        assert!(
            registry.dispatch("gtd.census", args.clone()).await.is_err(),
            "must reject {args}"
        );
    }
    let empty = registry
        .dispatch(
            "gtd.census",
            json!({"include_candidates": true, "limit": 1, "cursor": cursor}),
        )
        .await
        .unwrap();
    assert!(
        candidate_rows(&empty).is_empty(),
        "a valid cursor is a position, not proof a row still exists"
    );
    assert_eq!(empty["candidates"]["next_cursor"], Value::Null);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidates_preserve_large_and_decimal_json_source_lexemes() {
    let runtime = memory_runtime();
    let registry = registry(&runtime, false);
    let sources = [
        ("18446744073709551616000000000000000001", "other"),
        (
            "1234567890.123456789012345678901234567890",
            "magnitude_10_digits",
        ),
    ];
    for (index, (raw, _)) in sources.iter().enumerate() {
        let properties = format!("{{\"status\":{raw},\"archived_at\":{raw}}}");
        runtime.sql().writer().await.unwrap().execute(SqlStatement {
            sql: "INSERT INTO notes (id,namespace,kind,content,properties,created_at,updated_at) VALUES (?1,'local','task','exact evidence fixture',?2,0,?3)".into(),
            params: vec![
                SqlValue::Text(Uuid::from_u128(index as u128 + 1).to_string()),
                SqlValue::Text(properties),
                SqlValue::Integer(CURRENT_MAGNITUDE),
            ],
            label: Some("seed-exact-census-json".into()),
        }).await.unwrap();
    }
    let before = snapshot(&runtime).await;
    let result = registry
        .dispatch("gtd.census", json!({"include_candidates": true}))
        .await
        .unwrap();
    assert_eq!(candidate_rows(&result).len(), sources.len());
    for (row, (raw, bucket)) in candidate_rows(&result).iter().zip(sources) {
        assert_eq!(
            row["stored_status"], raw,
            "status must not round-trip through a numeric JSON value"
        );
        assert_eq!(
            row["raw"]["archived_at"], raw,
            "retain the original numeric lexeme"
        );
        assert_eq!(row["raw"]["created_at"], "0");
        assert_eq!(row["raw"]["updated_at"], CURRENT_MAGNITUDE.to_string());
        assert_eq!(row["buckets"]["archived_at"], bucket);
    }
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn census_candidate_sql_classifies_nullable_legacy_columns_without_schema_changes() {
    let runtime = memory_runtime();
    let production = include_str!("../sql/task-timestamp-candidates.sql");
    let query = format!(
        "WITH notes(id,namespace,kind,deleted_at,created_at,updated_at,properties) AS (VALUES \
         ('00000000-0000-0000-0000-000000000001','local','task',NULL,NULL,1700000000000000,'{{\"status\":null,\"archived_at\":null}}'), \
         ('00000000-0000-0000-0000-000000000002','local','task',NULL,1700000000000000,NULL,'{{}}')), {}",
        production.strip_prefix("WITH ").expect("production CTE query")
    );
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: query,
            params: vec![
                SqlValue::Text("[\"local\"]".into()),
                SqlValue::Null,
                SqlValue::Null,
                SqlValue::Integer(3),
            ],
            label: Some("synthetic-nullable-census-candidates".into()),
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0].get("created_at"), Some(SqlValue::Null)));
    assert!(
        matches!(rows[0].get("created_bucket"), Some(SqlValue::Text(bucket)) if bucket == "null")
    );
    assert!(matches!(rows[1].get("updated_at"), Some(SqlValue::Null)));
    assert!(
        matches!(rows[1].get("updated_bucket"), Some(SqlValue::Text(bucket)) if bucket == "null")
    );
    assert!(
        matches!(rows[0].get("stored_status_json"), Some(SqlValue::Text(raw)) if raw == "null")
    );
    assert!(matches!(rows[0].get("archived_json"), Some(SqlValue::Text(raw)) if raw == "null"));
    assert!(matches!(
        rows[1].get("stored_status_json"),
        Some(SqlValue::Null)
    ));
    assert!(matches!(rows[1].get("archived_json"), Some(SqlValue::Null)));
}

#[tokio::test]
async fn census_candidates_refuse_unrepresentable_core_values_without_rewriting() {
    for value in [SqlValue::Blob(vec![0, 65]), SqlValue::Float(f64::INFINITY)] {
        for updated_anomaly in [false, true] {
            let runtime = memory_runtime();
            let registry = registry(&runtime, false);
            let (created, updated) = if updated_anomaly {
                (SqlValue::Integer(CURRENT_MAGNITUDE), value.clone())
            } else {
                (value.clone(), SqlValue::Integer(CURRENT_MAGNITUDE))
            };
            seed_candidate(&runtime, CandidateSeed::new("local", 1, created, updated)).await;
            let before = snapshot(&runtime).await;
            assert!(
                registry
                    .dispatch("gtd.census", json!({"include_candidates": true}))
                    .await
                    .is_err(),
                "an unrepresentable stored value must not become fabricated JSON null"
            );
            assert_eq!(snapshot(&runtime).await, before);
        }
    }
}
