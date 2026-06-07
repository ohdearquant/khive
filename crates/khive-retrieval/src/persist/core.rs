//! Core persistence types: error enum, `RetrievalPersistence`, and statistics.

use std::sync::Arc;

use rusqlite::Connection;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

/// Errors that can occur during retrieval persistence operations.
#[derive(Error, Debug)]
pub enum PersistError {
    /// SQLite operation failed.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

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

/// Retrieval index persistence using SQLite.
///
/// Provides methods to persist and restore HNSW and BM25 index snapshots
/// to/from SQLite. Uses the write-through pattern from khive-engine.
pub struct RetrievalPersistence {
    /// SQLite connection (thread-safe via async mutex).
    pub(crate) conn: Arc<Mutex<Connection>>,
    /// Namespace for multi-tenancy.
    /// Uses Arc<str> for O(1) cloning in async spawn contexts.
    pub(crate) namespace: Arc<str>,
}

impl RetrievalPersistence {
    /// Create a new persistence layer.
    pub fn new(conn: Arc<Mutex<Connection>>, namespace: impl Into<String>) -> Self {
        Self {
            conn,
            namespace: Arc::from(namespace.into()),
        }
    }

    /// Initialize the persistence schema.
    ///
    /// Creates tables for index snapshots if they don't exist.
    pub async fn init_schema(&self) -> Result<(), PersistError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS retrieval_snapshots (
                    namespace   TEXT NOT NULL,
                    index_type  TEXT NOT NULL,
                    snapshot    BLOB NOT NULL,
                    created_at  INTEGER NOT NULL,
                    PRIMARY KEY (namespace, index_type)
                );

                CREATE INDEX IF NOT EXISTS idx_retrieval_snapshots_namespace
                    ON retrieval_snapshots(namespace);
                "#,
            )?;
            Ok(())
        })
        .await
        .map_err(|e| PersistError::TaskJoin(e.to_string()))?
    }

    /// Generic snapshot persistence.
    pub(crate) async fn persist_snapshot<T: Serialize + Send + Sync>(
        &self,
        index_type: &str,
        snapshot: &T,
    ) -> Result<(), PersistError> {
        let data =
            serde_json::to_vec(snapshot).map_err(|e| PersistError::Serialize(e.to_string()))?;

        let conn = self.conn.clone();
        let namespace = self.namespace.clone();
        let index_type = index_type.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                r#"
                INSERT OR REPLACE INTO retrieval_snapshots
                    (namespace, index_type, snapshot, created_at)
                VALUES
                    (?1, ?2, ?3, ?4)
                "#,
                rusqlite::params![
                    &*namespace,
                    index_type,
                    data,
                    chrono::Utc::now().timestamp_micros()
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| PersistError::TaskJoin(e.to_string()))?
    }

    /// Generic snapshot loading.
    pub(crate) async fn load_snapshot<T: DeserializeOwned + Send + 'static>(
        &self,
        index_type: &str,
    ) -> Result<Option<T>, PersistError> {
        let conn = self.conn.clone();
        let namespace = self.namespace.clone();
        let index_type = index_type.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                r#"
                SELECT snapshot FROM retrieval_snapshots
                WHERE namespace = ?1 AND index_type = ?2
                "#,
            )?;

            let result: Option<Vec<u8>> = match stmt
                .query_row(rusqlite::params![&*namespace, index_type], |row| row.get(0))
            {
                Ok(data) => Some(data),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(PersistError::Sqlite(e)),
            };

            match result {
                Some(data) => {
                    let snapshot: T = serde_json::from_slice(&data)
                        .map_err(|e| PersistError::Deserialize(e.to_string()))?;
                    Ok(Some(snapshot))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| PersistError::TaskJoin(e.to_string()))?
    }

    /// Delete all snapshots for this namespace.
    pub async fn clear(&self) -> Result<(), PersistError> {
        let conn = self.conn.clone();
        let namespace = self.namespace.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM retrieval_snapshots WHERE namespace = ?1",
                rusqlite::params![&*namespace],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| PersistError::TaskJoin(e.to_string()))?
    }

    /// Get persistence statistics.
    pub async fn stats(&self) -> Result<PersistenceStats, PersistError> {
        let conn = self.conn.clone();
        let namespace = self.namespace.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                r#"
                SELECT index_type, length(snapshot), created_at
                FROM retrieval_snapshots
                WHERE namespace = ?1
                "#,
            )?;

            let mut stats = PersistenceStats::default();
            let mut rows = stmt.query(rusqlite::params![&*namespace])?;

            while let Some(row) = rows.next()? {
                let index_type: String = row.get(0)?;
                let size: i64 = row.get(1)?;
                let created_at: i64 = row.get(2)?;

                match index_type.as_str() {
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
        })
        .await
        .map_err(|e| PersistError::TaskJoin(e.to_string()))?
    }
}

/// Statistics about persisted snapshots.
#[derive(Debug, Default, Clone)]
pub struct PersistenceStats {
    /// Size of HNSW snapshot in bytes.
    pub hnsw_snapshot_size: usize,
    /// Timestamp when HNSW snapshot was created (Unix seconds).
    pub hnsw_snapshot_at: Option<i64>,
    /// Size of BM25 snapshot in bytes.
    pub bm25_snapshot_size: usize,
    /// Timestamp when BM25 snapshot was created (Unix seconds).
    pub bm25_snapshot_at: Option<i64>,
}
