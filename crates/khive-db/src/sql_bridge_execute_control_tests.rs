// Single-statement execution must refuse caller transaction control before
// either route acquires or submits work. The same handle must remain usable.
async fn assert_execute_control_refusals(mut config: PoolConfig) {
    let queue_enabled = config
        .write_queue_enabled
        .expect("each fixture must choose its writer route explicitly");
    let dir = tempfile::tempdir().unwrap();
    config.path = Some(dir.path().join("execute_control.db"));
    config.checkout_timeout = std::time::Duration::from_millis(250);
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    assert_eq!(
        pool.writer_task_handle().unwrap().is_some(),
        queue_enabled,
        "the fixture must exercise its requested writer route"
    );
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE execute_control_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();

    // Prove the route is healthy before testing refusal. Control words inside
    // a SQL literal are data, and parameter binding must remain supported.
    let affected = writer
        .execute(SqlStatement {
            sql: "INSERT INTO execute_control_test (id, val) VALUES (?1, 'COMMIT; BEGIN; END; ROLLBACK; SAVEPOINT; RELEASE')".into(),
            params: vec![SqlValue::Integer(0)],
            label: None,
        })
        .await
        .expect("a legitimate single statement must succeed before refusal");
    assert_eq!(affected, 1);
    let literal = writer
        .query_scalar(SqlStatement {
            sql: "SELECT val FROM execute_control_test WHERE id = ?1".into(),
            params: vec![SqlValue::Integer(0)],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(&literal, Some(SqlValue::Text(value)) if value == "COMMIT; BEGIN; END; ROLLBACK; SAVEPOINT; RELEASE"),
        "control words in a literal must be preserved; got {literal:?}"
    );

    let mut expected_count = 1_i64;
    // COMMIT goes first: a queue-only guard bypass must fail the typed refusal
    // assertion, before any later probe can encounter a damaged writer task.
    for (sql, head) in [
        ("COMMIT", "COMMIT"),
        ("BEGIN IMMEDIATE", "BEGIN"),
        ("START TRANSACTION", "START"),
        ("eNd transaction", "END"),
        ("ROLLBACK", "ROLLBACK"),
        ("SAVEPOINT sp1", "SAVEPOINT"),
        ("RELEASE sp1", "RELEASE"),
        ("  -- leading comment\ncommit", "COMMIT"),
        ("/* block */ rollback to savepoint sp1", "ROLLBACK"),
        ("/* leading */ \u{feff} ; -- empty\n ; COMMIT", "COMMIT"),
    ] {
        let rejected = writer
            .execute(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput {
                    capability: StorageCapability::Sql,
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "execute"
                    && message.contains("transaction control")
                    && message.contains(head)
            ),
            "single execute must reject transaction control before routing: {sql:?}; got {rejected:?}"
        );
        let count = writer
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM execute_control_test".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("refusal must leave the same handle usable for a query");
        assert!(
            matches!(count, Some(SqlValue::Integer(value)) if value == expected_count),
            "a refused statement must leave the row count unchanged; got {count:?}"
        );

        let value = format!("after-{head}-{expected_count}");
        let affected = writer
            .execute(SqlStatement {
                sql: "INSERT INTO execute_control_test (id, val) VALUES (?1, ?2)".into(),
                params: vec![
                    SqlValue::Integer(expected_count),
                    SqlValue::Text(value.clone()),
                ],
                label: None,
            })
            .await
            .expect("refusal must leave the same handle usable for a parameterized write");
        assert_eq!(affected, 1);
        let persisted = writer
            .query_scalar(SqlStatement {
                sql: "SELECT val FROM execute_control_test WHERE id = ?1".into(),
                params: vec![SqlValue::Integer(expected_count)],
                label: None,
            })
            .await
            .expect("the same handle must read its successful parameterized write");
        assert!(
            matches!(&persisted, Some(SqlValue::Text(actual)) if actual == &value),
            "the successful write must preserve its bound value; got {persisted:?}"
        );
        expected_count += 1;
    }
    let count = writer
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM execute_control_test".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(count, Some(SqlValue::Integer(value)) if value == expected_count));
}

#[tokio::test]
async fn execute_rejects_transaction_control_on_standalone_handle() {
    assert_execute_control_refusals(PoolConfig {
        write_queue_enabled: Some(false),
        write_routing_strict: false,
        ..PoolConfig::for_test()
    })
    .await;
}

#[tokio::test]
async fn execute_rejects_transaction_control_before_queue_submission() {
    assert_execute_control_refusals(PoolConfig {
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    })
    .await;
}
