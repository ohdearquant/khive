use super::*;
use khive_runtime::{event_insert_statements, Namespace};
use khive_types::{EventKind, RefResolution, SubstrateKind};
use serde_json::json;

fn event() -> Event {
    let mut event = Event::new(
        "local",
        "conditional-test",
        EventKind::LinkCreated,
        SubstrateKind::Entity,
        "test:actor",
    );
    event.payload = json!({"source_id":Uuid::new_v4(), "target_id":Uuid::new_v4()});
    event.target_id = Some(Uuid::new_v4());
    event.session_id = Some(Uuid::new_v4());
    event.aggregate_kind = Some("proposal".into());
    event.aggregate_id = Some(Uuid::new_v4());
    event.payload_schema_version = 3;
    event.profile_state_version = Some(i64::MAX as u64);
    event.duration_us = 17;
    event.op_index = Some(2);
    event.ref_resolution = Some(RefResolution::Resolved);
    event
}

fn normalized_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unguarded_insert(sql: &str) -> String {
    let normalized = normalized_sql(sql);
    let insert = normalized
        .strip_suffix(" WHERE (changes() = 1)")
        .expect("conditional insert must end with the proposal CAS guard");
    let (target, values) = insert
        .split_once(" SELECT ")
        .expect("conditional insert must select its bound values");
    // Undo only the VALUES-to-SELECT conversion required by the guard.
    // Keep every other token, including insert mode and conflict clauses.
    format!("{target} VALUES ({values})")
}

#[test]
fn conditional_inserts_keep_all_shared_bindings_and_derived_rows() {
    let event = event();
    let canonical = event_insert_statements(&event).unwrap();
    let conditional = build_conditional_event_inserts(&event).unwrap();
    assert_eq!(conditional.len(), 4, "event plus three link observations");
    assert_eq!(canonical.len(), conditional.len());
    for (canonical, conditional) in canonical.iter().zip(&conditional) {
        assert_eq!(
            format!("{:?}", canonical.params),
            format!("{:?}", conditional.params)
        );
        assert_eq!(
            normalized_sql(&canonical.sql),
            unguarded_insert(&conditional.sql),
            "guarded inserts must preserve the complete canonical SQL shape"
        );
    }
    assert_eq!(
        conditional[0].label.as_deref(),
        Some("projection_worker.conditional_event_insert")
    );
    assert!(matches!(
        conditional[0].params[9],
        SqlValue::Integer(i64::MAX)
    ));
    assert!(matches!(conditional[0].params[16], SqlValue::Integer(2)));
    assert!(matches!(
        &conditional[0].params[17],
        SqlValue::Text(text) if text == "resolved"
    ));
}

async fn fixture() -> (KhiveRuntime, ProposalsProjectionWorker) {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.authorize(Namespace::local()).unwrap();
    runtime.sql().writer().await.unwrap().execute_script(
        "CREATE TABLE conditional_event_guard (id INTEGER PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO conditional_event_guard VALUES (1, 0);".into()
    ).await.unwrap();
    let worker = ProposalsProjectionWorker::new(runtime.clone());
    (runtime, worker)
}

fn projection(expected: i64) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE conditional_event_guard SET value = value + 1 WHERE id = 1 AND value = ?1"
            .into(),
        params: vec![SqlValue::Integer(expected)],
        label: Some("test.conditional_event_guard".into()),
    }
}

async fn count(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> i64 {
    let row = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_row(SqlStatement {
            sql: sql.into(),
            params,
            label: Some("test.conditional_event_count".into()),
        })
        .await
        .unwrap()
        .unwrap();
    match row.get("n") {
        Some(SqlValue::Integer(value)) => *value,
        value => panic!("invalid count: {value:?}"),
    }
}

#[tokio::test]
async fn conditional_chain_hit_persists_observations_and_counts_one_event() {
    let (runtime, worker) = fixture().await;
    let event = event();
    let usage = khive_runtime::usage::UsageContext::new();
    let hit = khive_runtime::usage::scope(usage.clone(), async {
        worker
            .execute_guarded_event(projection(0), event.clone())
            .await
    })
    .await
    .unwrap();
    assert!(hit);
    assert_eq!(usage.snapshot()["event_rows"], json!(1));
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) AS n FROM event_observations WHERE event_id = ?1",
            vec![SqlValue::Text(event.id.to_string())]
        )
        .await,
        3
    );
    assert_eq!(
        count(
            &runtime,
            "SELECT value AS n FROM conditional_event_guard WHERE id = 1",
            vec![]
        )
        .await,
        1
    );
}

#[tokio::test]
async fn conditional_chain_miss_suppresses_event_and_every_observation() {
    let (runtime, worker) = fixture().await;
    let event = event();
    let usage = khive_runtime::usage::UsageContext::new();
    assert!(!khive_runtime::usage::scope(usage.clone(), async {
        worker
            .execute_guarded_event(projection(99), event.clone())
            .await
    })
    .await
    .unwrap());
    assert!(usage.shipping_snapshot().is_some(), "miss is measured");
    assert!(
        usage.snapshot().get("event_rows").is_none(),
        "zero counters are omitted"
    );
    for sql in [
        "SELECT COUNT(*) AS n FROM events WHERE id = ?1",
        "SELECT COUNT(*) AS n FROM event_observations WHERE event_id = ?1",
    ] {
        assert_eq!(
            count(&runtime, sql, vec![SqlValue::Text(event.id.to_string())]).await,
            0
        );
    }
    assert_eq!(
        count(
            &runtime,
            "SELECT value AS n FROM conditional_event_guard WHERE id = 1",
            vec![]
        )
        .await,
        0
    );
}

#[tokio::test]
async fn conditional_observation_error_rolls_back_projection_and_whole_event_chain() {
    let (runtime, worker) = fixture().await;
    runtime.sql().writer().await.unwrap().execute_script(
        "CREATE TRIGGER reject_second_observation BEFORE INSERT ON event_observations WHEN NEW.position = 1 BEGIN SELECT RAISE(ABORT, 'injected observation failure'); END;".into()
    ).await.unwrap();
    let event = event();
    assert!(worker
        .execute_guarded_event(projection(0), event.clone())
        .await
        .is_err());
    assert_eq!(
        count(
            &runtime,
            "SELECT value AS n FROM conditional_event_guard WHERE id = 1",
            vec![]
        )
        .await,
        0
    );
    for sql in [
        "SELECT COUNT(*) AS n FROM events WHERE id = ?1",
        "SELECT COUNT(*) AS n FROM event_observations WHERE event_id = ?1",
    ] {
        assert_eq!(
            count(&runtime, sql, vec![SqlValue::Text(event.id.to_string())]).await,
            0
        );
    }
}

#[tokio::test]
async fn conditional_builder_refuses_invalid_metadata_before_mutating_projection() {
    let (runtime, worker) = fixture().await;
    let mut invalid = Vec::new();
    let mut overflow = event();
    overflow.profile_state_version = Some(u64::MAX);
    invalid.push(overflow);
    let mut unpaired = event();
    unpaired.ref_resolution = None;
    invalid.push(unpaired);
    let mut malformed = event();
    malformed.payload["source_id"] = json!("invalid UUID");
    invalid.push(malformed);
    for event in invalid {
        assert!(matches!(
            worker.execute_guarded_event(projection(0), event).await,
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    assert_eq!(
        count(
            &runtime,
            "SELECT value AS n FROM conditional_event_guard WHERE id = 1",
            vec![]
        )
        .await,
        0
    );
}
