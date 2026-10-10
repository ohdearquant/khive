//! sqlite-vec implementation of the storage capability trait.

use super::{
    async_trait, batch_insert_vectors_dml, bind_params, current_failpoint,
    delete_vector_provenance, delete_vector_statement, delete_vector_subjects_dml,
    log_vector_deletes, map_err, non_finite_index, non_finite_vector_error, orphan_sweep_dml,
    provenance_read_sql, provenance_sidecar_exists, replace_vector_row_dml, sqlite_cosine_score,
    vec_upsert_atomic_dml, BatchWriteSummary, ContentRef, DateTime, HashSet, IndexRebuildScope,
    OnceLock, OptionalExtension, OrphanSweepConfig, OrphanSweepResult, SqliteVecStore,
    StorageCapability, StorageError, StorageResult, SubstrateKind, Utc, Uuid, VectorIndexKind,
    VectorProvenance, VectorRecord, VectorRowRef, VectorSearchHit, VectorSearchRequest,
    VectorStore, VectorStoreCapabilities, VectorStoreInfo,
};
use khive_storage::{decode_f32_native, encode_f32_native};

#[async_trait]
impl VectorStore for SqliteVecStore {
    async fn score_candidates(
        &self,
        query_embedding: &[f32],
        candidate_ids: &[Uuid],
        kind: Option<SubstrateKind>,
    ) -> StorageResult<Vec<VectorSearchHit>> {
        let mut unique_ids = candidate_ids.to_vec();
        unique_ids.sort_unstable();
        unique_ids.dedup();
        self.score_candidates_with_kind(query_embedding, &unique_ids, kind)
            .await
    }

    async fn insert(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), StorageError> {
        self.insert_one(
            subject_id,
            kind,
            namespace,
            field,
            vectors,
            true,
            "vec_insert",
            "vec_insert_atomic",
        )
        .await
    }

    async fn insert_exact_only(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), StorageError> {
        self.insert_one(
            subject_id,
            kind,
            namespace,
            field,
            vectors,
            false,
            "vec_insert_exact_only",
            "vec_insert_exact_only_atomic",
        )
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

        // Capture the failpoint Arc (if any) from the thread-local on the
        // calling thread before handing the closure to spawn_blocking — both
        // the WriterTask path and the legacy path eventually run the closure
        // on a different thread than the one that reads the thread-local.
        let failpoint_flag = current_failpoint();

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure (the per-record
        // `SAVEPOINT vec_batch_record` is preserved unchanged — only the
        // OUTER BEGIN IMMEDIATE/COMMIT is removed, since the WriterTask's
        // run loop owns the enclosing transaction).
        if let Some(writer_task) = self.current_writer_task("vec_insert_batch")? {
            let table2 = table.clone();
            let store_embedding_model2 = store_embedding_model.clone();
            return writer_task
                .send_bounded(move |conn| {
                    batch_insert_vectors_dml(
                        conn,
                        &table2,
                        dims,
                        &store_embedding_model2,
                        &records,
                        attempted,
                        failpoint_flag,
                    )
                    .map_err(|e| map_err(e, "vec_insert_batch"))
                })
                .await;
        }

        // The direct pooled fallback opens and admits one transaction before
        // this DML body, then settles it after the complete batch.
        self.with_writer("vec_insert_batch", move |conn| {
            batch_insert_vectors_dml(
                conn,
                &table,
                dims,
                &store_embedding_model,
                &records,
                attempted,
                failpoint_flag,
            )
        })
        .await
    }

    async fn provenance(&self, subject_id: Uuid) -> Result<Option<VectorProvenance>, StorageError> {
        let table = self.table_name.clone();
        let model_key = self.model_key.clone();
        let namespace = self.namespace.clone();
        self.with_reader("vec_provenance", move |conn| {
            let has_sidecar = provenance_sidecar_exists(conn)?;
            let sql = provenance_read_sql(&table, has_sidecar);
            let subject_id = subject_id.to_string();
            let with_sidecar: [&dyn rusqlite::ToSql; 3] = [&model_key, &subject_id, &namespace];
            let without_sidecar: [&dyn rusqlite::ToSql; 2] = [&subject_id, &namespace];
            let params: &[&dyn rusqlite::ToSql] = if has_sidecar {
                &with_sidecar
            } else {
                &without_sidecar
            };
            conn.query_row(&sql, params, |row| {
                let embedding_model = row.get(0)?;
                let field = row.get(1)?;
                let live_embedding: Vec<u8> = row.get(2)?;
                let stored_digest: Option<String> = row.get(3)?;
                let live_digest = blake3::hash(&live_embedding).to_hex().to_string();
                if stored_digest.as_deref() != Some(live_digest.as_str()) {
                    return Ok(VectorProvenance {
                        embedding_model,
                        field,
                        text_fingerprint: None,
                        updated_at: None,
                    });
                }
                let fingerprint: Option<String> = row.get(4)?;
                let text_fingerprint = fingerprint
                    .map(|raw| {
                        ContentRef::from_hex(raw).map_err(|message| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    message,
                                )),
                            )
                        })
                    })
                    .transpose()?;
                let timestamp: Option<String> = row.get(5)?;
                let updated_at = timestamp
                    .map(|raw| {
                        DateTime::parse_from_rfc3339(&raw)
                            .map(|value| value.with_timezone(&Utc))
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    5,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })
                    })
                    .transpose()?;
                Ok(VectorProvenance {
                    embedding_model,
                    field,
                    text_fingerprint,
                    updated_at,
                })
            })
            .optional()
        })
        .await
    }

    async fn update(
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
                operation: "vec_update".into(),
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
                return Err(non_finite_vector_error("vec_update", idx, embedding[idx]));
            }
        }

        // Capture the failpoint Arc (if any) from the thread-local on the
        // calling thread before handing the closure to spawn_blocking.
        let failpoint_flag = current_failpoint();

        // ADR-067 Component A (Fork C slice 2): when the write queue is
        // enabled, route through the pool-wide WriterTask. DML-only
        // closure — atomicity is provided by `vec_upsert_atomic_dml`'s
        // named SAVEPOINT rather than `conn.unchecked_transaction()`,
        // which would attempt a nested `BEGIN` and fail under the
        // WriterTask's already-open transaction.
        if let Some(writer_task) = self.current_writer_task("vec_update")? {
            let table2 = table.clone();
            let namespace2 = namespace.clone();
            let field2 = field.clone();
            let kind_str2 = kind_str.clone();
            let embedding_model2 = embedding_model.clone();
            let embedding2 = embedding.clone();
            return writer_task
                .send_bounded(move |conn| {
                    vec_upsert_atomic_dml(
                        conn,
                        &table2,
                        dims,
                        subject_id,
                        &kind_str2,
                        &namespace2,
                        &field2,
                        &embedding_model2,
                        &embedding2,
                        "vec_update_atomic",
                        true,
                        failpoint_flag,
                    )
                    .map_err(|e| map_err(e, "vec_update"))
                })
                .await;
        }

        // The direct pooled fallback owns the admitted transaction. The
        // DELETE+INSERT body is shared with the WriterTask/batch paths (#546).
        self.with_writer("vec_update", move |conn| {
            replace_vector_row_dml(
                conn,
                &table,
                dims,
                VectorRowRef {
                    subject_id,
                    namespace: &namespace,
                    kind: &kind_str,
                    field: &field,
                    embedding_model: &embedding_model,
                    embedding: &embedding,
                    text_fingerprint: None,
                    updated_at: None,
                },
                true,
                failpoint_flag,
            )
        })
        .await
    }

    async fn delete(&self, subject_id: Uuid) -> Result<bool, StorageError> {
        let statement = delete_vector_statement(&self.table_name, subject_id, &self.namespace);
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();

        self.with_writer("vec_delete", move |conn| {
            conn.execute_batch("SAVEPOINT vec_delete_log")?;
            let result = (|| {
                log_vector_deletes(
                    conn,
                    &table,
                    "subject_id = ?1 AND namespace = ?2",
                    &[&subject_id.to_string(), &namespace],
                )?;
                let mut stmt = conn.prepare(&statement.sql)?;
                bind_params(&mut stmt, &statement.params)?;
                let deleted = stmt.raw_execute()? > 0;
                if deleted {
                    delete_vector_provenance(conn, &table, &[subject_id.to_string()])?;
                }
                Ok(deleted)
            })();
            match result {
                Ok(v) => {
                    conn.execute_batch("RELEASE SAVEPOINT vec_delete_log")?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT vec_delete_log");
                    let _ = conn.execute_batch("RELEASE SAVEPOINT vec_delete_log");
                    Err(e)
                }
            }
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

            // Filter before the exact scan's total order and limit. A MATCH
            // query cannot order by a second key, so boundary ties require the
            // scalar distance over all eligible rows.
            let kind_clause = if kind_filter.is_some() {
                "AND kind = ?5"
            } else {
                ""
            };
            let sql = format!(
                "SELECT subject_id, vec_distance_cosine(embedding, ?1) AS exact_distance \
                 FROM {t} \
                 WHERE namespace = ?3 \
                   AND embedding_model = ?4 \
                   {kind_clause} \
                 ORDER BY exact_distance, subject_id \
                 LIMIT ?2",
                t = table,
                kind_clause = kind_clause
            );

            let query_blob = encode_f32_native(&query_embedding);
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

                hits.push(VectorSearchHit {
                    subject_id,
                    score: sqlite_cosine_score(distance)?,
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

        // The WriterTask owns one BEGIN IMMEDIATE/COMMIT/ROLLBACK around each
        // request. Submit the complete chunk loop as one DML-only request so a
        // failure in any chunk makes the task roll back the complete input.
        if let Some(writer_task) = self.current_writer_task("vec_delete_subjects")? {
            let table_for_error = table.clone();
            return writer_task
                .send_bounded(move |conn| {
                    delete_vector_subjects_dml(conn, &table, &id_strings)
                        .map_err(|e| map_err(e, "vec_delete_subjects"))
                })
                .await
                .map_err(|e| {
                    tracing::warn!(target: "khive_db::stores::vectors", error = %e, table = %table_for_error, "delete_subjects failed");
                    e
                });
        }

        // The direct pooled path owns an admitted transaction around all
        // chunks and verifies rollback/autocommit before returning the writer.
        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteVecDeleteSubjects);
        let table_for_error = table.clone();
        self.with_writer_unmanaged("vec_delete_subjects", move |conn| {
            delete_vector_subjects_dml(conn, &table, &id_strings)
        })
        .await
        .map_err(|e| {
            tracing::warn!(target: "khive_db::stores::vectors", error = %e, table = %table_for_error, "delete_subjects failed");
            e
        })
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
            // vec0's primary-key IN constraint otherwise selects a full scan.
            let sql = format!(
                "SELECT subject_id FROM {table} WHERE namespace = ?1 \
                 AND embedding_model = ?2 AND subject_id = ?3"
            );
            let mut stmt = conn.prepare(&sql)?;

            for id in id_strings {
                let id_str: Option<String> = stmt
                    .query_row(rusqlite::params![&namespace, &model, &id], |row| row.get(0))
                    .optional()?;
                if let Some(id_str) = id_str {
                    if let Ok(uuid) = Uuid::parse_str(&id_str) {
                        found.insert(uuid);
                    }
                }
            }

            Ok(found)
        })
        .await
    }

    async fn get_vectors(
        &self,
        ids: &[Uuid],
        namespace: &str,
        field: &str,
    ) -> StorageResult<std::collections::HashMap<Uuid, Vec<f32>>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let table = self.table_name.clone();
        let namespace = namespace.to_owned();
        let field = field.to_owned();
        let model = self.embedding_model.clone();
        let dims = self.dimensions;
        let ids = ids.to_vec();

        self.with_reader("vec_get_vectors", move |conn| {
            // The vec0 subject_id primary key constrains each lookup before
            // metadata filtering, so the work is bounded by ids.len().
            let sql = format!(
                "SELECT embedding FROM {table} WHERE subject_id = ?1 \
                 AND namespace = ?2 AND field = ?3 AND embedding_model = ?4"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut found = std::collections::HashMap::with_capacity(ids.len());
            for id in ids {
                let blob: Option<Vec<u8>> = stmt
                    .query_row(
                        rusqlite::params![id.to_string(), &namespace, &field, &model],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(blob) = blob {
                    if blob.len() != dims * std::mem::size_of::<f32>() {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "stored vector has {} bytes, expected {}",
                                    blob.len(),
                                    dims * std::mem::size_of::<f32>()
                                ),
                            )),
                        ));
                    }
                    // vec0 exposes its native f32 ABI; keep the dimension check above.
                    let vector = decode_f32_native(&blob).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            Box::new(error),
                        )
                    })?;
                    found.insert(id, vector);
                }
            }
            Ok(found)
        })
        .await
    }

    async fn orphan_sweep(&self, config: &OrphanSweepConfig) -> StorageResult<OrphanSweepResult> {
        let table = self.table_name.clone();

        // Serialize filter lists as JSON arrays for json_each() usage inside SQL.
        // An empty list becomes None, which binds as NULL; the IS NULL guard then
        // short-circuits to true, passing all rows through (= no filtering).
        let ns_json: Option<String> = if config.namespaces.is_empty() {
            None
        } else {
            serde_json::to_string(&config.namespaces).ok()
        };

        let kind_json: Option<String> = if config.substrate_kinds.is_empty() {
            None
        } else {
            let strs: Vec<String> = config
                .substrate_kinds
                .iter()
                .map(|k| k.to_string())
                .collect();
            serde_json::to_string(&strs).ok()
        };

        // None = all rows eligible; Some(ids) = only those IDs may be swept.
        let allow_json: Option<String> = config.subject_id_allowlist.as_ref().map(|ids| {
            let strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            serde_json::to_string(&strs).unwrap_or_default()
        });

        let max_delete = config.max_delete as i64;
        let dry_run = config.dry_run;

        // ADR-067 Amendment 1: when the write queue is enabled, route through
        // the pool-wide WriterTask. DML-only closure — `run_writer_task`'s
        // drain loop already owns the enclosing `BEGIN IMMEDIATE`/`COMMIT`/
        // `ROLLBACK` for this request, so the closure must not open or commit
        // its own transaction; issuing `Transaction::new_unchecked`'s `BEGIN
        // IMMEDIATE` here would violate SQLite's nested-transaction rule and
        // fail with `SQLITE_ERROR: cannot start a transaction within a
        // transaction` (ADR-067 lines 271-276).
        if let Some(writer_task) = self.current_writer_task("orphan_sweep")? {
            let table2 = table.clone();
            let ns_json2 = ns_json.clone();
            let kind_json2 = kind_json.clone();
            let allow_json2 = allow_json.clone();
            return writer_task
                .send_bounded(move |conn| {
                    orphan_sweep_dml(
                        conn,
                        &table2,
                        ns_json2.as_deref(),
                        kind_json2.as_deref(),
                        allow_json2.as_deref(),
                        max_delete,
                        dry_run,
                    )
                    .map_err(|e| map_err(e, "orphan_sweep"))
                })
                .await;
        }

        // The direct pooled fallback owns the admitted transaction around
        // this DML body and verifies rollback/autocommit on every outcome.
        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteOrphanSweep);
        self.with_writer_unmanaged("orphan_sweep", move |conn| {
            orphan_sweep_dml(
                conn,
                &table,
                ns_json.as_deref(),
                kind_json.as_deref(),
                allow_json.as_deref(),
                max_delete,
                dry_run,
            )
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
            supports_orphan_sweep: true,
            supports_vector_read: true,
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
