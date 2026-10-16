use super::*;
use khive_runtime::{RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::SqlColumn;
use serde_json::{json, Value};

async fn fixture() -> (KhiveRuntime, VerbRegistry, Uuid) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        actor_id: None,
        brain_profile: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        events_split: None,
        mounts: Vec::new(),
        packs: vec!["kg".into(), "git".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert!(!runtime.backend().is_file_backed());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::GitPack::new(runtime.clone()));
    builder.with_runtime_event_store(&runtime).unwrap();
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(runtime.backend());
    let project = registry
        .dispatch("create", json!({"kind":"project","name":"cursor snapshot"}))
        .await
        .unwrap();
    let project = Uuid::parse_str(project["id"].as_str().unwrap()).unwrap();
    (runtime, registry, project)
}

async fn store(runtime: &KhiveRuntime, project: Uuid, kind: &str, value: SqlValue) {
    runtime.sql().writer().await.unwrap().execute(SqlStatement::new(
        "INSERT INTO git_mirror_cursor(project_id,kind,cursor_value,updated_at) VALUES(?1,?2,?3,42) \
         ON CONFLICT(project_id,kind) DO UPDATE SET cursor_value=excluded.cursor_value",
        vec![SqlValue::Text(project.to_string()), SqlValue::Text(kind.into()), value],
    )).await.unwrap();
}

async fn inspect(registry: &VerbRegistry, project: Uuid) -> Value {
    registry
        .dispatch(
            "git.ingest_cursor",
            json!({"project":project,"source_kind":"commits"}),
        )
        .await
        .unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn shared_snapshot_bounds_bytes_and_selects_one_ordered_project_pair() {
    let (runtime, _, project) = fixture().await;
    store(
        &runtime,
        project,
        "commits_checkpoint",
        SqlValue::Text("文a".into()),
    )
    .await;
    store(
        &runtime,
        project,
        "commits",
        SqlValue::Blob(vec![255, 0, 128]),
    )
    .await;
    store(
        &runtime,
        project,
        "issues",
        SqlValue::Text("unrelated".into()),
    )
    .await;
    store(
        &runtime,
        Uuid::new_v4(),
        "commits",
        SqlValue::Text("foreign".into()),
    )
    .await;
    let statement = snapshot::statement(
        &project.to_string(),
        "commits",
        "commits_checkpoint",
        3,
        "bounded snapshot",
    );
    assert_eq!(statement.label.as_deref(), Some("bounded snapshot"));
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(statement)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].text("kind").unwrap(), "commits");
    assert_eq!(rows[1].text("kind").unwrap(), "commits_checkpoint");
    assert_eq!(snapshot::value_bytes(&rows[0]).unwrap(), Some(3));
    assert_eq!(
        snapshot::value(&rows[0]).unwrap(),
        Some([255, 0, 128].as_slice())
    );
    assert_eq!(snapshot::value_bytes(&rows[1]).unwrap(), Some(4));
    assert_eq!(snapshot::value(&rows[1]).unwrap(), None);
    assert_eq!(rows[1].text("value_type").unwrap(), "text");
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(snapshot::statement(
            &project.to_string(),
            "commits' OR 1=1 --",
            "absent",
            3,
            "bound kind",
        ))
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn inspection_and_repair_keep_their_distinct_size_policies() {
    let (runtime, registry, project) = fixture().await;
    store(&runtime, project, "commits_checkpoint", SqlValue::Null).await;
    for size in [8192, 8193, 262_144, 262_145] {
        let value = "x".repeat(size);
        store(&runtime, project, "commits", SqlValue::Text(value.clone())).await;
        let repair = cursor_snapshot(&runtime, project).await;
        if size == 8192 {
            let rows = repair.unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].value.as_deref(), Some(value.as_bytes()));
            assert_eq!(rows[1].value, None);
            assert_eq!(rows[1].value_type, "null");
        } else {
            assert_eq!(
                repair.unwrap_err().to_string(),
                "stored commits cursor exceeds the repair size limit"
            );
        }
        let result = inspect(&registry, project).await;
        assert_eq!(result["cursor"]["value_bytes"], size);
        assert_eq!(result["cursor"]["truncated"], size > 262_144);
        assert_eq!(
            result["cursor"]["value"],
            if size > 262_144 {
                Value::Null
            } else {
                json!(value)
            }
        );
        assert_eq!(
            result["checkpoint"],
            json!({"value":null,"updated_at":42,"value_bytes":null,"truncated":false})
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn repair_keeps_raw_bytes_in_preview_input_while_inspection_refuses_nontext() {
    let (runtime, registry, project) = fixture().await;
    store(&runtime, project, "commits_checkpoint", SqlValue::Null).await;
    store(
        &runtime,
        project,
        "commits",
        SqlValue::Blob(vec![0, 255, 128]),
    )
    .await;
    let rows = cursor_snapshot(&runtime, project).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&rows).unwrap().as_slice(),
        br#"[{"kind":"commits","updated_at":42,"value_type":"blob","value":[0,255,128]},{"kind":"commits_checkpoint","updated_at":42,"value_type":"null","value":null}]"#.as_slice(),
    );
    assert_eq!(
        cursor_text(&rows, "commits").unwrap_err().to_string(),
        "stored commits cursor is not text"
    );
    for value in [vec![0, 255, 128], vec![0; 262_145]] {
        store(&runtime, project, "commits", SqlValue::Blob(value)).await;
        let error = registry
            .dispatch(
                "git.ingest_cursor",
                json!({"project":project,"source_kind":"commits"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, khive_runtime::RuntimeError::InvalidInput(message)
            if message == "stored ingest cursor row has invalid column types or text encoding")
        );
    }
}

#[test]
fn repair_rejects_malformed_columns_in_the_original_order() {
    let mut row = SqlRow {
        columns: ["kind", "value_type", "value_bytes", "value", "updated_at"]
            .into_iter()
            .map(|name| SqlColumn {
                name: name.into(),
                value: SqlValue::Null,
            })
            .collect(),
    };
    assert_eq!(
        decode_cursor_row(&row).unwrap_err().to_string(),
        "stored kind has an invalid type"
    );
    row.columns[0].value = SqlValue::Text("commits".into());
    assert_eq!(
        decode_cursor_row(&row).unwrap_err().to_string(),
        "stored value_type has an invalid type"
    );
    row.columns[1].value = SqlValue::Text("text".into());
    row.columns[2].value = SqlValue::Integer(-1);
    row.columns[3].value = SqlValue::Integer(7);
    assert_eq!(
        decode_cursor_row(&row).unwrap_err().to_string(),
        "stored cursor length has an invalid type"
    );
    row.columns[2].value = SqlValue::Integer(CURSOR_MAX_BYTES + 1);
    assert_eq!(
        decode_cursor_row(&row).unwrap_err().to_string(),
        "stored commits cursor exceeds the repair size limit"
    );
    row.columns[2].value = SqlValue::Integer(0);
    assert_eq!(
        decode_cursor_row(&row).unwrap_err().to_string(),
        "stored cursor value has an invalid type"
    );
    row.columns[3].value = SqlValue::Blob(Vec::new());
    assert_eq!(
        decode_cursor_row(&row).unwrap_err().to_string(),
        "stored updated_at has an invalid type"
    );
    row.columns[4].value = SqlValue::Integer(42);
    let decoded = decode_cursor_row(&row).unwrap();
    assert_eq!(decoded.updated_at, 42);
    assert_eq!(decoded.value, Some(Vec::new()));
}
