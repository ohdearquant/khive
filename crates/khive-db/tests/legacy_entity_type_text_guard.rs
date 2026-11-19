use khive_db::StorageBackend;
use khive_storage::entity::EntityFilter;
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn count_free_legacy_type_fallback_excludes_json_arrays() {
    let backend = StorageBackend::memory().expect("in-memory backend");
    backend.prepare_core_schema().expect("prepare core schema");
    let namespace = "legacy_type_text_guard";
    let store = backend
        .entities_for_namespace(namespace)
        .expect("entity store");
    let array_id = Uuid::from_u128(1);
    let text_id = Uuid::from_u128(2);
    let fixtures = [
        (array_id, json!({"type": ["a"]})),
        (text_id, json!({"type": "[\"a\"]"})),
    ];
    let sql = backend.sql();

    // Store upsert normalizes entity_type, so seed the legacy NULL directly.
    {
        let mut writer = sql.writer().await.expect("fixture writer");
        for (id, properties) in &fixtures {
            let inserted = writer
                .execute(SqlStatement {
                    sql: "INSERT INTO entities \
                          (id, namespace, kind, entity_type, name, properties, \
                          created_at, updated_at) \
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)"
                        .into(),
                    params: vec![
                        SqlValue::Text(id.to_string()),
                        SqlValue::Text(namespace.into()),
                        SqlValue::Text("concept".into()),
                        SqlValue::Null,
                        SqlValue::Text("legacy fixture".into()),
                        SqlValue::Text(properties.to_string()),
                        SqlValue::Integer(1),
                    ],
                    label: None,
                })
                .await
                .expect("insert legacy fixture");
            assert_eq!(inserted, 1);
        }
    }

    {
        let mut reader = sql.reader().await.expect("fixture reader");
        for (id, properties) in &fixtures {
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT entity_type, properties FROM entities WHERE id = ?1".into(),
                    params: vec![SqlValue::Text(id.to_string())],
                    label: None,
                })
                .await
                .expect("read legacy fixture")
                .expect("legacy fixture exists");
            assert!(matches!(row.get("entity_type"), Some(SqlValue::Null)));
            let stored: serde_json::Value =
                serde_json::from_str(row.text("properties").expect("properties text"))
                    .expect("valid fixture JSON");
            assert_eq!(&stored, properties);
        }
    }

    let page = store
        .query_entities_count_free(
            namespace,
            EntityFilter {
                entity_types: vec![r#"["a"]"#.into()],
                legacy_entity_type_fallback: true,
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("count-free legacy type query");

    assert_eq!(page.total, None);
    assert_eq!(
        page.items
            .iter()
            .map(|entity| entity.id)
            .collect::<Vec<_>>(),
        vec![text_id],
        "the string-valued legacy type must match; the array-valued type must not"
    );
}
