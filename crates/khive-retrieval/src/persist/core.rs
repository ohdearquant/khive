//! Core persistence types: error enum, `RetrievalPersistence`, and statistics.

use std::sync::Arc;

use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess, StorageError};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

/// Errors that can occur during retrieval persistence operations.
#[derive(Error, Debug)]
pub enum PersistError {
    /// SQLite operation failed.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Routed storage operation failed, including read-only admission refusal.
    #[error("Storage error: {0}")]
    Storage(#[from] khive_storage::StorageError),

    /// Serialization failed.
    #[error("Serialization error: {0}")]
    Serialize(String),

    /// Deserialization failed.
    #[error("Deserialization error: {0}")]
    Deserialize(String),

    /// Spawn blocking task failed.
    #[error("Task join error: {0}")]
    TaskJoin(String),

    /// Snapshot verification failed.
    #[error("Snapshot verification failed: {0}")]
    SnapshotVerification(String),

    /// Validation error (e.g. empty namespace, out-of-range parameter).
    #[error("Validation error: {0}")]
    Validation(String),

    /// Task join error from spawn_blocking.
    #[error("Blocking task failed: {0}")]
    BlockingJoin(String),

    /// JoinError from tokio spawn_blocking (auto-converted).
    #[error("Tokio join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Internal error (generic, for ported engine code).
    #[error("Internal error: {0}")]
    Internal(String),

    /// Embedding error (for ported engine code).
    #[error("Embedding error: {0}")]
    Embedding(String),

    /// Retrieval error (for ported engine code).
    #[error("Retrieval error: {0}")]
    Retrieval(String),
}

/// Retrieval index persistence through a shared storage capability.
///
/// Provides methods to persist and restore HNSW and BM25 index snapshots.
/// Managed backends create the snapshot schema through core migrations.
pub struct RetrievalPersistence {
    /// Shared SQL capability; the backend owns reader and writer admission.
    pub(crate) sql: Arc<dyn SqlAccess>,
    /// Namespace for multi-tenancy.
    pub(crate) namespace: Arc<str>,
}

impl RetrievalPersistence {
    /// Create a persistence layer using the backend's shared SQL capability.
    pub fn new(sql: Arc<dyn SqlAccess>, namespace: impl Into<String>) -> Self {
        Self {
            sql,
            namespace: Arc::from(namespace.into()),
        }
    }

    /// Initialize the persistence schema through an admitted writer.
    ///
    /// This compatibility bootstrap creates the snapshot table and index if
    /// absent. Normal managed backends already create them in core migrations.
    pub async fn init_schema(&self) -> Result<(), PersistError> {
        let mut writer = self.sql.writer().await?;
        writer
            .execute_script(include_str!("../../sql/retrieval_snapshots_create.sql").to_owned())
            .await?;
        Ok(())
    }

    /// Generic snapshot persistence.
    pub(crate) async fn persist_snapshot<T: Serialize + Send + Sync>(
        &self,
        index_type: &str,
        snapshot: &T,
    ) -> Result<(), PersistError> {
        let data =
            serde_json::to_vec(snapshot).map_err(|e| PersistError::Serialize(e.to_string()))?;
        let statement = SqlStatement::new(
            include_str!("../../sql/retrieval_snapshots_upsert.sql"),
            vec![
                SqlValue::Text(self.namespace.to_string()),
                SqlValue::Text(index_type.to_owned()),
                SqlValue::Blob(data),
                SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
            ],
        );
        self.sql.writer().await?.execute(statement).await?;
        Ok(())
    }

    /// Generic snapshot loading.
    pub(crate) async fn load_snapshot<T: DeserializeOwned + Send + 'static>(
        &self,
        index_type: &str,
    ) -> Result<Option<T>, PersistError> {
        let row = {
            let mut reader = self.sql.reader().await?;
            reader
                .query_row(SqlStatement::new(
                    include_str!("../../sql/retrieval_snapshots_select_snapshot.sql"),
                    vec![
                        SqlValue::Text(self.namespace.to_string()),
                        SqlValue::Text(index_type.to_owned()),
                    ],
                ))
                .await?
        };
        let Some(row) = row else {
            return Ok(None);
        };
        let data = match row
            .columns
            .into_iter()
            .find(|column| column.name == "snapshot")
            .map(|column| column.value)
        {
            Some(SqlValue::Blob(data)) => data,
            _ => {
                return Err(StorageError::Internal(
                    "retrieval snapshot column must contain a blob".into(),
                )
                .into())
            }
        };
        // Keep CPU-heavy decoding off the async worker, after releasing the reader.
        tokio::task::spawn_blocking(move || {
            serde_json::from_slice(&data)
                .map(Some)
                .map_err(|e| PersistError::Deserialize(e.to_string()))
        })
        .await
        .map_err(|e| PersistError::TaskJoin(e.to_string()))?
    }

    /// Delete all snapshots for this namespace.
    pub async fn clear(&self) -> Result<(), PersistError> {
        let statement = SqlStatement::new(
            include_str!("../../sql/retrieval_snapshots_delete_namespace.sql"),
            vec![SqlValue::Text(self.namespace.to_string())],
        );
        self.sql.writer().await?.execute(statement).await?;
        Ok(())
    }

    /// Get persistence statistics.
    pub async fn stats(&self) -> Result<PersistenceStats, PersistError> {
        let rows = {
            let mut reader = self.sql.reader().await?;
            reader
                .query_all(SqlStatement::new(
                    include_str!("../../sql/retrieval_snapshots_list_namespace.sql"),
                    vec![SqlValue::Text(self.namespace.to_string())],
                ))
                .await?
        };
        let mut stats = PersistenceStats::default();
        for row in rows {
            let index_type = row
                .text("index_type")
                .map_err(|e| StorageError::Internal(e.to_string()))?;
            let size = row
                .i64("snapshot_size")
                .map_err(|e| StorageError::Internal(e.to_string()))?;
            let created_at = row
                .i64("created_at")
                .map_err(|e| StorageError::Internal(e.to_string()))?;
            match index_type {
                "hnsw" => {
                    stats.hnsw_snapshot_size = size as usize;
                    stats.hnsw_snapshot_at = Some(created_at);
                }
                "bm25" => {
                    stats.bm25_snapshot_size = size as usize;
                    stats.bm25_snapshot_at = Some(created_at);
                }
                _ => {}
            }
        }
        Ok(stats)
    }
}

/// Statistics about persisted snapshots.
#[derive(Debug, Default, Clone)]
pub struct PersistenceStats {
    /// Size of HNSW snapshot in bytes.
    pub hnsw_snapshot_size: usize,
    /// Timestamp when HNSW snapshot was created (Unix microseconds).
    pub hnsw_snapshot_at: Option<i64>,
    /// Size of BM25 snapshot in bytes.
    pub bm25_snapshot_size: usize,
    /// Timestamp when BM25 snapshot was created (Unix microseconds).
    pub bm25_snapshot_at: Option<i64>,
}
