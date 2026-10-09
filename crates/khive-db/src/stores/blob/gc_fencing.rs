//! Attachment GC admission, fence probes, evidence validation and bounded claims.

use khive_storage::blob::ContentRef;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue, StorageResult};
#[cfg(doc)]
use khive_storage::BlobStore;
use khive_storage::{AtomicUnitOp, SqlAccess, StorageCapability, StorageError};
use uuid::Uuid;

use super::BLOB_GC_CLAIM_BATCH_SIZE;

#[derive(Debug)]
pub(super) struct BlobGcBatchRows {
    pub(super) grace_period_skipped: u64,
    pub(super) would_delete: u64,
    pub(super) claimed_rows: Vec<SqlRow>,
}

fn required_nonnegative_count(
    value: Option<SqlValue>,
    operation: &'static str,
) -> StorageResult<u64> {
    match value {
        Some(SqlValue::Integer(value)) if value >= 0 => Ok(value as u64),
        other => Err(StorageError::Internal(format!(
            "{operation} returned an invalid count: {other:?}"
        ))),
    }
}

fn invalid_content_ref(message: String) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Blob,
        operation: "transactional_orphan_sweep".into(),
        message,
    }
}

/// Whether this database carries the complete V21 attachment-only GC fencing
/// set and durable completed cutover marker.
///
/// `transactional_orphan_sweep` is reachable from any `SqlAccess` a caller
/// hands it, including a `StorageBackend` constructed directly (e.g.
/// `StorageBackend::memory()`/`sqlite()` used without `prepare_core_schema`)
/// that never ran core migrations. The triggers are the fence that keeps a
/// concurrent attachment write from resurrecting a claimed digest in the
/// released-writer window, so a database missing any element of the set
/// cannot satisfy the fail-closed guarantee the
/// [`BlobStore::transactional_orphan_sweep`] contract requires; the sweep
/// refuses with [`StorageError::Unsupported`] rather than degrading to
/// unfenced deletion.
pub(super) async fn blob_gc_fencing_complete(sql: &dyn SqlAccess) -> StorageResult<bool> {
    let mut reader = sql.reader().await?;
    let present = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM sqlite_master \
                      WHERE (type = 'table' AND name IN ( \
                                 'blob_gc_claims', 'attachments', \
                                 'attachment_cutover_state')) \
                         OR (type = 'index' AND name IN ( \
                             'idx_blob_gc_claims_content_ref', \
                             'idx_attachments_content_ref')) \
                         OR (type = 'trigger' AND name IN ( \
                             'attachments_reject_claimed_blob_insert', \
                             'attachments_reject_claimed_blob_update'))"
                    .to_string(),
                params: vec![],
                label: Some("blob_gc_fencing_complete".to_string()),
            })
            .await?,
        "blob_gc_fencing_complete",
    )?;
    if present != 7 {
        return Ok(false);
    }

    let legacy_objects = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT \
                        (SELECT COUNT(*) FROM pragma_table_info('entities') \
                         WHERE name = 'content_ref') \
                      + (SELECT COUNT(*) FROM sqlite_master \
                         WHERE (type = 'index' AND name = 'idx_entities_content_ref') \
                            OR (type = 'trigger' AND name IN ( \
                                'entities_reject_claimed_blob_insert', \
                                'entities_reject_claimed_blob_update')))"
                    .to_string(),
                params: vec![],
                label: Some("blob_gc_legacy_fencing_absent".to_string()),
            })
            .await?,
        "blob_gc_legacy_fencing_absent",
    )?;
    if legacy_objects != 0 {
        return Ok(false);
    }

    // The sweep is admitted only for the EXACT completed V21 epoch
    // (ADR-160 Phase 4a: "report-only and destructive sweeps only for an
    // exact completed V21 epoch … ahead-of-V21 epochs return typed
    // Unsupported"). A ledger above V21 — whether ahead of this binary or a
    // migration this same binary applied — belongs to a schema epoch whose
    // attachment/liveness semantics this gate never validated, so it fails
    // closed; the author of a future migration extends the gate in the same
    // change that proves the new epoch's liveness set, never by default.
    // General schema validation (`attachment_cutover_status`) deliberately
    // keeps accepting later versions on top of a completed cutover: that is
    // the normal serving course, and the exact-epoch rule is scoped to
    // destructive GC admission.
    //
    // "Exact completed V21 epoch" means the WHOLE canonical ledger, not a
    // V21 terminal row: `version` is the table's PRIMARY KEY, so
    // COUNT(*) = 21 ∧ MIN = 1 ∧ MAX = 21 holds if and only if the ledger is
    // exactly the contiguous set {1..21}. A ledger that merely retains a V21
    // row at MAX(version) = 21 while earlier rows are missing is an
    // incomplete migration history whose physical schema this gate never
    // validated — it must fail closed, same as ahead-of-V21. Name-level
    // canonicality of the below-terminal rows stays boot's job
    // (`validate_applied_migration_ledger`); this predicate enforces the
    // structural contiguity a destructive sweep's admission rests on, plus
    // the named V21 row itself.
    let complete = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachment_cutover_state AS cutover \
                      WHERE cutover.singleton = 1 \
                        AND cutover.state = 'complete' \
                        AND cutover.completed_at IS NOT NULL \
                        AND (SELECT COUNT(*) FROM _schema_migrations \
                             WHERE version = ?1 \
                               AND name = 'attachments_first_class') = 1 \
                        AND (SELECT COUNT(*) FROM _schema_migrations) = ?1 \
                        AND (SELECT MIN(version) FROM _schema_migrations) = 1 \
                        AND (SELECT MAX(version) FROM _schema_migrations) = ?1"
                    .to_string(),
                params: vec![SqlValue::Integer(i64::from(
                    crate::migrations::ATTACHMENT_CUTOVER_VERSION,
                ))],
                label: Some("blob_gc_cutover_complete".to_string()),
            })
            .await?,
        "blob_gc_cutover_complete",
    )?;
    Ok(complete == 1)
}

pub(super) fn unsupported_blob_gc_epoch() -> StorageError {
    StorageError::Unsupported {
        capability: StorageCapability::Blob,
        operation: "transactional_orphan_sweep".into(),
        message: "transactional blob GC requires a complete V21 attachment cutover with \
                  the attachment claim-fencing set; refusing both report-only and \
                  destructive sweep in this database epoch"
            .into(),
    }
}

/// The sentinel digest the fence probe claims. All zeros is canonical-form
/// valid (64 lowercase hex) and unreachable as a real BLAKE3 digest for any
/// stored object in practice; probe rows never survive the probe transaction.
const BLOB_GC_FENCE_PROBE_REF: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The RAISE(ABORT) message shared by the V20 and V21 fencing triggers. The probe
/// requires the rejection to be OUR fence, not an incidental failure.
const BLOB_GC_FENCE_TRIGGER_MESSAGE: &str = "content_ref is reserved by an active blob sweep";

/// Prove the V21 fence actually fences, not merely that objects with the
/// right NAMES exist in `sqlite_master`. Same-named no-op triggers (or a
/// rewritten trigger body) would pass the name census while letting a
/// claimed `content_ref` become live in the released-writer window, so the
/// gate exercises the fence: inside one writer transaction it claims the
/// all-zero sentinel AND a second random digest, and attempts the attachment
/// INSERT and attachment UPDATE the triggers must reject for EACH claimed
/// digest — with the second digest's arms using a different attachment shape
/// (substrate `note`, role `evidence`), so a trigger rewrite restricted to
/// one digest, substrate, or role fails an arm instead of passing a
/// fixed-sentinel census. Every arm must fail with the triggers' own RAISE
/// message, and every probe row is deleted before the unit commits. Any
/// other outcome — a write accepted, or rejected for a different reason —
/// refuses the sweep with [`StorageError::Unsupported`].
pub(super) async fn blob_gc_fence_probe(sql: &dyn SqlAccess) -> StorageResult<()> {
    let run = Uuid::new_v4().simple().to_string();
    blob_gc_fence_probe_with_ids(
        sql,
        format!("__blob-gc-fence-probe-insert-{run}__"),
        format!("__blob-gc-fence-probe-update-{run}__"),
        format!("__blob-gc-fence-probe-insert2-{run}__"),
        format!("__blob-gc-fence-probe-update2-{run}__"),
        format!("__fence_probe-{run}__"),
    )
    .await
}

/// Probe body with explicit row ids so tests can force an id collision.
/// Production callers go through [`blob_gc_fence_probe`], which mints
/// per-run random ids; the guard below still refuses to run — touching
/// nothing — if any minted id already names a row.
pub(super) async fn blob_gc_fence_probe_with_ids(
    sql: &dyn SqlAccess,
    insert_id: String,
    update_id: String,
    insert2_id: String,
    update2_id: String,
    claim_key: String,
) -> StorageResult<()> {
    fn fence_rejection(result: Result<u64, StorageError>) -> Result<bool, String> {
        match result {
            Ok(_) => Ok(false),
            Err(error) => {
                let text = error.to_string();
                if text.contains(BLOB_GC_FENCE_TRIGGER_MESSAGE) {
                    Ok(true)
                } else {
                    Err(text)
                }
            }
        }
    }

    fn required_seed(value: Option<SqlValue>) -> StorageResult<String> {
        match value {
            Some(SqlValue::Text(seed)) => Ok(seed),
            _ => Err(StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation: "transactional_orphan_sweep".into(),
                message: "the blob GC fence probe could not select an unclaimed \
                          canonical seed; refusing deletion so a later sweep can retry"
                    .into(),
            }),
        }
    }

    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            // Ownership guard: the cleanup below deletes these ids
            // unconditionally, so the probe may only proceed when it can
            // prove every id is unclaimed in EVERY table cleanup touches.
            let preexisting = writer
                .query_row(SqlStatement {
                    sql: "SELECT (SELECT COUNT(*) FROM attachments \
                                   WHERE record_uuid IN (?1, ?2, ?3, ?4)) \
                              + (SELECT COUNT(*) FROM blob_gc_claims WHERE root_key = ?5)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert_id.clone()),
                        SqlValue::Text(update_id.clone()),
                        SqlValue::Text(insert2_id.clone()),
                        SqlValue::Text(update2_id.clone()),
                        SqlValue::Text(claim_key.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_ownership_guard".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|c| c.value.clone()));
            match preexisting {
                Some(SqlValue::Integer(0)) => {}
                Some(SqlValue::Integer(_)) => {
                    return Err(StorageError::Unsupported {
                        capability: StorageCapability::Blob,
                        operation: "transactional_orphan_sweep".into(),
                        message: "the blob GC fence probe's row ids collide with existing \
                                  rows; refusing to probe rather than delete data the \
                                  probe does not own"
                            .into(),
                    });
                }
                _ => {
                    return Err(StorageError::Internal(
                        "blob GC fence probe ownership guard returned no count".into(),
                    ));
                }
            }

            // The UPDATE arm needs a valid, initially unclaimed reference.
            // Select it under this same writer transaction instead of using a
            // fixed sentinel that a recoverable abandoned claim could fence
            // forever. Eight fresh candidates keep collision handling bounded;
            // no candidate means a safe, retryable refusal.
            let seed_ref = writer
                .query_row(SqlStatement {
                    sql: "WITH RECURSIVE candidates(attempt, content_ref) AS ( \
                              SELECT 1, lower(hex(randomblob(32))) \
                              UNION ALL \
                              SELECT attempt + 1, lower(hex(randomblob(32))) \
                              FROM candidates WHERE attempt < 8 \
                          ) \
                          SELECT candidate.content_ref FROM candidates AS candidate \
                          WHERE candidate.content_ref <> ?1 \
                            AND NOT EXISTS ( \
                                SELECT 1 FROM blob_gc_claims \
                                WHERE content_ref = candidate.content_ref \
                            ) \
                          LIMIT 1"
                        .to_string(),
                    params: vec![SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string())],
                    label: Some("blob_gc_fence_probe_select_seed".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|column| column.value.clone()));
            let seed_ref = required_seed(seed_ref)?;

            let seed2_ref = writer
                .query_row(SqlStatement {
                    sql: "WITH RECURSIVE candidates(attempt, content_ref) AS ( \
                              SELECT 1, lower(hex(randomblob(32))) \
                              UNION ALL \
                              SELECT attempt + 1, lower(hex(randomblob(32))) \
                              FROM candidates WHERE attempt < 8 \
                          ) \
                          SELECT candidate.content_ref FROM candidates AS candidate \
                          WHERE candidate.content_ref NOT IN (?1, ?2) \
                            AND NOT EXISTS ( \
                                SELECT 1 FROM blob_gc_claims \
                                WHERE content_ref = candidate.content_ref \
                            ) \
                          LIMIT 1"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(seed_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_select_seed2".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|column| column.value.clone()));
            let seed2_ref = required_seed(seed2_ref)?;

            // The second CLAIMED digest. A trigger rewrite conditioned on the
            // fixed all-zero sentinel passes that sentinel's arms; this digest
            // is random per run, so such a rewrite fails the arms below.
            let probe2_ref = writer
                .query_row(SqlStatement {
                    sql: "WITH RECURSIVE candidates(attempt, content_ref) AS ( \
                              SELECT 1, lower(hex(randomblob(32))) \
                              UNION ALL \
                              SELECT attempt + 1, lower(hex(randomblob(32))) \
                              FROM candidates WHERE attempt < 8 \
                          ) \
                          SELECT candidate.content_ref FROM candidates AS candidate \
                          WHERE candidate.content_ref NOT IN (?1, ?2, ?3) \
                            AND NOT EXISTS ( \
                                SELECT 1 FROM blob_gc_claims \
                                WHERE content_ref = candidate.content_ref \
                            ) \
                          LIMIT 1"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(seed_ref.clone()),
                        SqlValue::Text(seed2_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_select_probe2".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|column| column.value.clone()));
            let probe2_ref = required_seed(probe2_ref)?;

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                          VALUES (?1, ?2, 0), (?1, ?3, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(claim_key.clone()),
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(probe2_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_claim".to_string()),
                })
                .await?;

            let insert_attempt = writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'entity', 'content', ?2, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert_id.clone()),
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                    ],
                    label: Some("blob_gc_fence_probe_insert_arm".to_string()),
                })
                .await;
            let insert_fenced = fence_rejection(insert_attempt);

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'entity', 'content', ?2, 0)"
                        .to_string(),
                    params: vec![SqlValue::Text(update_id.clone()), SqlValue::Text(seed_ref)],
                    label: Some("blob_gc_fence_probe_update_arm_seed".to_string()),
                })
                .await?;
            let update_attempt = writer
                .execute(SqlStatement {
                    sql: "UPDATE attachments SET content_ref = ?1 \
                          WHERE record_uuid = ?2 AND role = 'content'"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(update_id.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_update_arm".to_string()),
                })
                .await;
            let update_fenced = fence_rejection(update_attempt);

            let insert2_attempt = writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'note', 'evidence', ?2, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert2_id.clone()),
                        SqlValue::Text(probe2_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_insert2_arm".to_string()),
                })
                .await;
            let insert2_fenced = fence_rejection(insert2_attempt);

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'note', 'evidence', ?2, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(update2_id.clone()),
                        SqlValue::Text(seed2_ref),
                    ],
                    label: Some("blob_gc_fence_probe_update2_arm_seed".to_string()),
                })
                .await?;
            let update2_attempt = writer
                .execute(SqlStatement {
                    sql: "UPDATE attachments SET content_ref = ?1 \
                          WHERE record_uuid = ?2 AND role = 'evidence'"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(probe2_ref),
                        SqlValue::Text(update2_id.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_update2_arm".to_string()),
                })
                .await;
            let update2_fenced = fence_rejection(update2_attempt);

            // Remove every probe row before this unit commits, including an
            // attachment row a dead fence let through.
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM attachments WHERE record_uuid IN (?1, ?2, ?3, ?4)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert_id.clone()),
                        SqlValue::Text(update_id.clone()),
                        SqlValue::Text(insert2_id.clone()),
                        SqlValue::Text(update2_id.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_cleanup_attachments".to_string()),
                })
                .await?;
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM blob_gc_claims WHERE root_key = ?1".to_string(),
                    params: vec![SqlValue::Text(claim_key)],
                    label: Some("blob_gc_fence_probe_cleanup_claim".to_string()),
                })
                .await?;

            Ok(
                Box::new((insert_fenced, update_fenced, insert2_fenced, update2_fenced))
                    as Box<dyn std::any::Any + Send>,
            )
        })
    });
    let outcome = sql.atomic_unit(op).await?;
    let (insert_fenced, update_fenced, insert2_fenced, update2_fenced) = *outcome
        .downcast::<(
            Result<bool, String>,
            Result<bool, String>,
            Result<bool, String>,
            Result<bool, String>,
        )>()
        .map_err(|_| {
            StorageError::Internal("blob GC fence probe returned an unexpected outcome type".into())
        })?;
    let arm_verdict = |arm: &str, fenced: Result<bool, String>| -> StorageResult<()> {
        match fenced {
            Ok(true) => Ok(()),
            Ok(false) => Err(StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation: "transactional_orphan_sweep".into(),
                message: format!(
                    "the V21 fencing triggers exist by name but did not reject a claimed \
                     content_ref on the attachment {arm} path; refusing unfenced deletion"
                ),
            }),
            Err(other) => Err(StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation: "transactional_orphan_sweep".into(),
                message: format!(
                    "the blob GC fence probe could not verify the attachment {arm} fence \
                     (unexpected rejection: {other}); refusing unfenced deletion"
                ),
            }),
        }
    };
    arm_verdict("INSERT", insert_fenced)?;
    arm_verdict("UPDATE", update_fenced)?;
    arm_verdict("second-digest INSERT", insert2_fenced)?;
    arm_verdict("second-digest UPDATE", update2_fenced)
}

pub(super) async fn validate_blob_gc_evidence(sql: &dyn SqlAccess) -> StorageResult<()> {
    // These full-table integrity probes are statement-scoped reads. Keep them
    // off the single writer; only their one-row result is materialized. The
    // database sweep owner excludes another claim producer, and each bounded
    // claim unit anti-joins the then-current live rows under its writer lock.
    let mut reader = sql.reader().await?;
    // length() and GLOB both stop at an embedded NUL, so a value of 64 hex
    // characters followed by a NUL and arbitrary bytes passes them while
    // failing the exact-equality liveness anti-join. The byte-length arm
    // closes that class: chars = 64 AND bytes = 64 * the encoding's bytes
    // per ASCII character forces a NUL-free canonical value. CAST(TEXT AS
    // BLOB) yields the database text encoding's bytes (1 per hex char in
    // UTF-8, 2 in UTF-16), so the width is derived from the same database
    // rather than assumed, and an unrecognizable answer fails closed.
    let canonical_bytes = match reader
        .query_row(SqlStatement {
            sql: "SELECT length(CAST('x' AS BLOB))".to_string(),
            params: vec![],
            label: Some("blob_gc_validate_encoding_width".to_string()),
        })
        .await?
        .and_then(|row| row.columns.first().map(|column| column.value.clone()))
    {
        Some(SqlValue::Integer(width)) if (1..=4).contains(&width) => width * 64,
        other => {
            return Err(invalid_content_ref(format!(
                "the text-encoding width probe returned {other:?}; refusing GC validation"
            )));
        }
    };
    let invalid_claim = reader
        .query_row(SqlStatement {
            sql: "SELECT content_ref FROM blob_gc_claims \
                  WHERE typeof(content_ref) <> 'text' \
                     OR length(content_ref) <> 64 \
                     OR length(CAST(content_ref AS BLOB)) <> ?1 \
                     OR content_ref GLOB '*[^0-9a-f]*' \
                  LIMIT 1"
                .to_string(),
            params: vec![SqlValue::Integer(canonical_bytes)],
            label: Some("blob_gc_validate_existing_claims".to_string()),
        })
        .await?;
    if invalid_claim.is_some() {
        return Err(invalid_content_ref(
            "blob_gc_claims.content_ref contained a non-canonical value".into(),
        ));
    }

    let invalid_live = reader
        .query_row(SqlStatement {
            sql: "SELECT content_ref FROM attachments \
                  WHERE typeof(content_ref) <> 'text' \
                      OR length(content_ref) <> 64 \
                      OR length(CAST(content_ref AS BLOB)) <> ?1 \
                      OR content_ref GLOB '*[^0-9a-f]*' \
                  LIMIT 1"
                .to_string(),
            params: vec![SqlValue::Integer(canonical_bytes)],
            label: Some("blob_gc_validate_live_refs".to_string()),
        })
        .await?;
    if invalid_live.is_some() {
        return Err(invalid_content_ref(
            "attachments.content_ref contained a non-canonical value".into(),
        ));
    }
    Ok(())
}

pub(super) async fn release_abandoned_blob_gc_claim_batch(
    sql: &dyn SqlAccess,
) -> StorageResult<u64> {
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let released = writer
                .execute(SqlStatement {
                    sql: "DELETE FROM blob_gc_claims \
                          WHERE rowid IN ( \
                            SELECT rowid FROM blob_gc_claims \
                            ORDER BY rowid LIMIT ?1 \
                          )"
                    .to_string(),
                    params: vec![SqlValue::Integer(BLOB_GC_CLAIM_BATCH_SIZE as i64)],
                    label: Some("blob_gc_release_abandoned_claim_batch".to_string()),
                })
                .await?;
            Ok(Box::new(released) as Box<dyn std::any::Any + Send>)
        })
    });
    let released = sql.atomic_unit(op).await?;
    released.downcast::<u64>().map(|count| *count).map_err(|_| {
        StorageError::Internal(
            "transactional orphan sweep returned an unexpected recovery count type".into(),
        )
    })
}

/// Candidate ownership for every GC accounting and claim site.
///
/// The exact-V21 admission gate remains unchanged. Its legacy schema has no
/// quarantine table, so callers select the canonical-only fragment when that
/// table is absent rather than preparing a reference to a missing table.
pub(super) fn blob_gc_unowned_attachment_predicate(quarantine_present: bool) -> &'static str {
    if quarantine_present {
        "NOT EXISTS ( \
           SELECT 1 FROM attachments \
           WHERE content_ref = candidate.value \
         ) AND NOT EXISTS ( \
           SELECT 1 FROM attachment_quarantine \
           WHERE content_ref = candidate.value \
         )"
    } else {
        "NOT EXISTS ( \
           SELECT 1 FROM attachments \
           WHERE content_ref = candidate.value \
         )"
    }
}

pub(super) async fn claim_blob_gc_batch(
    sql: &dyn SqlAccess,
    root_key: String,
    candidates: &[(ContentRef, bool)],
    dry_run: bool,
) -> StorageResult<BlobGcBatchRows> {
    debug_assert!(candidates.len() <= BLOB_GC_CLAIM_BATCH_SIZE);
    let eligible_refs = candidates
        .iter()
        .filter(|(_, within_grace)| !within_grace)
        .map(|(content_ref, _)| content_ref.to_string())
        .collect::<Vec<_>>();
    let grace_refs = candidates
        .iter()
        .filter(|(_, within_grace)| *within_grace)
        .map(|(content_ref, _)| content_ref.to_string())
        .collect::<Vec<_>>();
    let eligible_json = serde_json::to_string(&eligible_refs).map_err(|error| {
        StorageError::Internal(format!(
            "failed to prepare blob GC eligible candidate batch: {error}"
        ))
    })?;
    let grace_json = serde_json::to_string(&grace_refs).map_err(|error| {
        StorageError::Internal(format!(
            "failed to prepare blob GC grace candidate batch: {error}"
        ))
    })?;
    let claimed_at = chrono::Utc::now().timestamp_micros();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let quarantine_present = required_nonnegative_count(
                writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM sqlite_master \
                              WHERE type = 'table' AND name = 'attachment_quarantine'"
                            .to_string(),
                        params: vec![],
                        label: Some("blob_gc_quarantine_table_present".to_string()),
                    })
                    .await?,
                "blob_gc_quarantine_table_present",
            )?;
            let ownership_predicate = match quarantine_present {
                0 => blob_gc_unowned_attachment_predicate(false),
                1 => blob_gc_unowned_attachment_predicate(true),
                _ => {
                    return Err(StorageError::Internal(
                        "blob GC quarantine table presence returned an invalid count".into(),
                    ));
                }
            };
            let grace_period_skipped = required_nonnegative_count(
                writer
                    .query_scalar(SqlStatement {
                        sql: format!(
                            "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                             WHERE {ownership_predicate}"
                        ),
                        params: vec![SqlValue::Text(grace_json)],
                        label: Some("blob_gc_count_grace_candidates_batch".to_string()),
                    })
                    .await?,
                "blob_gc_count_grace_candidates_batch",
            )?;

            if dry_run {
                let would_delete = required_nonnegative_count(
                    writer
                        .query_scalar(SqlStatement {
                            sql: format!(
                                "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                                 WHERE {ownership_predicate}"
                            ),
                            params: vec![SqlValue::Text(eligible_json)],
                            label: Some("blob_gc_count_dry_run_candidates_batch".to_string()),
                        })
                        .await?,
                    "blob_gc_count_dry_run_candidates_batch",
                )?;
                return Ok(Box::new(BlobGcBatchRows {
                    grace_period_skipped,
                    would_delete,
                    claimed_rows: Vec::new(),
                }) as Box<dyn std::any::Any + Send>);
            }

            writer
                .execute(SqlStatement {
                    sql: format!(
                        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                         SELECT ?1, candidate.value, ?3 \
                         FROM json_each(?2) AS candidate \
                         WHERE {ownership_predicate}"
                    ),
                    params: vec![
                        SqlValue::Text(root_key.clone()),
                        SqlValue::Text(eligible_json),
                        SqlValue::Integer(claimed_at),
                    ],
                    label: Some("blob_gc_claim_candidate_batch".to_string()),
                })
                .await?;

            let claimed_rows = writer
                .query_all(SqlStatement {
                    sql: "SELECT content_ref FROM blob_gc_claims \
                          WHERE root_key = ?1 ORDER BY content_ref"
                        .to_string(),
                    params: vec![SqlValue::Text(root_key)],
                    label: Some("blob_gc_claimed_candidate_batch".to_string()),
                })
                .await?;
            Ok(Box::new(BlobGcBatchRows {
                grace_period_skipped,
                would_delete: claimed_rows.len() as u64,
                claimed_rows,
            }) as Box<dyn std::any::Any + Send>)
        })
    });
    let rows = sql.atomic_unit(op).await?;
    rows.downcast::<BlobGcBatchRows>()
        .map(|rows| *rows)
        .map_err(|_| {
            StorageError::Internal(
                "transactional orphan sweep returned an unexpected batch-row type".into(),
            )
        })
}

pub(super) fn parse_blob_gc_claim_rows(rows: Vec<SqlRow>) -> StorageResult<Vec<ContentRef>> {
    let mut claimed = Vec::with_capacity(rows.len());
    for row in rows {
        let raw = match row.get("content_ref") {
            Some(SqlValue::Text(raw)) => raw.clone(),
            _ => {
                return Err(invalid_content_ref(
                    "blob_gc_claims.content_ref contained a non-text value".into(),
                ));
            }
        };
        claimed.push(ContentRef::from_hex(raw).map_err(invalid_content_ref)?);
    }
    Ok(claimed)
}

pub(super) async fn release_blob_gc_batch(
    sql: &dyn SqlAccess,
    root_key: String,
) -> StorageResult<()> {
    let cleanup: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM blob_gc_claims WHERE root_key = ?1".to_string(),
                    params: vec![SqlValue::Text(root_key)],
                    label: Some("blob_gc_release_claim_batch".to_string()),
                })
                .await?;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });
    sql.atomic_unit(cleanup).await?;
    Ok(())
}
