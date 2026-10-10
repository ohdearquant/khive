use super::super::StorageBackend;
use crate::sql_bridge::SqlBridge;
use async_trait::async_trait;
use khive_storage::{AtomicUnitOp, SqlAccess, SqlReader, SqlWriter, StorageResult};
use std::any::Any;
use std::sync::Arc;

#[async_trait]
impl SqlAccess for StorageBackend {
    fn database_path(&self) -> Option<std::path::PathBuf> {
        self.sql().database_path()
    }

    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        self.sql().reader().await
    }

    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
        self.sql().writer().await
    }

    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
        self.sql().atomic_unit(op).await
    }
}

impl StorageBackend {
    /// Get the SQL access capability.
    ///
    /// Returns an `Arc<dyn SqlAccess>` suitable for passing to services.
    pub fn sql(&self) -> Arc<dyn khive_storage::SqlAccess> {
        Arc::new(SqlBridge::new(Arc::clone(&self.pool), self.is_file_backed))
    }
}
