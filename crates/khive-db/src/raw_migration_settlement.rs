use super::{Connection, SqliteError, WriteAdmission};
use crate::disk_guard::VolumeLease;
use rusqlite::hooks::{AuthContext, Authorization};

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
        let result = operation(&mut *self.conn);
        let was_unsettled = !self.conn.is_autocommit();
        let retired = self.settle();
        if retired || (was_unsettled && result.is_ok()) {
            Err(SqliteError::WriterSettlementUnknown)
        } else {
            result
        }
    }

    fn settle(&mut self) -> bool {
        if self.conn.is_autocommit() {
            return false;
        }
        if self.conn.execute_batch("ROLLBACK").is_ok() && self.conn.is_autocommit() {
            return false;
        }
        // Prepared before admission: cleanup must not allocate a replacement
        // after it has lost the ability to settle the original connection.
        let replacement = self
            .replacement
            .take()
            .expect("raw migration retirement owner");
        let original = std::mem::replace(self.conn, replacement);
        crate::connection_settlement::close_retired_connection(
            original,
            &self.database_path,
            &self.volume_key,
        );
        true
    }
}

impl Drop for RawMigrationWriteUnit<'_> {
    fn drop(&mut self) {
        self.settle();
    }
}
