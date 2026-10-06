use super::*;

impl StorageBackend {
    /// Validate the sealed memory-receipt cutover without a writer or migration.
    pub fn validate_memory_visibility_cutover(&self) -> Result<(), SqliteError> {
        let reader = self.pool.reader()?;
        crate::migrations::validate_memory_visibility_cutover(reader.conn())
    }

    /// Read the applied schema version through the pool's ordinary reader or
    /// writer, without running migrations. Unlike
    /// [`migrations::inspect_schema_version`](crate::migrations::inspect_schema_version),
    /// this goes through the already-open pool rather than a fresh boot-time
    /// snapshot connection, so it tolerates a WAL sidecar left by this same
    /// backend's own recent writes.
    pub fn schema_version(&self) -> Result<u32, SqliteError> {
        if self.is_read_only() {
            let reader = self.pool.reader()?;
            crate::migrations::read_schema_version(reader.conn())
        } else {
            let writer = self.pool.try_writer()?;
            crate::migrations::read_schema_version(writer.conn())
        }
    }
}
