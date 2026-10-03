use super::*;
use khive_storage::types::SqlColumn;
use khive_types::{Namespace, SubstrateKind};

#[tokio::test]
async fn projected_event_row_preserves_absent_optional_uuid() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("normal namespace token");
    let target = Uuid::new_v4();
    let event = Event::new(
        "local",
        "get",
        EventKind::Audit,
        SubstrateKind::Event,
        "actor:a",
    )
    .with_target(target);
    runtime
        .events(&token)
        .expect("events")
        .append_event(event.clone())
        .await
        .expect("append fixture event");
    let mut reader = runtime.sql().reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id, target_id, session_id FROM events WHERE id = ?1".into(),
            params: vec![SqlValue::Text(event.id.to_string())],
            label: Some("get_column_fixture".into()),
        })
        .await
        .expect("project actual events row")
        .expect("event row exists");
    assert_eq!(
        parse_uuid_column(&row, "id").expect("required id"),
        event.id
    );
    assert_eq!(
        sql_optional_uuid(&row, "target_id").expect("positive optional UUID"),
        Some(target)
    );
    assert!(matches!(row.get("session_id"), Some(SqlValue::Null)));
    assert_eq!(
        sql_optional_uuid(&row, "session_id").expect("NULL optional UUID"),
        None
    );
    assert!(row.get("aggregate_id").is_none());
    assert_eq!(
        sql_optional_uuid(&row, "aggregate_id").expect("absent optional UUID stays absent"),
        None
    );
}

#[test]
fn event_uuid_column_diagnostics_keep_value_and_absence_wording() {
    let row = SqlRow {
        columns: vec![SqlColumn {
            name: "id".into(),
            value: SqlValue::Float(1.5),
        }],
    };
    for error in [
        parse_uuid_column(&row, "id").unwrap_err(),
        sql_optional_uuid(&row, "id").unwrap_err(),
    ] {
        match error {
            RuntimeError::Internal(message) => {
                assert_eq!(message, "events.id has unexpected SQL value Float(1.5)")
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
    match parse_uuid_column(&row, "missing").unwrap_err() {
        RuntimeError::Internal(message) => assert_eq!(message, "events row missing missing"),
        other => panic!("unexpected error: {other:?}"),
    }
}
