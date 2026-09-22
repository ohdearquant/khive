use std::collections::BTreeMap;

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

fn registry(runtime: &KhiveRuntime, extra_namespace: bool) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
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
    let runtime = KhiveRuntime::memory().unwrap();
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
    let runtime = KhiveRuntime::memory().unwrap();
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
    let runtime = KhiveRuntime::memory().unwrap();
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
    let runtime = KhiveRuntime::memory().unwrap();
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
