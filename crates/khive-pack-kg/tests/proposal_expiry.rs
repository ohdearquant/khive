//! Proposal expiry admission preserves the event and projection timestamp value.

use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
    WalCeilingSource,
};
use khive_storage::{EventFilter, PageRequest, SqlStatement, SqlValue};
use khive_types::EventKind;
use serde_json::{json, Value};
use uuid::Uuid;

fn surface() -> (KhiveRuntime, VerbRegistry) {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("isolated in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

fn proposal(expiry: Option<Value>) -> Value {
    let mut params = json!({
        "title": "Expiry admission",
        "description": "Preserve the supplied non-negative timestamp",
        "changeset": {
            "kind": "add_entity",
            "entity": {"kind": "concept", "name": "Proposed only"}
        }
    });
    if let Some(expiry) = expiry {
        params["expiry"] = expiry;
    }
    params
}

async fn history(runtime: &KhiveRuntime) -> Value {
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let events = runtime.events(&token).expect("event store");
    let filter = EventFilter {
        kinds: vec![EventKind::ProposalCreated],
        ..Default::default()
    };
    let count = events
        .count_events(filter.clone())
        .await
        .expect("event count");
    let mut page = events
        .query_events(
            filter,
            PageRequest {
                offset: 0,
                limit: 20,
            },
        )
        .await
        .expect("proposal event page");
    assert_eq!(
        page.items.len() as u64,
        count,
        "snapshot includes every proposal event"
    );
    page.items.sort_by_key(|event| event.id);
    let mut reader = runtime.sql().reader().await.expect("projection reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM proposals_open ORDER BY proposal_id".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("complete proposal projection");
    json!({"events": page.items, "projection": rows})
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn propose_expiry_preserves_nonnegative_values_and_refuses_negative_without_history() {
    let (runtime, registry) = surface();
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let events = runtime.events(&token).expect("event store");
    for (index, supplied) in [
        None,
        Some(Value::Null),
        Some(json!(0)),
        Some(json!(1)),
        Some(json!(1_000_000)),
        Some(json!(i64::MAX)),
    ]
    .into_iter()
    .enumerate()
    {
        let expected = supplied.clone().unwrap_or(Value::Null);
        let response = registry
            .dispatch("propose", proposal(supplied))
            .await
            .expect("omitted, null, zero, and non-negative expiry remain accepted");
        assert_eq!(response["status"], "open");
        let id = Uuid::parse_str(response["id"].as_str().expect("proposal id")).expect("full UUID");
        let filter = EventFilter {
            kinds: vec![EventKind::ProposalCreated],
            payload_proposal_id: Some(id),
            ..Default::default()
        };
        assert_eq!(
            events
                .count_events(filter.clone())
                .await
                .expect("matching event count"),
            1
        );
        let page = events
            .query_events(
                filter,
                PageRequest {
                    offset: 0,
                    limit: 2,
                },
            )
            .await
            .expect("raw event");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].aggregate_id, Some(id));
        assert_eq!(page.items[0].payload["expiry"], expected);
        {
            let mut reader = runtime.sql().reader().await.expect("projection reader");
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT expiry FROM proposals_open WHERE proposal_id = ?1".into(),
                    params: vec![SqlValue::Text(id.to_string())],
                    label: None,
                })
                .await
                .expect("projection query")
                .expect("projected proposal");
            assert_eq!(
                row.opt_i64("expiry").expect("nullable integer expiry"),
                expected.as_i64()
            );
        }
        if index < 5 {
            let listed = registry
                .dispatch("list", json!({"kind": "proposal", "limit": 20}))
                .await
                .expect("public projection list");
            let row = listed["items"]
                .as_array()
                .expect("projection rows")
                .iter()
                .find(|row| row["id"] == id.to_string())
                .expect("created proposal listed");
            let expected_wire = [
                Value::Null,
                Value::Null,
                json!("1970-01-01T00:00:00.000000Z"),
                json!("1970-01-01T00:00:00.000001Z"),
                json!("1970-01-01T00:00:01.000000Z"),
            ];
            assert_eq!(row["expiry"], expected_wire[index]);
        }
    }
    let before = history(&runtime).await;
    assert_eq!(before["events"].as_array().expect("events").len(), 6);
    assert_eq!(
        before["projection"].as_array().expect("projection").len(),
        6
    );
    for expiry in [-1, i64::MIN] {
        let error = registry
            .dispatch("propose", proposal(Some(json!(expiry))))
            .await
            .expect_err("negative expiry must refuse");
        let RuntimeError::InvalidInput(message) = error else {
            panic!("expected input refusal for {expiry}: {error:?}");
        };
        assert!(message.contains("expiry must be a non-negative integer"));
        assert_eq!(
            history(&runtime).await,
            before,
            "refusal must not append an event or mutate the projection"
        );
    }
    for invalid in [json!(true), json!("1"), json!(1.5), json!(u64::MAX)] {
        assert!(matches!(
            registry.dispatch("propose", proposal(Some(invalid))).await,
            Err(RuntimeError::InvalidInput(_))
        ));
        assert_eq!(
            history(&runtime).await,
            before,
            "existing type/range refusals remain mutation-free"
        );
    }
}
