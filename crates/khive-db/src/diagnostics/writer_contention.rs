use super::*;

impl WriterContentionDiagnostics {
    pub(super) fn snapshot(
        pool: &ConnectionPool,
        audit_append_failures: Option<u64>,
        runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
    ) -> Self {
        let writer = pool.writer_acquisition_snapshot();
        let configured_guard_deadline_ms = pool
            .effective_disk_guard_config()
            .map(|policy| policy.guard_deadline_ms);
        let configured_checkout_timeout_ms =
            u64::try_from(pool.config().checkout_timeout.as_millis()).unwrap_or(u64::MAX);
        let unavailable_reason =
            || Some("no audit-batch control is registered with this runtime instance".to_string());
        Self {
            writer_acquisitions: writer.acquisitions,
            pooled_writer_acquisitions: writer.pooled_acquisitions,
            standalone_writer_acquisitions: writer.standalone_acquisitions,
            writer_task_acquisitions: writer.writer_task_acquisitions,
            writer_acquisition_timeouts: writer.timeouts,
            writer_lease_timeouts: writer.lease_timeouts,
            configured_guard_deadline_ms,
            configured_checkout_timeout_ms,
            effective_writer_wait_bound_ms: configured_guard_deadline_ms
                .unwrap_or(0)
                .saturating_add(configured_checkout_timeout_ms),
            direct_writer_busy_refusals: writer.direct_busy_refusals,
            writer_task_begin_busy: writer.writer_task_begin_busy,
            writer_task_begin_busy_absorbed: writer.writer_task_begin_busy_absorbed,
            writer_task_begin_errors: writer.writer_task_begin_errors,
            writer_task_request_failures: writer.writer_task_request_failures,
            writer_task_side_effects_unknown: writer.writer_task_side_effects_unknown,
            audit_append_failures,
            audit_append_failures_unavailable_reason: audit_append_failures.is_none().then(|| {
                "runtime audit instrumentation was not supplied to khive-db diagnostics".to_string()
            }),
            audit_obligation_append_failures: None,
            audit_obligation_append_failures_unavailable_reason: Some(
                "runtime obligation audit instrumentation was not supplied to khive-db diagnostics"
                    .to_string(),
            ),
            audit_batch_flush_failures: runtime_audit_batch_metrics.map(|m| m.flush_failures),
            audit_batch_flush_failures_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_degraded_rows: runtime_audit_batch_metrics.map(|m| m.degraded_rows),
            audit_degraded_rows_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_degraded: runtime_audit_batch_metrics.map(|m| m.degraded),
            audit_degraded_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_admission_refused_obligations: runtime_audit_batch_metrics
                .map(|m| m.admission_refused_obligations),
            audit_admission_refused_obligations_last_at_ms: runtime_audit_batch_metrics
                .and_then(|m| m.admission_refused_obligations_last_at_ms),
            audit_admission_refused_obligations_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_admission_unresolved_obligations: runtime_audit_batch_metrics
                .map(|m| m.admission_unresolved_obligations),
            audit_admission_unresolved_obligations_last_at_ms: runtime_audit_batch_metrics
                .and_then(|m| m.admission_unresolved_obligations_last_at_ms),
            audit_admission_unresolved_obligations_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
        }
    }
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_busy_diagnostics_serializes_each_pool_counter() {
    use khive_storage::{SqlAccess, SqlStatement};
    let fixture = crate::writer_busy_fixture::Fixture::new(true, false);
    let other = crate::writer_busy_fixture::Fixture::new(true, false);
    let bridge = crate::SqlBridge::new(std::sync::Arc::clone(&fixture.pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let holder = fixture.lock(false);
    let error = writer
        .execute(SqlStatement {
            sql: crate::writer_busy_fixture::INSERT.to_string(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        crate::read_cancellation::storage_error_sqlite_code(&error),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    let wire = serde_json::to_value(WriterContentionDiagnostics::snapshot(
        &fixture.pool,
        None,
        None,
    ))
    .unwrap();
    assert_eq!(wire["direct_writer_busy_refusals"], 1);
    assert_eq!(wire["writer_task_begin_busy"], 0);
    let other_wire = serde_json::to_value(WriterContentionDiagnostics::snapshot(
        &other.pool,
        None,
        None,
    ))
    .unwrap();
    assert_eq!(other_wire["direct_writer_busy_refusals"], 0);
    assert!(!holder.is_autocommit());
    holder.execute_batch("ROLLBACK").unwrap();
}

#[cfg(test)]
#[test]
fn writer_contention_reports_lease_timeouts_and_the_wait_bound() {
    use crate::pool::PoolConfig;
    use std::sync::Arc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("writer-wait-bound.db")),
            checkout_timeout: Duration::from_millis(50),
            // Same-volume pools in this process wait behind the held lease.
            disk_guard_config: Some(crate::EffectiveDiskGuardConfig {
                guard_deadline_ms: 100,
                ..Default::default()
            }),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let held = pool.writer().unwrap();
    let waiter = Arc::clone(&pool);
    let refused = std::thread::spawn(move || waiter.writer().is_err())
        .join()
        .unwrap();
    drop(held);
    assert!(refused, "a contended writer must be refused at the lease");
    let wire =
        serde_json::to_value(WriterContentionDiagnostics::snapshot(&pool, None, None)).unwrap();
    assert_eq!(wire["writer_lease_timeouts"], 1);
    assert_eq!(wire["writer_acquisition_timeouts"], 0);
    assert_eq!(wire["configured_guard_deadline_ms"], 100);
    assert_eq!(wire["configured_checkout_timeout_ms"], 50);
    assert_eq!(wire["effective_writer_wait_bound_ms"], 150);

    let memory = ConnectionPool::new(PoolConfig {
        path: None,
        checkout_timeout: Duration::from_millis(50),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let memory_wire =
        serde_json::to_value(WriterContentionDiagnostics::snapshot(&memory, None, None)).unwrap();
    assert!(
        memory_wire["configured_guard_deadline_ms"].is_null(),
        "an in-memory pool takes no lease: {memory_wire}"
    );
    assert_eq!(memory_wire["effective_writer_wait_bound_ms"], 50);
}
