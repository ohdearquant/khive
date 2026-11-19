use super::*;

impl SqliteWriter {
    pub(super) fn map_direct_error(
        &self,
        error: rusqlite::Error,
        operation: &'static str,
        stage: khive_storage::error::SqliteWriteStage,
    ) -> StorageError {
        crate::timeout_sink::maybe_emit_busy(
            &self.db,
            crate::timeout_sink::Site::StandaloneSqlBridge,
            &error,
        );
        let error = crate::error::with_write_stage(map_rusqlite_err(error, operation), stage);
        if self.observe_direct_errors {
            self.pool.record_direct_writer_error(&error);
        }
        error
    }

    pub(super) fn map_direct_batch_failure(&self, failure: BatchFailure) -> StorageError {
        crate::timeout_sink::maybe_emit_busy(
            &self.db,
            crate::timeout_sink::Site::StandaloneSqlBridge,
            &failure.error,
        );
        let native =
            crate::error::native_write_failure(&failure.error, failure.stage).map(|mut native| {
                native.settlement_unknown = matches!(
                    &failure.poison_reason,
                    Some(standalone_batch::BatchPoisonReason::RollbackFailed(_))
                );
                native
            });
        let error = match failure.poison_reason {
            Some(poison_reason) => StorageError::driver(
                StorageCapability::Sql,
                "execute_batch",
                PoisonedBatchError {
                    original: failure.error,
                    poison_reason,
                },
            ),
            None => map_rusqlite_err(failure.error, "execute_batch"),
        };
        let error = match native {
            Some(failure) => error.with_sqlite_write_failure(failure),
            None => error,
        };
        if self.observe_direct_errors {
            self.pool.record_direct_writer_error(&error);
        }
        error
    }
}
