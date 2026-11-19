use super::*;
use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::types::SqlColumn;

fn fixture() -> (KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(khive_pack_blob::BlobPack::new(rt.clone()));
    builder.register(khive_pack_tool::ToolPack::new(rt.clone()));
    builder.register(crate::ExecPack::new(rt.clone()));
    builder.build().unwrap().apply_schema_plans(rt.backend());
    let token = rt.authorize(Namespace::local()).unwrap();
    (rt, token)
}

#[test]
fn lifecycle_page_bounds_satisfy_transport_timestamp_validation() {
    let query = lifecycle_page_query();
    assert!(chrono::DateTime::from_timestamp_micros(query.since_us).is_some());
    assert!(chrono::DateTime::from_timestamp_micros(query.until_us).is_some());
    assert!(query.since_us < 0 && query.until_us > 0);
}

fn lifecycle(run: Uuid, id: u128, at: i64, detail: Value) -> Event {
    let mut event = Event::new(
        "local",
        "exec.run",
        EventKind::Audit,
        SubstrateKind::Event,
        "fixture",
    )
    .with_target(run)
    .with_payload(json!({"phase": "materialized", "detail": detail}));
    event.id = Uuid::from_u128(id);
    event.created_at = at;
    event
}

async fn legacy(rt: &KhiveRuntime, ns: &str, id: i64, run: &str, at: i64, detail: &str) {
    rt.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "INSERT INTO exec_events (id, namespace, run_id, kind, at, detail) VALUES (?1, ?2, ?3, 'materialized', ?4, ?5)".into(),
        params: vec![SqlValue::Integer(id), SqlValue::Text(ns.into()), SqlValue::Text(run.into()), SqlValue::Integer(at), SqlValue::Text(detail.into())],
        label: Some("test_legacy_exec_event".into()),
    }).await.unwrap();
}

#[tokio::test]
async fn lifecycle_append_precedes_receipt_and_uses_canonical_anonymous_actor() {
    let (rt, token) = fixture();
    assert!(token.actor().is_anonymous());
    let run = Uuid::new_v4();
    let detail = json!({"run_dir": "/fixture", "entries": 2});
    event(
        &rt,
        &token,
        &run.to_string(),
        "materialized",
        detail.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(
        get(&rt, "local", &run.to_string()).await,
        Err(RuntimeError::NotFound(_))
    ));
    let stored = rt
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                target_id: Some(run),
                kinds: vec![EventKind::Audit],
                verbs: vec!["exec.run".into()],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].substrate, SubstrateKind::Event);
    assert_eq!(
        stored[0].actor,
        format!("{}:{}", token.actor().kind, token.actor().id)
    );
    assert_eq!(
        stored[0].payload,
        json!({"phase": "materialized", "detail": detail})
    );
    let receipt = json!({"id": run, "actor": "caller-claimed", "tool": "sh", "seq": null});
    insert(&rt, "local", &receipt).await.unwrap();
    assert_eq!(get(&rt, "local", &run.to_string()).await.unwrap(), receipt);
    assert_ne!(stored[0].actor, "caller-claimed");
    let rows = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT id FROM exec_events".into(),
            params: Vec::new(),
            label: Some("test_no_legacy_exec_write".into()),
        })
        .await
        .unwrap();
    assert!(rows.is_empty());
    let wire = events(&rt, &token, Some(&run.to_string()), 200)
        .await
        .unwrap();
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0]["id"], stored[0].id.to_string());
    assert_eq!(wire[0]["detail"], detail);
}

#[tokio::test]
async fn events_merge_time_and_string_id_before_limit_and_keep_legacy_filters() {
    let (rt, token) = fixture();
    let run = Uuid::new_v4();
    legacy(&rt, "local", 2, &run.to_string(), 20, "{\"legacy\":2}").await;
    legacy(&rt, "local", 10, &run.to_string(), 20, "invalid json").await;
    legacy(&rt, "local", 99, "historical-run", 5, "null").await;
    let new = lifecycle(run, 1, 20, json!({"new": true}));
    let new_id = new.id.to_string();
    rt.events(&token)
        .unwrap()
        .append_event(new.clone())
        .await
        .unwrap();
    let rows = events(&rt, &token, None, 3).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["99", new_id.as_str(), "10"]
    );
    assert!(rows[2]["detail"].is_null());
    let filtered = events(&rt, &token, Some(&run.to_string()), 3)
        .await
        .unwrap();
    assert_eq!(
        filtered
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![new_id.as_str(), "10", "2"]
    );
    let historical = events(&rt, &token, Some("historical-run"), 200)
        .await
        .unwrap();
    assert_eq!(historical.len(), 1);
    assert_eq!(historical[0]["id"], "99");
    assert!(events(&rt, &token, Some("absent-non-uuid"), 200)
        .await
        .unwrap()
        .is_empty());
    legacy(&rt, "local", 20, "legacy-ties", 30, "null").await;
    legacy(&rt, "local", 100, "legacy-ties", 30, "null").await;
    let limited = events(&rt, &token, Some("legacy-ties"), 1).await.unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0]["id"], "100");
}

#[tokio::test]
async fn events_exclude_dispatch_audits_and_other_scopes() {
    let (rt, token) = fixture();
    let run = Uuid::new_v4();
    let wanted = lifecycle(run, 1, 10, json!({"kept": true}));
    let mut dispatch = lifecycle(run, 2, 10, Value::Null);
    dispatch.payload = json!({"params": {"actor": "local"}, "result": "ok"});
    let mut phase = lifecycle(run, 3, 10, Value::Null);
    phase.payload["phase"] = json!("unknown");
    let mut verb = lifecycle(run, 4, 10, Value::Null);
    verb.verb = "other.run".into();
    let mut substrate = lifecycle(run, 5, 10, Value::Null);
    substrate.substrate = SubstrateKind::Note;
    let mut missing_detail = lifecycle(run, 6, 10, Value::Null);
    missing_detail.payload = json!({"phase": "exited"});
    let other_run = lifecycle(Uuid::new_v4(), 7, 10, Value::Null);
    rt.events(&token)
        .unwrap()
        .append_events(vec![
            wanted.clone(),
            dispatch,
            phase,
            verb,
            substrate,
            missing_detail,
            other_run,
        ])
        .await
        .unwrap();
    let other = rt
        .authorize(Namespace::parse("lambda:elsewhere").unwrap())
        .unwrap();
    rt.events(&other)
        .unwrap()
        .append_event(lifecycle(run, 8, 10, Value::Null))
        .await
        .unwrap();
    legacy(&rt, "lambda:elsewhere", 1, &run.to_string(), 1, "null").await;
    let rows = events(&rt, &token, Some(&run.to_string()), 200)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], wanted.id.to_string());
    assert_eq!(events(&rt, &token, None, 200).await.unwrap().len(), 2);
}

#[tokio::test]
async fn events_fill_five_thousand_after_a_page_without_lifecycle_rows() {
    let (rt, token) = fixture();
    let run = Uuid::new_v4();
    let store = rt.events(&token).unwrap();
    for chunk in (0..9097_u128).collect::<Vec<_>>().chunks(256) {
        let rows = chunk
            .iter()
            .map(|&index| {
                let mut row = lifecycle(run, index + 1, index as i64, json!({"index": index}));
                if index < 4096 {
                    row.verb = "unrelated.audit".into();
                }
                row
            })
            .collect();
        let summary = store.append_events(rows).await.unwrap();
        assert_eq!(summary.failed, 0);
    }
    let rows = events(&rt, &token, Some(&run.to_string()), 5000)
        .await
        .unwrap();
    assert_eq!(rows.len(), 5000);
    assert_eq!(rows.first().unwrap()["detail"]["index"], 4096);
    assert_eq!(rows.last().unwrap()["detail"]["index"], 9095);
    assert!(rows.iter().all(|row| row["id"].is_string()));
}

#[tokio::test]
async fn events_resume_nonempty_byte_budget_prefix_and_refuse_an_unreadable_row() {
    let (rt, token) = fixture();
    let run = Uuid::new_v4();
    let store = rt.events(&token).unwrap();
    for id in 1..=2 {
        store
            .append_event(lifecycle(
                run,
                id,
                id as i64,
                json!({"data": "x".repeat(600 * 1024)}),
            ))
            .await
            .unwrap();
    }
    let rows = events(&rt, &token, None, 2).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], Uuid::from_u128(1).to_string());
    assert_eq!(rows[1]["id"], Uuid::from_u128(2).to_string());
    store
        .append_event(lifecycle(
            run,
            3,
            3,
            json!({"data": "x".repeat(2 * 1024 * 1024)}),
        ))
        .await
        .unwrap();
    let error = events(&rt, &token, None, 3).await.unwrap_err();
    assert!(matches!(error, RuntimeError::Internal(_)));
    assert!(error.to_string().contains("store page byte budget"));
}

#[tokio::test]
async fn specific_run_uses_indexed_rows_despite_an_unrelated_oversized_audit() {
    let (rt, token) = fixture();
    let run = Uuid::new_v4();
    let store = rt.events(&token).unwrap();
    store
        .append_event(lifecycle(
            Uuid::new_v4(),
            1,
            1,
            json!({"data": "x".repeat(2 * 1024 * 1024)}),
        ))
        .await
        .unwrap();
    store
        .append_event(lifecycle(run, 2, 2, json!({"kept": true})))
        .await
        .unwrap();
    let rows = events(&rt, &token, Some(&run.to_string()), 200)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"], json!({"kept": true}));
    assert!(matches!(
        events(&rt, &token, None, 200).await,
        Err(RuntimeError::Internal(_))
    ));
}

#[tokio::test]
async fn events_finish_timestamp_group_before_canonical_id_limit() {
    let (rt, token) = fixture();
    let run = Uuid::new_v4();
    let store = rt.events(&token).unwrap();
    let first = Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000001").unwrap();
    let wanted = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();
    store
        .append_event(lifecycle(run, first.as_u128(), 10, Value::Null))
        .await
        .unwrap();
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "UPDATE events SET id = ?1 WHERE id = ?2".into(),
            params: vec![
                SqlValue::Text(first.to_string().to_uppercase()),
                SqlValue::Text(first.to_string()),
            ],
            label: Some("test_physical_exec_event_id".into()),
        })
        .await
        .unwrap();
    for chunk in (1..=4095_u128).collect::<Vec<_>>().chunks(256) {
        let rows = chunk
            .iter()
            .map(|&index| {
                let mut event = lifecycle(run, index, 10, Value::Null);
                event.verb = "unrelated.audit".into();
                event
            })
            .collect();
        assert_eq!(store.append_events(rows).await.unwrap().failed, 0);
    }
    store
        .append_event(lifecycle(run, wanted.as_u128(), 10, Value::Null))
        .await
        .unwrap();
    let rows = events(&rt, &token, None, 1).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], wanted.to_string());
}

#[test]
fn decode_preserves_receipts_without_effective_output_cap() {
    let receipt = json!({
        "id": "historical-receipt",
        "actor": "local",
        "tool": "sh",
        "stdout_produced_bytes": 200,
        "stdout_retained_bytes": 128,
        "stdout_capture": "incomplete",
        "seq": null,
    });
    let row = SqlRow {
        columns: vec![
            SqlColumn {
                name: "receipt".into(),
                value: SqlValue::Text(receipt.to_string()),
            },
            SqlColumn {
                name: "seq".into(),
                value: SqlValue::Null,
            },
        ],
    };
    let decoded = decode(&row).expect("historical receipt decodes");
    assert_eq!(decoded, receipt);
    assert!(decoded.get("effective_max_output_bytes").is_none());
}
