#[cfg(test)]
use super::failpoint;
use super::{
    non_finite_index, provenance_sidecar_exists, BatchWriteErrorClass, BatchWriteRetryability,
    BatchWriteSummary, ContentRef, DateTime, OrphanSweepResult, Utc, Uuid, VectorRecord,
};
use khive_storage::encode_f32_native;

/// One vector row's identity + payload for [`replace_vector_row_dml`] (#546).
/// `embedding` must already be validated for the target table's dimension
/// count (or delegated to the helper's own dimension check).
pub(super) struct VectorRowRef<'a> {
    pub(super) subject_id: Uuid,
    pub(super) namespace: &'a str,
    pub(super) kind: &'a str,
    pub(super) field: &'a str,
    pub(super) embedding_model: &'a str,
    pub(super) embedding: &'a [f32],
    pub(super) text_fingerprint: Option<&'a ContentRef>,
    pub(super) updated_at: Option<&'a DateTime<Utc>>,
}

/// Shared DELETE-then-INSERT replacement DML for a single vector row (#546);
/// caller owns the enclosing transaction/savepoint. See
/// crates/khive-db/docs/api/vectors.md#replace_vector_row_dml--shared-delete-then-insert-replacement-546
pub(super) fn replace_vector_row_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    row: VectorRowRef<'_>,
    record_ann_delta: bool,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), rusqlite::Error> {
    if row.embedding.len() != dims {
        return Err(rusqlite::Error::InvalidParameterCount(
            row.embedding.len(),
            dims,
        ));
    }

    // Vector tables use subject_id as their primary key. Delete the common
    // same-identity row directly; its incoming upsert log is sufficient. Only
    // the metadata-repair path needs to discover and log the old ANN identity.
    // The caller's transaction/savepoint restores the prior row on failure.
    let subject_id = row.subject_id.to_string();
    let delete_same_identity_sql = format!(
        "DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2 \
         AND embedding_model = ?3 AND kind = ?4 AND field = ?5"
    );
    let deleted_same_identity = conn.execute(
        &delete_same_identity_sql,
        rusqlite::params![
            &subject_id,
            row.namespace,
            row.embedding_model,
            row.kind,
            row.field
        ],
    )?;
    if deleted_same_identity == 0 {
        if record_ann_delta {
            let logged = log_vector_deletes(conn, table, "subject_id = ?1", &[&subject_id])?;
            if logged > 0 {
                let delete_prior_identity_sql =
                    format!("DELETE FROM {table} WHERE subject_id = ?1");
                conn.execute(&delete_prior_identity_sql, rusqlite::params![&subject_id])?;
            }
        } else {
            let delete_prior_identity_sql = format!("DELETE FROM {table} WHERE subject_id = ?1");
            conn.execute(&delete_prior_identity_sql, rusqlite::params![&subject_id])?;
        }
    }

    // Failpoint: fires only in cfg(test) when the guard is active. DELETE has
    // already run; if the caller's rollback (transaction or SAVEPOINT) is
    // missing, the deleted row is lost permanently.
    #[cfg(test)]
    if let Some(ref fp) = failpoint_flag {
        if failpoint::take(fp) {
            return Err(rusqlite::Error::InvalidParameterName(
                "__test_failpoint_after_delete__".into(),
            ));
        }
    }
    #[cfg(not(test))]
    let _ = failpoint_flag;

    let ins_sql = format!(
        "INSERT INTO {table} (subject_id, namespace, kind, field, embedding_model, embedding) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    );
    let blob = encode_f32_native(row.embedding);
    conn.execute(
        &ins_sql,
        rusqlite::params![
            &subject_id,
            row.namespace,
            row.kind,
            row.field,
            row.embedding_model,
            blob
        ],
    )?;

    if provenance_sidecar_exists(conn)? {
        let model_key = table
            .strip_prefix("vec_")
            .expect("vector table names use the vec_ prefix");
        // Bind provenance to the bytes the live vec0 table exposes, rather than
        // assuming its read representation matches the input slice's layout.
        let stored_embedding: Vec<u8> = conn.query_row(
            &format!("SELECT embedding FROM {table} WHERE subject_id = ?1 AND namespace = ?2"),
            rusqlite::params![&subject_id, row.namespace],
            |stored| stored.get(0),
        )?;
        let embedding_digest = blake3::hash(&stored_embedding).to_hex().to_string();
        let updated_at = row.updated_at.map(DateTime::to_rfc3339);
        conn.execute(
            "INSERT INTO vector_provenance \
             (model_key, subject_id, namespace, embedding_digest, text_fingerprint, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(model_key, subject_id) DO UPDATE SET \
             namespace = excluded.namespace, \
             embedding_digest = excluded.embedding_digest, \
             text_fingerprint = excluded.text_fingerprint, \
             updated_at = excluded.updated_at",
            rusqlite::params![
                model_key,
                &subject_id,
                row.namespace,
                embedding_digest,
                row.text_fingerprint.map(ContentRef::as_str),
                updated_at,
            ],
        )?;
    }

    if record_ann_delta {
        // Delta record for the ANN restart classifier; rides the caller's
        // savepoint/transaction so a rolled-back upsert leaves no log row.
        conn.execute(
            "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')",
            rusqlite::params![
                row.namespace,
                row.embedding_model,
                row.kind,
                row.field,
                &subject_id
            ],
        )?;
    }

    Ok(())
}

/// Log `'delete'` rows into `ann_write_log` for every vector row in `table`
/// matching `where_clause` (a predicate over the vec0 table's own columns).
/// Must run in the same transaction as — and before — the corresponding
/// `DELETE`, so the logged set is exactly the deleted set. Returns the number
/// of identities logged.
pub(super) fn log_vector_deletes(
    conn: &rusqlite::Connection,
    table: &str,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<usize, rusqlite::Error> {
    let sql = format!(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
         FROM {table} WHERE {where_clause}"
    );
    conn.execute(&sql, params)
}

pub(super) fn delete_vector_provenance(
    conn: &rusqlite::Connection,
    table: &str,
    subject_ids: &[String],
) -> Result<(), rusqlite::Error> {
    if subject_ids.is_empty() {
        return Ok(());
    }
    if !provenance_sidecar_exists(conn)? {
        return Ok(());
    }
    let model_key = table
        .strip_prefix("vec_")
        .expect("vector table names use the vec_ prefix");
    let placeholders = (2..=subject_ids.len() + 1)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "DELETE FROM vector_provenance \
         WHERE model_key = ?1 AND subject_id IN ({placeholders})"
    );
    let mut statement = conn.prepare(&sql)?;
    statement.raw_bind_parameter(1, model_key)?;
    for (index, subject_id) in subject_ids.iter().enumerate() {
        statement.raw_bind_parameter(index + 2, subject_id.as_str())?;
    }
    statement.raw_execute()?;
    Ok(())
}

/// DML-only multi-chunk subject deletion shared by both the legacy
/// (flag-off) and WriterTask-routed (flag-on) `delete_subjects` paths.
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT`: the caller owns
/// one transaction around the complete input so a failure in any later chunk
/// rolls back every earlier vector deletion and matching ANN-log row.
pub(super) fn delete_vector_subjects_dml(
    conn: &rusqlite::Connection,
    table: &str,
    id_strings: &[String],
) -> Result<u64, rusqlite::Error> {
    let mut total_deleted = 0u64;
    // vec0 only selects its point plan for primary-key equality outside KNN.
    let log_sql = format!(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
         FROM {table} WHERE subject_id = ?1"
    );
    let mut log_stmt = conn.prepare(&log_sql)?;
    let mut delete_stmt = conn.prepare(&format!("DELETE FROM {table} WHERE subject_id = ?1"))?;

    // The relational provenance sidecar still uses bounded IN statements.
    for chunk in id_strings.chunks(400) {
        for id in chunk {
            log_stmt.execute([id.as_str()])?;
            total_deleted += delete_stmt.execute([id.as_str()])? as u64;
        }
        delete_vector_provenance(conn, table, chunk)?;
    }

    Ok(total_deleted)
}

/// Delete `subject_id`'s row from every registered-model vector table, in
/// `namespace` (#546).
///
/// Shared by runtime curation's entity/note merge cleanup, which must sweep
/// the merged-away subject out of every model's `vec_{model_key}` table, not
/// just the primary embedding model's. Callers own the enclosing transaction;
/// this issues no `BEGIN`/`COMMIT`/`SAVEPOINT`.
pub fn delete_subject_from_vector_tables(
    conn: &rusqlite::Connection,
    tables: &[String],
    subject_id: Uuid,
    namespace: &str,
) -> Result<(), rusqlite::Error> {
    let subject_id = subject_id.to_string();
    for table in tables {
        log_vector_deletes(
            conn,
            table,
            "subject_id = ?1 AND namespace = ?2",
            &[&subject_id, &namespace],
        )?;
        let sql = format!("DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2");
        if conn.execute(&sql, rusqlite::params![&subject_id, namespace])? > 0 {
            delete_vector_provenance(conn, table, std::slice::from_ref(&subject_id))?;
        }
    }
    Ok(())
}

/// DML-only batch insert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `insert_batch` paths (ADR-067 Component A).
///
/// Issues no OUTER `BEGIN` / `COMMIT` / `ROLLBACK` — the caller owns the
/// enclosing transaction. The per-record named `SAVEPOINT vec_batch_record`
/// is preserved unchanged: it gives a failed INSERT a no-worse-than-stale
/// rollback (only that record's DELETE is undone) independent of which
/// outer transaction wraps the loop.
#[allow(clippy::too_many_arguments)]
pub(super) fn batch_insert_vectors_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    store_embedding_model: &str,
    records: &[VectorRecord],
    attempted: u64,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<BatchWriteSummary, rusqlite::Error> {
    let mut summary = BatchWriteSummary {
        attempted,
        ..BatchWriteSummary::default()
    };

    for (index, record) in records.iter().enumerate() {
        let item_id = Some(record.subject_id.to_string());
        if record.vectors.len() != 1 {
            summary.record_failure(
                index,
                item_id,
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                format!("expected 1 vector per record, got {}", record.vectors.len()),
            );
            continue;
        }
        let embedding = &record.vectors[0];
        if embedding.len() != dims {
            summary.record_failure(
                index,
                item_id,
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                format!(
                    "wrong vector dimension: expected {dims}, got {}",
                    embedding.len()
                ),
            );
            continue;
        }
        if non_finite_index(embedding).is_some() {
            summary.record_failure(
                index,
                item_id,
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                "embedding contains non-finite values (NaN or Inf)",
            );
            continue;
        }
        let kind_str = record.kind.to_string();

        // Wrap each record's DELETE+INSERT in a savepoint so a failed INSERT
        // rolls back only that record's DELETE, leaving the prior vector intact
        // (no-worse-than-stale guarantee, same as single-record `insert`).
        conn.execute_batch("SAVEPOINT vec_batch_record")?;
        let result = replace_vector_row_dml(
            conn,
            table,
            dims,
            VectorRowRef {
                subject_id: record.subject_id,
                namespace: &record.namespace,
                kind: &kind_str,
                field: &record.field,
                embedding_model: store_embedding_model,
                embedding,
                text_fingerprint: record.text_fingerprint.as_ref(),
                updated_at: Some(&record.updated_at),
            },
            true,
            failpoint_flag.clone(),
        );
        match result {
            Ok(()) => {
                conn.execute_batch("RELEASE SAVEPOINT vec_batch_record")?;
                summary.affected = summary.affected.saturating_add(1);
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT vec_batch_record");
                let _ = conn.execute_batch("RELEASE SAVEPOINT vec_batch_record");
                let (class, retryability) = super::classify_batch_sqlite_error(&e);
                summary.record_failure(index, item_id, class, retryability, e.to_string());
            }
        }
    }

    Ok(summary)
}

/// Shared DELETE-then-INSERT DML for single-record `insert`/`update`, run
/// inside a named `SAVEPOINT` (nestable inside the WriterTask's own
/// transaction) instead of `conn.unchecked_transaction()` (which would
/// attempt a nested `BEGIN` and fail once this runs inside the WriterTask's
/// already-open transaction). A failed INSERT rolls back only this
/// SAVEPOINT, leaving the previous vector intact (no-worse-than-stale
/// guarantee) — the single-record analog of `batch_insert_vectors_dml`'s
/// per-record `SAVEPOINT vec_batch_record`.
#[allow(clippy::too_many_arguments)]
pub(super) fn vec_upsert_atomic_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    subject_id: Uuid,
    kind_str: &str,
    namespace: &str,
    field: &str,
    embedding_model: &str,
    embedding: &[f32],
    savepoint_name: &'static str,
    record_ann_delta: bool,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), rusqlite::Error> {
    conn.execute_batch(&format!("SAVEPOINT {savepoint_name}"))?;
    let result = replace_vector_row_dml(
        conn,
        table,
        dims,
        VectorRowRef {
            subject_id,
            namespace,
            kind: kind_str,
            field,
            embedding_model,
            embedding,
            text_fingerprint: None,
            updated_at: None,
        },
        record_ann_delta,
        failpoint_flag,
    );

    match result {
        Ok(()) => {
            conn.execute_batch(&format!("RELEASE SAVEPOINT {savepoint_name}"))?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {savepoint_name}"));
            let _ = conn.execute_batch(&format!("RELEASE SAVEPOINT {savepoint_name}"));
            Err(e)
        }
    }
}

/// DML-only orphan-sweep body shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `orphan_sweep` paths (ADR-067 Amendment 1).
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` — the caller owns the enclosing
/// transaction (either the flag-off path's `Transaction::new_unchecked`, or
/// the WriterTask drain loop's own `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`
/// wrap). `ns_json` / `kind_json` / `allow_json` are the pre-serialized JSON
/// filter arguments (or `None` for "no filter") computed once by the caller.
pub(super) fn orphan_sweep_dml(
    conn: &rusqlite::Connection,
    table: &str,
    ns_json: Option<&str>,
    kind_json: Option<&str>,
    allow_json: Option<&str>,
    max_delete: i64,
    dry_run: bool,
) -> Result<OrphanSweepResult, rusqlite::Error> {
    // Optional-filter clause shared across all three queries.
    // Each ?N appears twice (IS NULL guard + json_each call); SQLite
    // reuses the same bound value for every occurrence of the same ?N.
    //   ?1 = namespace JSON or NULL   ?2 = kind JSON or NULL
    //   ?3 = allowlist JSON or NULL
    let filter_pred = "(?1 IS NULL OR namespace IN (SELECT value FROM json_each(?1))) \
                       AND (?2 IS NULL OR kind IN (SELECT value FROM json_each(?2))) \
                       AND (?3 IS NULL OR subject_id IN (SELECT value FROM json_each(?3)))";

    // Live-subjects subquery used in the orphan anti-join.
    //
    // Policy-critical: only live subjects protect their vectors. Knowledge
    // atoms have their own core table but use Entity-kind vectors in this
    // same store. The table is required by the core schema: a missing table
    // must fail the sweep before deletion rather than treat atoms as absent.
    // Memory records live in `notes` with kind = 'memory'.
    let live_subq = "SELECT id FROM entities WHERE deleted_at IS NULL \
                     UNION ALL \
                     SELECT id FROM notes WHERE deleted_at IS NULL \
                     UNION ALL \
                     SELECT id FROM knowledge_atoms WHERE deleted_at IS NULL";

    // 1. Scanned: rows matching the caller's filters (before orphan check).
    let scan_sql = format!(
        "SELECT COUNT(*) FROM {t} WHERE {f}",
        t = table,
        f = filter_pred
    );
    let scanned: i64 = conn.query_row(
        &scan_sql,
        rusqlite::params![ns_json, kind_json, allow_json],
        |row| row.get(0),
    )?;

    // Snapshot the live registry once for this sweep. Each bounded delete
    // batch consults this temp table instead of rescanning all three source
    // tables. The caller's transaction owns the snapshot and any rollback.
    conn.execute_batch("CREATE TEMP TABLE khive_orphan_sweep_live_ids(id TEXT)")?;
    conn.execute_batch(&format!(
        "INSERT INTO temp.khive_orphan_sweep_live_ids(id) {live_subq}"
    ))?;
    conn.execute_batch(
        "CREATE INDEX temp.khive_orphan_sweep_live_ids_idx \
         ON khive_orphan_sweep_live_ids(id)",
    )?;
    let orphan_pred = format!(
        "subject_id NOT IN (SELECT id FROM temp.khive_orphan_sweep_live_ids) AND {filter_pred}"
    );

    // 2. Would-delete: orphaned rows among the scanned set.
    let count_sql = format!(
        "SELECT COUNT(*) FROM {t} WHERE {p}",
        t = table,
        p = orphan_pred,
    );
    let would_delete: i64 = conn.query_row(
        &count_sql,
        rusqlite::params![ns_json, kind_json, allow_json],
        |row| row.get(0),
    )?;

    let max_delete_hit = would_delete > max_delete;

    // 3. Delete — skipped in dry-run mode.
    //
    // `DELETE … LIMIT N` requires SQLITE_ENABLE_UPDATE_DELETE_LIMIT, which
    // rusqlite's bundled SQLite does not enable.  Portable alternative:
    // delete subject_ids returned by a capped SELECT subquery.  SQLite
    // materialises the inner SELECT before running the outer DELETE, so there
    // is no self-referential conflict.
    // Select one bounded batch at a time. The same explicit IDs feed its log
    // insert and delete: evaluating a `LIMIT` subquery twice would not
    // guarantee that the logged and deleted rows are identical. The enclosing
    // writer transaction keeps later batches from racing another writer.
    let deleted: i64 = if dry_run {
        0
    } else {
        let select_sql = format!(
            "SELECT subject_id FROM {t} WHERE {p} LIMIT ?4",
            t = table,
            p = orphan_pred,
        );
        let mut select_stmt = conn.prepare(&select_sql)?;
        // vec0 only selects its point plan for primary-key equality outside KNN,
        // so the log insert and the delete each run once per victim id.
        let log_sql = format!(
            "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
             SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
             FROM {table} WHERE subject_id = ?1"
        );
        let mut log_stmt = conn.prepare(&log_sql)?;
        let del_sql = format!("DELETE FROM {table} WHERE subject_id = ?1");
        let mut delete_stmt = conn.prepare(&del_sql)?;
        let mut total: i64 = 0;
        let mut remaining = max_delete;
        while remaining > 0 {
            let batch_limit = remaining.min(400);
            let victim_ids: Vec<String> = select_stmt
                .query_map(
                    rusqlite::params![ns_json, kind_json, allow_json, batch_limit],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<_, _>>()?;
            if victim_ids.is_empty() {
                break;
            }

            for id in &victim_ids {
                log_stmt.execute([id.as_str()])?;
                total += delete_stmt.execute([id.as_str()])? as i64;
            }
            // The relational provenance sidecar keeps its bounded IN statement.
            delete_vector_provenance(conn, table, &victim_ids)?;
            remaining -= victim_ids.len() as i64;
        }
        total
    };

    conn.execute_batch("DROP TABLE temp.khive_orphan_sweep_live_ids")?;

    Ok(OrphanSweepResult {
        scanned: scanned as u64,
        would_delete: would_delete as u64,
        deleted: deleted as u64,
        max_delete_hit,
    })
}
