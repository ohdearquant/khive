use super::{Connection, SqliteError, WriteAdmission};
use crate::disk_guard::VolumeLease;
use rusqlite::hooks::{AuthContext, Authorization};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

pub(super) struct RawMigrationWriteUnit<'conn> {
    conn: &'conn mut Connection,
    replacement: Option<Connection>,
    _volume_lease: Option<VolumeLease>,
    database_path: String,
    volume_key: String,
}

impl<'conn> RawMigrationWriteUnit<'conn> {
    pub(super) fn new(
        conn: &'conn mut Connection,
        admission: &WriteAdmission,
    ) -> Result<Self, SqliteError> {
        if !conn.is_autocommit() {
            return Err(SqliteError::InheritedWriterTransaction);
        }
        let database_path = super::canonical_connection_database_path(conn)?
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| ":memory:".to_string());
        let volume_key = admission
            .volume_identity()?
            .map(|volume| volume.diagnostic_key())
            .unwrap_or_else(|| "in-memory".to_string());
        let replacement = Connection::open_in_memory()?;
        replacement.authorizer(Some(|_: AuthContext<'_>| Authorization::Deny))?;
        let volume_lease = admission.acquire()?;
        Ok(Self {
            conn,
            replacement: Some(replacement),
            _volume_lease: volume_lease,
            database_path,
            volume_key,
        })
    }

    pub(super) fn run<T>(
        mut self,
        operation: impl FnOnce(&mut Connection) -> Result<T, SqliteError>,
    ) -> Result<T, SqliteError> {
        let operation_result = catch_unwind(AssertUnwindSafe(|| operation(&mut *self.conn)));
        let was_unsettled = !self.conn.is_autocommit();
        // An unknown settlement outranks a panic from the operation: the
        // caller must learn that later writes on this database are refused.
        self.settle()?;
        match operation_result {
            Ok(result) if was_unsettled && result.is_ok() => {
                Err(SqliteError::WriterSettlementUnknown)
            }
            Ok(result) => result,
            Err(payload) => resume_unwind(payload),
        }
    }

    fn settle(&mut self) -> Result<(), SqliteError> {
        if self.conn.is_autocommit() {
            return Ok(());
        }
        if self.conn.execute_batch("ROLLBACK").is_ok() && self.conn.is_autocommit() {
            return Ok(());
        }
        let Some(replacement) = self.replacement.take() else {
            return Err(SqliteError::WriterSettlementUnknown);
        };
        let original = std::mem::replace(self.conn, replacement);
        crate::connection_settlement::close_retired_connection(
            original,
            &self.database_path,
            &self.volume_key,
        )
    }
}

/// Raw-connection migration writes: each admitted call is its own
/// [`RawMigrationWriteUnit`], so the lease and the settlement are per
/// transaction, never per run.
pub(super) struct RawMigrationTransactions<'conn, 'admission> {
    conn: &'conn mut Connection,
    admission: &'admission WriteAdmission,
}

impl<'conn, 'admission> RawMigrationTransactions<'conn, 'admission> {
    pub(super) fn new(conn: &'conn mut Connection, admission: &'admission WriteAdmission) -> Self {
        Self { conn, admission }
    }
}

impl super::MigrationTransactions for RawMigrationTransactions<'_, '_> {
    fn admitted<T>(
        &mut self,
        operation: impl FnOnce(&mut Connection) -> Result<T, SqliteError>,
    ) -> Result<T, SqliteError> {
        RawMigrationWriteUnit::new(self.conn, self.admission)?.run(operation)
    }
}

impl Drop for RawMigrationWriteUnit<'_> {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}
