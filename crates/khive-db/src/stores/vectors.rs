//! sqlite-vec backed `VectorStore` implementation.
//!
//! Each `SqliteVecStore` manages a single vec0 virtual table for one embedding
//! model. The store is scoped to a namespace for tenant isolation.
//!
//! # Blob format
//!
//! sqlite-vec expects embeddings as contiguous little-endian f32 bytes.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, VectorIndexKind, VectorRecord, VectorSearchHit,
    VectorSearchRequest, VectorStoreInfo,
};
use khive_storage::StorageCapability;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

/// Cast a `&[f32]` slice to `&[u8]` for sqlite-vec blob binding.
///
/// # Safety
///
/// Safe: f32 has no alignment requirements beyond what &[u8] needs, the byte
/// length is exactly the input slice size, and the lifetime is tied to input.
fn f32_slice_as_bytes(data: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Vectors, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Vectors, op, e)
}

fn non_finite_index(data: &[f32]) -> Option<usize> {
    data.iter().position(|v| !v.is_finite())
}

fn non_finite_vector_error(op: &'static str, idx: usize, value: f32) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Vectors,
        operation: op.into(),
        message: format!(
            "non-finite value at index {idx}: {value} \
             (NaN/Inf values corrupt distance computations)"
        ),
    }
}

/// Validate that `model_key` is safe to interpolate into a SQLite table name.
fn validate_model_key(model_key: &str) -> Result<(), SqliteError> {
    if model_key.is_empty()
        || !model_key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(SqliteError::InvalidData(format!(
            "invalid model_key '{}': must be non-empty and contain only ASCII alphanumeric/underscore characters",
            model_key
        )));
    }
    Ok(())
}

/// A VectorStore backed by sqlite-vec's vec0 virtual tables.
///
/// Each instance manages one table `vec_{model_key}`. All operations are
/// scoped to the store's `namespace` for tenant isolation.
pub struct SqliteVecStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    model_key: String,
    dimensions: usize,
    table_name: String,
    namespace: String,
}

impl SqliteVecStore {
    /// Create a new store scoped to the given namespace.
    ///
    /// Returns an error if `model_key` contains characters unsafe for table name interpolation.
    pub fn new(
        pool: Arc<ConnectionPool>,
        is_file_backed: bool,
        model_key: String,
        dimensions: usize,
        namespace: String,
    ) -> Result<Self, SqliteError> {
        validate_model_key(&model_key)?;
        let table_name = format!("vec_{}", model_key);
        Ok(Self {
            pool,
            is_file_backed,
            model_key,
            dimensions,
            table_name,
            namespace,
        })
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "vec_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_vec_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_vec_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_vec_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_vec_reader"))?;

        Ok(conn)
    }

    /// Write via pool writer to serialize through the mutex.
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
            f(guard.conn()).map_err(|e| map_err(e, op))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            let conn = self.open_standalone_reader()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
        }
    }
}

#[async_trait]
impl VectorStore for SqliteVecStore {
    async fn insert(
        &self,
        subject_id: Uuid,
        _kind: SubstrateKind,
        namespace: &str,
        embedding: Vec<f32>,
    ) -> Result<(), StorageError> {
        let table = self.table_name.clone();
        let dims = self.dimensions;
        let namespace = namespace.to_owned();

        if embedding.len() == dims {
            if let Some(idx) = non_finite_index(&embedding) {
                return Err(non_finite_vector_error("vec_insert", idx, embedding[idx]));
            }
        }

        self.with_writer("vec_insert", move |conn| {
            if embedding.len() != dims {
                return Err(rusqlite::Error::InvalidParameterCount(
                    embedding.len(),
                    dims,
                ));
            }

            // vec0 does not support INSERT OR REPLACE — delete then insert.
            let del_sql = format!(
                "DELETE FROM {} WHERE entity_id = ?1 AND namespace = ?2",
                table
            );
            conn.execute(
                &del_sql,
                rusqlite::params![subject_id.to_string(), &namespace],
            )?;

            let ins_sql = format!(
                "INSERT INTO {} (entity_id, namespace, embedding) VALUES (?1, ?2, ?3)",
                table
            );
            let blob = f32_slice_as_bytes(&embedding);
            conn.execute(
                &ins_sql,
                rusqlite::params![subject_id.to_string(), &namespace, blob],
            )?;
            Ok(())
        })
        .await
    }

    async fn insert_batch(
        &self,
        records: Vec<VectorRecord>,
    ) -> Result<BatchWriteSummary, StorageError> {
        let table = self.table_name.clone();
        let dims = self.dimensions;
        let attempted = records.len() as u64;

        self.with_writer("vec_insert_batch", move |conn| {
            let del_sql = format!(
                "DELETE FROM {} WHERE entity_id = ?1 AND namespace = ?2",
                table
            );
            let ins_sql = format!(
                "INSERT INTO {} (entity_id, namespace, embedding) VALUES (?1, ?2, ?3)",
                table
            );

            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;

            for record in &records {
                if record.embedding.len() != dims {
                    failed += 1;
                    continue;
                }
                if non_finite_index(&record.embedding).is_some() {
                    failed += 1;
                    continue;
                }
                let blob = f32_slice_as_bytes(&record.embedding);
                let id_str = record.subject_id.to_string();
                let _ = conn.execute(&del_sql, rusqlite::params![&id_str, &record.namespace]);
                match conn.execute(
                    &ins_sql,
                    rusqlite::params![&id_str, &record.namespace, blob],
                ) {
                    Ok(_) => affected += 1,
                    Err(_) => failed += 1,
                }
            }

            conn.execute_batch("COMMIT")?;

            Ok(BatchWriteSummary {
                attempted,
                affected,
                failed,
                first_error: String::new(),
            })
        })
        .await
    }

    async fn delete(&self, subject_id: Uuid) -> Result<bool, StorageError> {
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();

        self.with_writer("vec_delete", move |conn| {
            let sql = format!(
                "DELETE FROM {} WHERE entity_id = ?1 AND namespace = ?2",
                table
            );
            let deleted =
                conn.execute(&sql, rusqlite::params![subject_id.to_string(), &namespace])?;
            Ok(deleted > 0)
        })
        .await
    }

    async fn count(&self) -> Result<u64, StorageError> {
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();

        self.with_reader("vec_count", move |conn| {
            let sql = format!("SELECT COUNT(*) FROM {} WHERE namespace = ?1", table);
            let count: i64 =
                conn.query_row(&sql, rusqlite::params![&namespace], |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    async fn search(
        &self,
        request: VectorSearchRequest,
    ) -> Result<Vec<VectorSearchHit>, StorageError> {
        let table = self.table_name.clone();
        let dims = self.dimensions;
        // Use the request namespace if provided, fall back to the store's namespace.
        let namespace = request
            .namespace
            .clone()
            .unwrap_or_else(|| self.namespace.clone());

        if request.query_embedding.len() == dims {
            if let Some(idx) = non_finite_index(&request.query_embedding) {
                return Err(non_finite_vector_error(
                    "vec_search",
                    idx,
                    request.query_embedding[idx],
                ));
            }
        }

        self.with_reader("vec_search", move |conn| {
            if request.query_embedding.len() != dims {
                return Err(rusqlite::Error::InvalidParameterCount(
                    request.query_embedding.len(),
                    dims,
                ));
            }

            // Restrict candidate set to namespace via subquery, then MATCH-rank.
            let sql = format!(
                "SELECT entity_id, distance \
                 FROM {t} \
                 WHERE embedding MATCH ?1 \
                   AND entity_id IN (SELECT entity_id FROM {t} WHERE namespace = ?3) \
                 ORDER BY distance \
                 LIMIT ?2",
                t = table
            );

            let query_blob = f32_slice_as_bytes(&request.query_embedding);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                rusqlite::params![query_blob, request.top_k, &namespace],
                |row| {
                    let id_str: String = row.get(0)?;
                    let distance: f64 = row.get(1)?;
                    Ok((id_str, distance))
                },
            )?;

            let mut hits = Vec::new();
            for (rank_idx, row) in rows.enumerate() {
                let (id_str, distance) = row?;
                let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                // sqlite-vec cosine distance: 0.0 = identical, 2.0 = opposite.
                // Convert to similarity in [0, 1]: score = 1.0 - (distance / 2.0)
                let similarity = 1.0 - (distance / 2.0);

                hits.push(VectorSearchHit {
                    subject_id,
                    score: DeterministicScore::from_f64(similarity),
                    rank: (rank_idx + 1) as u32,
                });
            }

            Ok(hits)
        })
        .await
    }

    async fn info(&self) -> Result<VectorStoreInfo, StorageError> {
        let count = self.count().await?;

        Ok(VectorStoreInfo {
            model_name: self.model_key.clone(),
            dimensions: self.dimensions,
            index_kind: VectorIndexKind::SqliteVec,
            entry_count: count,
            needs_rebuild: false,
            last_rebuild_at: None,
        })
    }

    async fn rebuild(&self, _scope: IndexRebuildScope) -> Result<VectorStoreInfo, StorageError> {
        // sqlite-vec uses brute-force search — no index to rebuild.
        self.info().await
    }
}

impl SqliteVecStore {
    /// Score a fixed set of candidate IDs against a query embedding.
    ///
    /// Unlike `search`, this does not use the MATCH index — it computes cosine
    /// distance directly for the supplied IDs only. Results are returned sorted
    /// by descending score.
    pub async fn score_candidates(
        &self,
        query_embedding: &[f32],
        candidate_ids: &[Uuid],
    ) -> Result<Vec<VectorSearchHit>, StorageError> {
        if candidate_ids.is_empty() || query_embedding.is_empty() {
            return Ok(Vec::new());
        }

        let dims = self.dimensions;
        if query_embedding.len() != dims {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Vectors,
                operation: "score_candidates".into(),
                message: format!(
                    "query has {} dims, expected {}",
                    query_embedding.len(),
                    dims
                ),
            });
        }

        if let Some(idx) = non_finite_index(query_embedding) {
            return Err(non_finite_vector_error(
                "score_candidates",
                idx,
                query_embedding[idx],
            ));
        }

        let table = self.table_name.clone();
        let namespace = self.namespace.clone();
        let query_vec = query_embedding.to_vec();
        let ids: Vec<String> = candidate_ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("score_candidates", move |conn| {
            let mut all_hits: Vec<VectorSearchHit> = Vec::new();
            let query_blob = f32_slice_as_bytes(&query_vec);

            for chunk in ids.chunks(399) {
                let placeholders: String = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 3))
                    .collect::<Vec<_>>()
                    .join(", ");

                let sql = format!(
                    "SELECT e.entity_id, vec_distance_cosine(e.embedding, ?1) as distance \
                     FROM {} e \
                     WHERE e.namespace = ?2 AND e.entity_id IN ({})",
                    table, placeholders
                );

                let mut stmt = conn.prepare(&sql)?;
                stmt.raw_bind_parameter(1, query_blob)?;
                stmt.raw_bind_parameter(2, namespace.as_str())?;
                for (i, id_str) in chunk.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 3, id_str.as_str())?;
                }

                let mut rows = stmt.raw_query();
                while let Some(row) = rows.next()? {
                    let id_str: String = row.get(0)?;
                    let distance: f64 = row.get(1)?;

                    let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;

                    let similarity = 1.0 - (distance / 2.0);
                    all_hits.push(VectorSearchHit {
                        subject_id,
                        score: DeterministicScore::from_f64(similarity),
                        rank: 0,
                    });
                }
            }

            all_hits.sort_by(|a, b| b.score.cmp(&a.score));
            for (i, hit) in all_hits.iter_mut().enumerate() {
                hit.rank = (i + 1) as u32;
            }

            Ok(all_hits)
        })
        .await
    }
}
