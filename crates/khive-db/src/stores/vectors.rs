//! sqlite-vec backed `VectorStore`: one vec0 table per embedding model, scoped to namespace.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, VectorIndexKind, VectorRecord, VectorSearchHit,
    VectorSearchRequest, VectorStoreCapabilities, VectorStoreInfo,
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
    // SAFETY: `data` is a valid &[f32] so the pointer is non-null, well-aligned, and
    // live for the call duration. u8 alignment is 1 (satisfied by any allocation).
    // size_of_val gives the exact byte count. The returned slice borrows `data`
    // so its lifetime cannot outlive the input reference.
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
/// Each instance manages one table `vec_{model_key}`. The `namespace` field
/// is a default for trait methods that lack a per-call namespace parameter
/// (count, delete, info). Access control is enforced at the runtime layer.
pub struct SqliteVecStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    model_key: String,
    embedding_model: String,
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
        embedding_model: String,
        dimensions: usize,
        namespace: String,
    ) -> Result<Self, SqliteError> {
        validate_model_key(&model_key)?;
        let table_name = format!("vec_{}", model_key);
        Ok(Self {
            pool,
            is_file_backed,
            model_key,
            embedding_model,
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
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), StorageError> {
        if vectors.len() != 1 {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_insert".into(),
                message: "sqlite-vec supports exactly one vector per record".into(),
            });
        }
        let embedding = vectors.into_iter().next().expect("len checked");

        let table = self.table_name.clone();
        let dims = self.dimensions;
        let namespace = namespace.to_string();
        let field = field.to_string();
        let kind_str = kind.to_string();
        let embedding_model = self.embedding_model.clone();

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
            // Wrap in a transaction so a failed INSERT rolls back the DELETE,
            // leaving the previous vector intact (no-worse-than-stale guarantee).
            let tx = conn.unchecked_transaction()?;

            let del_sql = format!(
                "DELETE FROM {} WHERE subject_id = ?1 AND namespace = ?2",
                table
            );
            tx.execute(
                &del_sql,
                rusqlite::params![subject_id.to_string(), &namespace],
            )?;

            let ins_sql = format!(
                "INSERT INTO {} (subject_id, namespace, kind, field, embedding_model, embedding) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                table
            );
            let blob = f32_slice_as_bytes(&embedding);
            tx.execute(
                &ins_sql,
                rusqlite::params![
                    subject_id.to_string(),
                    &namespace,
                    &kind_str,
                    &field,
                    &embedding_model,
                    blob
                ],
            )?;

            tx.commit()
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
        let store_embedding_model = self.embedding_model.clone();

        self.with_writer("vec_insert_batch", move |conn| {
            let del_sql = format!(
                "DELETE FROM {} WHERE subject_id = ?1 AND namespace = ?2",
                table
            );
            let ins_sql = format!(
                "INSERT INTO {} (subject_id, namespace, kind, field, embedding_model, embedding) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                table
            );

            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;

            for record in &records {
                if record.vectors.len() != 1 {
                    failed += 1;
                    continue;
                }
                let embedding = &record.vectors[0];
                if embedding.len() != dims {
                    failed += 1;
                    continue;
                }
                if non_finite_index(embedding).is_some() {
                    failed += 1;
                    continue;
                }
                let blob = f32_slice_as_bytes(embedding);
                let id_str = record.subject_id.to_string();
                let kind_str = record.kind.to_string();
                // Use the record's own namespace — the caller is responsible for namespace.
                let _ = conn.execute(&del_sql, rusqlite::params![&id_str, &record.namespace]);
                match conn.execute(
                    &ins_sql,
                    rusqlite::params![
                        &id_str,
                        &record.namespace,
                        &kind_str,
                        &record.field,
                        &store_embedding_model,
                        blob
                    ],
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
                "DELETE FROM {} WHERE subject_id = ?1 AND namespace = ?2",
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
        if request.filter.as_ref().is_some_and(|f| !f.is_empty()) {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_search".into(),
                message: "use search_with_filter for filtered queries".into(),
            });
        }
        if request.query_vectors.len() != 1 {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_search".into(),
                message: "sqlite-vec supports exactly one query vector per search".into(),
            });
        }
        let query_embedding = request.query_vectors[0].clone();

        let table = self.table_name.clone();
        let dims = self.dimensions;
        // Use request.namespace if present; fall back to self.namespace.
        let namespace = request
            .namespace
            .clone()
            .unwrap_or_else(|| self.namespace.clone());
        let kind_filter = request.kind.map(|k| k.to_string());
        // Use the request's embedding_model filter, or fall back to this store's model.
        let effective_model = request
            .embedding_model
            .clone()
            .unwrap_or_else(|| self.embedding_model.clone());

        if query_embedding.len() == dims {
            if let Some(idx) = non_finite_index(&query_embedding) {
                return Err(non_finite_vector_error(
                    "vec_search",
                    idx,
                    query_embedding[idx],
                ));
            }
        }

        self.with_reader("vec_search", move |conn| {
            if query_embedding.len() != dims {
                return Err(rusqlite::Error::InvalidParameterCount(
                    query_embedding.len(),
                    dims,
                ));
            }

            // Push namespace+embedding_model (and optionally kind) directly into
            // the MATCH predicate so sqlite-vec evaluates them before computing
            // global top-k, preventing cross-namespace recall starvation.
            let kind_clause = if kind_filter.is_some() {
                "AND kind = ?5"
            } else {
                ""
            };
            let sql = format!(
                "SELECT subject_id, distance \
                 FROM {t} \
                 WHERE embedding MATCH ?1 \
                   AND namespace = ?3 \
                   AND embedding_model = ?4 \
                   {kind_clause} \
                 ORDER BY distance \
                 LIMIT ?2",
                t = table,
                kind_clause = kind_clause
            );

            let query_blob = f32_slice_as_bytes(&query_embedding);
            let mut stmt = conn.prepare(&sql)?;

            // Collect rows into a Vec to avoid holding MappedRows (which is
            // parameterised on its closure type) across both branches.
            let raw_rows: Vec<rusqlite::Result<(String, f64)>> =
                if let Some(ref kind_str) = kind_filter {
                    stmt.query_map(
                        rusqlite::params![
                            query_blob,
                            request.top_k,
                            &namespace,
                            &effective_model,
                            kind_str
                        ],
                        |row| {
                            let id_str: String = row.get(0)?;
                            let distance: f64 = row.get(1)?;
                            Ok((id_str, distance))
                        },
                    )?
                    .collect()
                } else {
                    stmt.query_map(
                        rusqlite::params![query_blob, request.top_k, &namespace, &effective_model],
                        |row| {
                            let id_str: String = row.get(0)?;
                            let distance: f64 = row.get(1)?;
                            Ok((id_str, distance))
                        },
                    )?
                    .collect()
                };

            let mut hits = Vec::new();
            for (rank_idx, row) in raw_rows.into_iter().enumerate() {
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

    async fn delete_subjects(&self, ids: &[Uuid]) -> Result<u64, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let table = self.table_name.clone();
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let mut total_deleted: u64 = 0;

        // Batch in ≤400 IDs per statement to stay within SQLite's variable limit.
        for chunk in id_strings.chunks(400) {
            let placeholders: String = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("DELETE FROM {table} WHERE subject_id IN ({placeholders})");
            let chunk_owned = chunk.to_vec();
            let table_cl = table.clone();
            let deleted = self
                .with_writer("vec_delete_subjects", move |conn| {
                    let mut stmt = conn.prepare(&sql)?;
                    for (i, id_str) in chunk_owned.iter().enumerate() {
                        stmt.raw_bind_parameter(i + 1, id_str.as_str())?;
                    }
                    stmt.raw_execute().map(|n| n as u64)
                })
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, table = %table_cl, "delete_subjects chunk failed");
                    e
                })?;
            total_deleted += deleted;
        }
        Ok(total_deleted)
    }

    async fn batch_exists(
        &self,
        ids: &[Uuid],
        namespace: &str,
    ) -> Result<HashSet<Uuid>, StorageError> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }

        let table = self.table_name.clone();
        let namespace = namespace.to_string();
        let model = self.embedding_model.clone();
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("vec_batch_exists", move |conn| {
            let mut found = HashSet::new();

            for chunk in id_strings.chunks(400) {
                // ?1 = namespace, ?2 = embedding_model, ?3.. = subject IDs.
                let placeholders: String = (0..chunk.len())
                    .map(|i| format!("?{}", i + 3))
                    .collect::<Vec<_>>()
                    .join(", ");

                let sql = format!(
                    "SELECT subject_id FROM {} WHERE namespace = ?1 \
                     AND embedding_model = ?2 AND subject_id IN ({})",
                    table, placeholders
                );

                let mut stmt = conn.prepare(&sql)?;
                stmt.raw_bind_parameter(1, namespace.as_str())?;
                stmt.raw_bind_parameter(2, model.as_str())?;
                for (i, id_str) in chunk.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 3, id_str.as_str())?;
                }

                let mut rows = stmt.raw_query();
                while let Some(row) = rows.next()? {
                    let id_str: String = row.get(0)?;
                    if let Ok(uuid) = Uuid::parse_str(&id_str) {
                        found.insert(uuid);
                    }
                }
            }

            Ok(found)
        })
        .await
    }

    fn capabilities(&self) -> &'static VectorStoreCapabilities {
        static SQLITE_VEC_CAPABILITIES: OnceLock<VectorStoreCapabilities> = OnceLock::new();
        SQLITE_VEC_CAPABILITIES.get_or_init(|| VectorStoreCapabilities {
            supports_filter: false,
            supports_batch_search: false,
            supports_quantization: false,
            supports_update: false,
            supports_orphan_sweep: false,
            // sqlite-vec uses subject_id as PRIMARY KEY — only one vector per
            // subject per namespace is stored. Callers must use a single canonical
            // field (e.g. "content") and are not permitted to store both
            // "entity.title" and "entity.body" as separate vectors in one table.
            supports_multi_field: false,
            // sqlite-vec 0.1.9 rejects dimensions > SQLITE_VEC_VEC0_MAX_DIMENSIONS (8192).
            // Reporting 8192 lets callers know that 4097–8192 dimensional models are
            // supported. The previous value of 4096 was the K_MAX (neighbors per query)
            // constant, not the dimension limit.
            max_dimensions: Some(8192),
            index_kinds: vec![VectorIndexKind::SqliteVec],
        })
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
        let embedding_model = self.embedding_model.clone();
        let query_vec = query_embedding.to_vec();
        let ids: Vec<String> = candidate_ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("score_candidates", move |conn| {
            let mut all_hits: Vec<VectorSearchHit> = Vec::new();
            let query_blob = f32_slice_as_bytes(&query_vec);

            for chunk in ids.chunks(399) {
                let placeholders: String = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 4))
                    .collect::<Vec<_>>()
                    .join(", ");

                let sql = format!(
                    "SELECT e.subject_id, vec_distance_cosine(e.embedding, ?1) as distance \
                     FROM {} e \
                     WHERE e.namespace = ?2 AND e.embedding_model = ?3 \
                       AND e.subject_id IN ({})",
                    table, placeholders
                );

                let mut stmt = conn.prepare(&sql)?;
                stmt.raw_bind_parameter(1, query_blob)?;
                stmt.raw_bind_parameter(2, namespace.as_str())?;
                stmt.raw_bind_parameter(3, embedding_model.as_str())?;
                for (i, id_str) in chunk.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 4, id_str.as_str())?;
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

#[cfg(all(test, feature = "vectors"))]
mod batch_exists_tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use khive_types::SubstrateKind;
    use uuid::Uuid;

    use super::*;

    fn make_vec_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        crate::extension::ensure_extensions_loaded();
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
    }

    fn create_vec_table(pool: &Arc<crate::pool::ConnectionPool>, model_key: &str, dims: usize) {
        let writer = pool.try_writer().expect("pool writer");
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
            model_key, dims
        );
        writer.conn().execute_batch(&ddl).expect("create vec table");
    }

    /// Valid (underscored) model key: batch_exists returns the exact set of IDs
    /// that have embeddings and excludes IDs that were never inserted.
    #[tokio::test]
    async fn batch_exists_returns_correct_set_for_underscored_model_key() {
        let pool = make_vec_pool();
        let model_key = "all_minilm_l6_v2";
        let dims = 4;
        let ns = "ns:test";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id_absent = Uuid::new_v4();

        store
            .insert(
                id1,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![0.1, 0.2, 0.3, 0.4]],
            )
            .await
            .expect("insert id1");
        store
            .insert(
                id2,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![0.5, 0.6, 0.7, 0.8]],
            )
            .await
            .expect("insert id2");

        let exists = store
            .batch_exists(&[id1, id2, id_absent], ns)
            .await
            .expect("batch_exists");

        assert!(exists.contains(&id1), "id1 must be found");
        assert!(exists.contains(&id2), "id2 must be found");
        assert!(
            !exists.contains(&id_absent),
            "absent id must not be returned"
        );
        assert_eq!(exists.len(), 2);
    }

    /// Empty input must return an empty set without hitting the DB.
    #[tokio::test]
    async fn batch_exists_empty_ids_returns_empty_set() {
        let pool = make_vec_pool();
        let model_key = "empty_test_model";
        create_vec_table(&pool, model_key, 4);

        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            4,
            "ns:test".to_string(),
        )
        .expect("SqliteVecStore::new");

        let exists: HashSet<Uuid> = store
            .batch_exists(&[], "ns:test")
            .await
            .expect("batch_exists");
        assert!(exists.is_empty());
    }

    /// A nearer vector in namespace A must not starve the top-k result in namespace B.
    ///
    /// Regression for the cross-namespace recall starvation path: sqlite-vec must
    /// evaluate the namespace predicate before computing global top-k, not after.
    #[tokio::test]
    async fn vector_search_namespace_predicate_prevents_recall_starvation() {
        let pool = make_vec_pool();
        let model_key = "knn_namespace_scope";
        let dims = 4;
        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            "ns:b".to_string(),
        )
        .expect("SqliteVecStore::new");

        let distractor_a = Uuid::new_v4();
        let victim_b = Uuid::new_v4();

        // Insert a nearer vector in namespace A (distractor).
        store
            .insert(
                distractor_a,
                SubstrateKind::Entity,
                "ns:a",
                "body",
                vec![vec![1.0, 0.0, 0.0, 0.0]],
            )
            .await
            .expect("insert nearer cross-namespace vector");

        // Insert a slightly farther vector in namespace B (victim).
        store
            .insert(
                victim_b,
                SubstrateKind::Entity,
                "ns:b",
                "body",
                vec![vec![0.8, 0.2, 0.0, 0.0]],
            )
            .await
            .expect("insert in-namespace vector");

        // top_k=1 search in ns:b must return victim_b, not the nearer distractor_a.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec![1.0, 0.0, 0.0, 0.0]],
                top_k: 1,
                namespace: Some("ns:b".to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search");

        assert_eq!(
            hits.len(),
            1,
            "namespace B must not be starved by namespace A"
        );
        assert_eq!(
            hits[0].subject_id, victim_b,
            "top-1 in ns:b must be victim_b, not cross-namespace distractor_a"
        );
    }

    /// Hyphenated model_key must be rejected at SqliteVecStore::new(), preventing
    /// any table-name divergence between the store and a hand-rolled sanitizer.
    #[test]
    fn hyphenated_model_key_is_rejected_at_construction() {
        use crate::pool::{ConnectionPool, PoolConfig};
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                ..PoolConfig::default()
            })
            .expect("pool"),
        );

        let result = SqliteVecStore::new(
            pool,
            false,
            "all-minilm-l6-v2".to_string(),
            "all-minilm-l6-v2".to_string(),
            4,
            "ns:test".to_string(),
        );

        assert!(
            result.is_err(),
            "hyphenated model_key 'all-minilm-l6-v2' must be rejected; \
             the store's table_name would differ from what a hand-rolled sanitizer produces"
        );
    }
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;

    fn make_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
    }

    #[test]
    fn sqlite_vec_store_capabilities_are_correct() {
        let store = SqliteVecStore::new(
            make_pool(),
            /*is_file_backed=*/ false,
            "test_model".into(),
            "test_model".into(),
            /*dimensions=*/ 4,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        let caps = store.capabilities();

        assert!(
            !caps.supports_filter,
            "sqlite-vec does not support filter pushdown"
        );
        assert!(
            !caps.supports_batch_search,
            "sqlite-vec does not support native batch search"
        );
        assert!(
            !caps.supports_quantization,
            "sqlite-vec does not support quantization"
        );
        assert!(
            !caps.supports_update,
            "sqlite-vec does not support in-place update"
        );
        assert!(
            !caps.supports_orphan_sweep,
            "sqlite-vec does not support orphan sweep"
        );
        // sqlite-vec 0.1.9: SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192.
        assert_eq!(caps.max_dimensions, Some(8192));
        assert_eq!(
            caps.index_kinds,
            vec![VectorIndexKind::SqliteVec],
            "index_kinds should be [SqliteVec]"
        );
    }

    /// Regression: max_dimensions must equal the sqlite-vec hard limit (8192),
    /// not the K_MAX constant (4096). A caller with 5000-dim embeddings must not
    /// be falsely told the backend is incapable.
    #[test]
    fn max_dimensions_reflects_sqlite_vec_hard_limit_not_k_max() {
        let store = SqliteVecStore::new(
            make_pool(),
            false,
            "test_dim_limit".into(),
            "test_dim_limit".into(),
            /*dimensions=*/ 4,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        let caps = store.capabilities();

        // SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192 (sqlite-vec.c:3488).
        // The previous incorrect value 4096 was SQLITE_VEC_VEC0_K_MAX (max neighbours),
        // which would falsely reject 4097–8192 dimensional models.
        let max = caps
            .max_dimensions
            .expect("SqliteVecStore must declare a finite dimension limit");
        assert!(
            max >= 8192,
            "max_dimensions ({max}) must be at least 8192 — the sqlite-vec hard limit"
        );
    }

    /// Capabilities struct is returned by &'static reference; calling twice must
    /// return the same value (OnceLock semantics, no allocation on repeat calls).
    #[test]
    fn capabilities_is_idempotent() {
        let store = SqliteVecStore::new(
            make_pool(),
            false,
            "test_idempotent".into(),
            "test_idempotent".into(),
            4,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        let caps1 = store.capabilities();
        let caps2 = store.capabilities();
        assert_eq!(
            caps1 as *const _, caps2 as *const _,
            "capabilities() must return the same static reference each call"
        );
    }
}
