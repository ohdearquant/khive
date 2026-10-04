use super::*;

impl SqliteWriter {
    pub(super) fn map_direct_error(
        &self,
        error: rusqlite::Error,
        operation: &'static str,
    ) -> StorageError {
        crate::timeout_sink::maybe_emit_busy(
            &self.db,
            crate::timeout_sink::Site::StandaloneSqlBridge,
            &error,
        );
        let error = map_rusqlite_err(error, operation);
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
        if self.observe_direct_errors {
            self.pool.record_direct_writer_error(&error);
        }
        error
    }
}
