#[test]
fn unique_constraint_failure_at_execute_sql_capability_is_classified() {
    let e = driver_err_sql(
        "execute",
        "UNIQUE constraint failed: brain_serve_ledger.namespace, \
             brain_serve_ledger.target_id, brain_serve_ledger.query_class, \
             brain_serve_ledger.served_at",
    );
    assert!(e.is_unique_constraint_violation());
}

#[test]
fn unique_constraint_failure_at_pool_writer_execute_is_classified() {
    let e = driver_err_sql("pool_writer.execute", "UNIQUE constraint failed: t.id");
    assert!(e.is_unique_constraint_violation());
}

#[test]
fn unique_constraint_message_at_non_execute_operation_is_not_classified() {
    let e = driver_err_sql("query_row", "UNIQUE constraint failed: t.id");
    assert!(!e.is_unique_constraint_violation());
}

#[test]
fn non_unique_driver_error_at_execute_is_not_classified() {
    let e = driver_err_sql("execute", "disk I/O error");
    assert!(!e.is_unique_constraint_violation());
}

#[test]
fn non_sql_capability_is_not_classified_as_unique_violation() {
    let e = driver_err("execute", "UNIQUE constraint failed: t.id");
    assert!(!e.is_unique_constraint_violation());
}

#[test]
fn timeout_is_not_classified_as_unique_violation() {
    let e = StorageError::Timeout {
        operation: "execute".into(),
    };
    assert!(!e.is_unique_constraint_violation());
}
