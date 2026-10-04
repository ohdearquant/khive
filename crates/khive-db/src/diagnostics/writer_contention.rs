use super::*;

impl WriterContentionDiagnostics {
    pub(super) fn snapshot(
        pool: &ConnectionPool,
        audit_append_failures: Option<u64>,
        runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
    ) -> Self {
        let writer = pool.writer_acquisition_snapshot();
        let unavailable_reason =
            || Some("no audit-batch control is registered with this runtime instance".to_string());
        Self {
            writer_acquisitions: writer.acquisitions,
            pooled_writer_acquisitions: writer.pooled_acquisitions,
            standalone_writer_acquisitions: writer.standalone_acquisitions,
            writer_task_acquisitions: writer.writer_task_acquisitions,
            writer_acquisition_timeouts: writer.timeouts,
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
