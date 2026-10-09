use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError};
use khive_storage::{SqlStatement, SqlValue};
use uuid::Uuid;

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    runtime
}

async fn fixture() -> KhiveRuntime {
    let runtime = runtime();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "CREATE TABLE pack_prefix_rows (record_id TEXT NOT NULL, namespace TEXT, deleted_at INTEGER)",
            vec![],
        ))
        .await
        .unwrap();
    runtime
}

async fn seed(runtime: &KhiveRuntime, id: &str, namespace: &str, deleted_at: Option<i64>) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "INSERT INTO pack_prefix_rows (record_id, namespace, deleted_at) VALUES (?1, ?2, ?3)",
            vec![
                SqlValue::Text(id.into()),
                SqlValue::Text(namespace.into()),
                SqlValue::from_opt_i64(deleted_at),
            ],
        ))
        .await
        .unwrap();
}

async fn resolve(runtime: &KhiveRuntime, prefix: &str) -> Result<Option<Uuid>, RuntimeError> {
    runtime
        .resolve_prefix_in("pack_prefix_rows", "record_id", prefix)
        .await
}

#[tokio::test]
async fn full_uuid_is_a_lookup_and_unique_prefixes_use_canonical_bounds() {
    let runtime = fixture().await;
    let id = Uuid::parse_str("aabbccdd-1234-4000-8000-000000000001").unwrap();
    seed(&runtime, &id.to_string(), "local", None).await;
    let compact = id.simple().to_string();
    for input in [
        id.to_string(),
        compact.clone(),
        id.to_string().to_uppercase(),
        compact.to_uppercase(),
        compact[..8].to_owned(),
        compact[..9].to_uppercase(),
        compact[..20].to_owned(),
        "AABBCCDD-1234-4".into(),
    ] {
        assert_eq!(resolve(&runtime, &input).await.unwrap(), Some(id));
    }
    let absent = Uuid::parse_str("aabbccdd-1234-4000-8000-000000000002").unwrap();
    for input in [
        absent.to_string(),
        absent.simple().to_string(),
        "00112233".into(),
    ] {
        assert_eq!(resolve(&runtime, &input).await.unwrap(), None);
    }
}

#[tokio::test]
async fn carry_and_all_f_prefixes_keep_the_upper_bound_exclusive() {
    let runtime = fixture().await;
    let carry = Uuid::parse_str("abcdefff-1000-4000-8000-000000000001").unwrap();
    let next = Uuid::parse_str("abcdf000-1000-4000-8000-000000000002").unwrap();
    let all_f = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
    for id in [carry, next, all_f] {
        seed(&runtime, &id.to_string(), "local", None).await;
    }
    assert_eq!(resolve(&runtime, "abcdefff").await.unwrap(), Some(carry));
    assert_eq!(resolve(&runtime, "abcdf000").await.unwrap(), Some(next));
    for input in [
        "FFFFFFFF".to_owned(),
        all_f.to_string(),
        all_f.simple().to_string(),
    ] {
        assert_eq!(resolve(&runtime, &input).await.unwrap(), Some(all_f));
    }
}

#[tokio::test]
async fn ambiguity_counts_distinct_ids_before_limiting_and_orders_its_sample() {
    let runtime = fixture().await;
    let first = Uuid::parse_str("aabbccdd-1000-4000-8000-000000000001").unwrap();
    let second = Uuid::parse_str("aabbccdd-2000-4000-8000-000000000002").unwrap();
    let third = Uuid::parse_str("aabbccdd-3000-4000-8000-000000000003").unwrap();
    seed(&runtime, &first.to_string(), "local", None).await;
    seed(&runtime, &first.to_string(), "duplicate", Some(1)).await;
    assert_eq!(resolve(&runtime, "aabbccdd").await.unwrap(), Some(first));
    for id in [third, second] {
        seed(&runtime, &id.to_string(), "local", None).await;
    }
    match resolve(&runtime, "AABBCCDD").await.unwrap_err() {
        RuntimeError::AmbiguousPrefix { prefix, matches } => {
            assert_eq!(prefix, "AABBCCDD");
            assert_eq!(matches, vec![first, second]);
        }
        error => panic!("expected distinct-ID ambiguity, got {error:?}"),
    }
}

#[tokio::test]
async fn namespace_and_tombstones_are_included_without_a_selection_policy() {
    let runtime = fixture().await;
    let foreign = Uuid::parse_str("aabbccdd-1000-4000-8000-000000000001").unwrap();
    let deleted = Uuid::parse_str("bbccddee-1000-4000-8000-000000000002").unwrap();
    seed(&runtime, &foreign.to_string(), "other", None).await;
    seed(&runtime, &deleted.to_string(), "local", Some(1)).await;
    assert_eq!(resolve(&runtime, "aabbccdd").await.unwrap(), Some(foreign));
    assert_eq!(resolve(&runtime, "bbccddee").await.unwrap(), Some(deleted));

    let collision = Uuid::parse_str("aabbccdd-2000-4000-8000-000000000003").unwrap();
    seed(&runtime, &collision.to_string(), "local", Some(1)).await;
    assert!(matches!(
        resolve(&runtime, "aabbccdd").await,
        Err(RuntimeError::AmbiguousPrefix { matches, .. }) if matches == vec![foreign, collision]
    ));
}

#[tokio::test]
async fn quoted_identifiers_work_without_namespace_or_tombstone_columns() {
    let runtime = runtime();
    let id = Uuid::parse_str("aabbccdd-1000-4000-8000-000000000001").unwrap();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_batch(vec![
            SqlStatement::new("CREATE TABLE \"select\" (\"from\" TEXT NOT NULL)", vec![]),
            SqlStatement::new(
                "INSERT INTO \"select\" (\"from\") VALUES (?1)",
                vec![SqlValue::Text(id.to_string())],
            ),
        ])
        .await
        .unwrap();
    assert_eq!(
        runtime
            .resolve_prefix_in("select", "from", "aabbccdd")
            .await
            .unwrap(),
        Some(id)
    );
    // A qualified nonexistent column must error, never become a quoted string.
    assert!(matches!(
        runtime
            .resolve_prefix_in("select", "missing", "aabbccdd")
            .await,
        Err(RuntimeError::Storage(_))
    ));
}

#[tokio::test]
async fn invalid_inputs_do_not_query_and_valid_missing_tables_propagate_storage_errors() {
    let runtime = runtime();
    // A valid lookup proves this table is absent; malformed prefixes must return
    // before reaching that same failing query, not turn database errors into None.
    assert!(matches!(
        runtime
            .resolve_prefix_in("absent_pack_table", "id", "aabbccdd")
            .await,
        Err(RuntimeError::Storage(_))
    ));
    for prefix in [
        "",
        "aabbccdd%",
        "aabbccdd_",
        "aabbccdd'",
        " aabbccdd",
        "aabbccdd\n",
        "aabb-ccdd",
        "gabbccdd",
        "éabcdef0",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert_eq!(
            runtime
                .resolve_prefix_in("absent_pack_table", "id", prefix)
                .await
                .unwrap(),
            None
        );
    }
    for identifier in [
        "",
        "1table",
        "a b",
        "main.entities",
        "id\"",
        "id; SELECT 1",
        "é",
        "id\0",
    ] {
        for (table, column) in [(identifier, "id"), ("absent_pack_table", identifier)] {
            assert!(matches!(
                runtime.resolve_prefix_in(table, column, "aabbccdd").await,
                Err(RuntimeError::InvalidInput(_))
            ));
        }
    }
    let full_id = "aabbccdd-1000-4000-8000-000000000001";
    assert!(matches!(
        runtime
            .resolve_prefix_in("absent_pack_table", "id", full_id)
            .await,
        Err(RuntimeError::Storage(_))
    ));
}

#[tokio::test]
async fn matching_malformed_stored_uuid_is_not_silently_skipped() {
    let runtime = fixture().await;
    seed(&runtime, "aabbccdd-not-a-uuid", "local", None).await;
    assert!(matches!(
        resolve(&runtime, "aabbccdd").await,
        Err(RuntimeError::Internal(_))
    ));
    seed(
        &runtime,
        "aabbccdd-1000-4000-8000-000000000001",
        "local",
        None,
    )
    .await;
    assert!(matches!(
        resolve(&runtime, "aabbccdd").await,
        Err(RuntimeError::Internal(_))
    ));
}
