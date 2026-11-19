use khive_runtime::{EmbeddingModelRecord, EmbeddingModelStatus, KhiveRuntime};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;
use uuid::Uuid;

async fn execute(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(sql, params))
        .await
        .unwrap();
}

fn values(id: Uuid, engine: &str, status: &str, activated_at: Option<i64>) -> Vec<SqlValue> {
    vec![
        SqlValue::Blob(id.as_bytes().to_vec()),
        SqlValue::Text(engine.into()),
        SqlValue::Text("stored-model".into()),
        SqlValue::Text("stored-key-version".into()),
        SqlValue::Integer(384),
        SqlValue::Null,
        SqlValue::Text(status.into()),
        SqlValue::from_opt_i64(activated_at),
        SqlValue::Null,
        SqlValue::Null,
        SqlValue::Blob(id.as_bytes().to_vec()),
        SqlValue::Integer(123),
    ]
}

async fn insert(runtime: &KhiveRuntime, values: Vec<SqlValue>) {
    execute(
        runtime,
        "INSERT INTO _embedding_models \
         (id, engine_name, model_id, key_version, dim, output_dim, status, activated_at, \
          superseded_at, superseded_by, canonical_key, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        values,
    )
    .await;
}

#[tokio::test]
async fn registered_model_returns_the_runtime_owned_record_and_active_status() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime
        .backend()
        .register_embedding_model("fixture-engine", "fixture-model", "v1", 384)
        .unwrap();
    let records: Vec<EmbeddingModelRecord> = runtime.list_embedding_models(None).await.unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.engine_name, "fixture-engine");
    assert_eq!(record.model_id, "fixture-model");
    assert_eq!(record.key_version, "v1");
    assert_eq!(record.dim, 384);
    assert_eq!(record.output_dim, None);
    assert_eq!(record.status, EmbeddingModelStatus::Active);
    assert_eq!(
        serde_json::to_value(&record.status).unwrap(),
        json!("active")
    );
    assert_eq!(record.canonical_key, b"fixture-engine:fixture-model:v1:384");
    assert!(!record.id.is_nil());
    assert!(record.created_at > 0);
    assert_eq!(record.activated_at, Some(record.created_at));
    assert_eq!(record.superseded_at, None);
    assert_eq!(record.superseded_by, None);

    let again = runtime.list_embedding_models(None).await.unwrap();
    assert_eq!(
        again[0].id, record.id,
        "reads must not synthesize another UUID"
    );
    assert_eq!(again[0].canonical_key, record.canonical_key);
}

#[tokio::test]
async fn all_twelve_fields_round_trip_stored_identity_and_history() {
    let runtime = KhiveRuntime::memory().unwrap();
    let id = Uuid::from_u128(0x0123456789abcdef0123456789abcdef);
    let replacement = Uuid::from_u128(0xabcdef0123456789abcdef0123456789);
    let canonical_key = vec![0, 255, 1, 128, 0, 42];
    let mut row = values(id, "engine", "superseded", Some(456));
    row[5] = SqlValue::Integer(192);
    row[8] = SqlValue::Integer(789);
    row[9] = SqlValue::Blob(replacement.as_bytes().to_vec());
    row[10] = SqlValue::Blob(canonical_key.clone());
    insert(&runtime, row).await;

    let records = runtime.list_embedding_models(Some("engine")).await.unwrap();
    assert_eq!(records.len(), 1);
    let expected = json!({
        "id": id,
        "engine_name": "engine",
        "model_id": "stored-model",
        "key_version": "stored-key-version",
        "dim": 384,
        "output_dim": 192,
        "status": "superseded",
        "activated_at": 456,
        "superseded_at": 789,
        "superseded_by": replacement,
        "canonical_key": canonical_key,
        "created_at": 123,
    });
    assert_eq!(serde_json::to_value(&records[0]).unwrap(), expected);
    let decoded: EmbeddingModelRecord = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded.clone()).unwrap(), expected);
    assert_eq!(decoded.id, id);
    assert_eq!(decoded.superseded_by, Some(replacement));
}

#[tokio::test]
async fn lifecycle_states_filter_and_existing_order_are_preserved() {
    let runtime = KhiveRuntime::memory().unwrap();
    // Deliberately insert out of the query's engine/activation order.
    for (id, engine, state, activated) in [
        (5, "beta", "active", Some(0)),
        (4, "alpha", "archived", None),
        (3, "alpha", "pending", Some(30)),
        (2, "alpha", "active", Some(20)),
        (1, "alpha", "superseded", Some(10)),
    ] {
        insert(
            &runtime,
            values(Uuid::from_u128(id), engine, state, activated),
        )
        .await;
    }
    let all = runtime.list_embedding_models(None).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.id.as_u128()).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    let alpha = runtime.list_embedding_models(Some("alpha")).await.unwrap();
    assert_eq!(alpha.len(), 4);
    for (record, status, spelling) in [
        (&alpha[0], EmbeddingModelStatus::Superseded, "superseded"),
        (&alpha[1], EmbeddingModelStatus::Active, "active"),
        (&alpha[2], EmbeddingModelStatus::Pending, "pending"),
        (&alpha[3], EmbeddingModelStatus::Archived, "archived"),
    ] {
        assert_eq!(record.status, status);
        assert_eq!(record.status.as_str(), spelling);
        assert_eq!(
            serde_json::to_value(&record.status).unwrap(),
            json!(spelling)
        );
        assert_eq!(
            serde_json::from_value::<EmbeddingModelStatus>(json!(spelling)).unwrap(),
            status
        );
        assert_eq!(record.output_dim, None);
        assert_eq!(record.superseded_by, None);
    }
    assert!(runtime
        .list_embedding_models(Some("missing"))
        .await
        .unwrap()
        .is_empty());
    assert!(runtime
        .list_embedding_models(Some("alpha' OR 1=1 --"))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn malformed_rows_are_skipped_without_losing_valid_neighbors() {
    let runtime = KhiveRuntime::memory().unwrap();
    // Model an old/corrupted registry without CHECK constraints or type affinity,
    // so every SqlValue reaches the real query decoder unchanged.
    execute(&runtime, "DROP TABLE _embedding_models", vec![]).await;
    execute(
        &runtime,
        "CREATE TABLE _embedding_models \
        (id, engine_name, model_id, key_version, dim, output_dim, status, \
         activated_at, superseded_at, superseded_by, canonical_key, created_at)",
        vec![],
    )
    .await;
    insert(
        &runtime,
        values(Uuid::from_u128(1), "engine", "active", Some(0)),
    )
    .await;
    let corruptions = [
        (0, SqlValue::Blob(vec![1; 15])),
        (0, SqlValue::Text(Uuid::from_u128(2).to_string())),
        (0, SqlValue::Null),
        (1, SqlValue::Blob(b"engine".to_vec())),
        (2, SqlValue::Null),
        (3, SqlValue::Integer(7)),
        (4, SqlValue::Integer(-1)),
        (4, SqlValue::Integer(i64::from(u32::MAX) + 1)),
        (4, SqlValue::Float(384.0)),
        (5, SqlValue::Integer(-1)),
        (5, SqlValue::Integer(i64::from(u32::MAX) + 1)),
        (5, SqlValue::Text("192".into())),
        (6, SqlValue::Text("unknown".into())),
        (6, SqlValue::Text("ACTIVE".into())),
        (9, SqlValue::Blob(vec![2; 17])),
        (9, SqlValue::Text(Uuid::from_u128(2).to_string())),
        (10, SqlValue::Text("fabricated-key".into())),
        (11, SqlValue::Null),
        (11, SqlValue::Text("123".into())),
    ];
    for (index, (column, bad_value)) in corruptions.into_iter().enumerate() {
        let mut row = values(
            Uuid::from_u128(index as u128 + 2),
            "engine",
            "active",
            Some(10),
        );
        row[column] = bad_value;
        insert(&runtime, row).await;
    }
    insert(
        &runtime,
        values(Uuid::from_u128(100), "engine", "archived", Some(20)),
    )
    .await;
    let records = runtime.list_embedding_models(None).await.unwrap();
    assert_eq!(
        records.iter().map(|r| r.id.as_u128()).collect::<Vec<_>>(),
        [1, 100]
    );
    assert_eq!(records[0].status, EmbeddingModelStatus::Active);
    assert_eq!(records[1].status, EmbeddingModelStatus::Archived);
}

#[tokio::test]
async fn optional_timestamp_compatibility_and_unsigned_dimensions_remain() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut row = values(Uuid::from_u128(1), "compatibility", "active", None);
    row[4] = SqlValue::Integer(0); // Existing checked-u32 policy did not require positivity.
    row[5] = SqlValue::Integer(i64::from(u32::MAX));
    row[7] = SqlValue::Text("legacy timestamp".into());
    row[8] = SqlValue::Blob(vec![1, 2]);
    insert(&runtime, row).await;
    let records = runtime.list_embedding_models(None).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].dim, 0);
    assert_eq!(records[0].output_dim, Some(u32::MAX));
    assert_eq!(records[0].activated_at, None);
    assert_eq!(records[0].superseded_at, None);
}

#[tokio::test]
async fn absent_table_is_empty_but_other_query_failures_propagate() {
    let runtime = KhiveRuntime::memory().unwrap();
    assert!(runtime
        .list_embedding_models(None)
        .await
        .unwrap()
        .is_empty());
    execute(&runtime, "DROP TABLE _embedding_models", vec![]).await;
    assert!(runtime
        .list_embedding_models(None)
        .await
        .unwrap()
        .is_empty());
    execute(
        &runtime,
        "CREATE TABLE _embedding_models (unrelated TEXT)",
        vec![],
    )
    .await;
    let error = runtime.list_embedding_models(None).await.unwrap_err();
    assert!(
        matches!(error, khive_runtime::RuntimeError::Storage(_)),
        "{error:?}"
    );
}
