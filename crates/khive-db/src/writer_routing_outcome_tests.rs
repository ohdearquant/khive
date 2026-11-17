use super::*;
use crate::backend::{StorageBackend, StoreSchemaGate};
use crate::pool::PoolConfig;
use crate::test_hooks::TestHooks;
use khive_storage::{SqlAccess, StorageCapability};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::sync::atomic::AtomicUsize;

fn seeded_pool(path: &Path, strict: bool, enabled: bool) -> Arc<ConnectionPool> {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE outcome_probe(id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
        .unwrap();
    drop(conn);
    Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.to_path_buf()),
            write_queue_enabled: Some(enabled),
            write_routing_strict: strict,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    )
}

fn raw_write_refuses(pool: &ConnectionPool) {
    let called = AtomicBool::new(false);
    let result = pool.writer().unwrap().transaction(|_| {
        called.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert!(
        matches!(result, Err(SqliteError::WriterRouteViolation { .. })),
        "a lifecycle owner must not authorize a raw request write: {result:?}"
    );
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn committed_reply_loss_has_real_row_and_keeps_writer_live() {
    for strict in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcome.db");
        let pool = seeded_pool(&path, strict, true);
        let writer = pool.writer_task_handle().unwrap().unwrap();
        let probe = writer
            .send_top_level(|conn| {
                Ok(conn.test_suppress_commit_reply_after_update("outcome_probe", 2))
            })
            .await
            .unwrap();
        writer
            .send(|conn| {
                conn.execute("INSERT INTO outcome_probe VALUES(1,'unrelated')", [])
                    .map_err(|error| {
                        StorageError::driver(StorageCapability::Sql, "fixture", error)
                    })?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            probe.snapshot().matched_requests,
            0,
            "an unrelated request cannot consume the fault arm"
        );
        let result = writer
            .send(|conn| {
                conn.execute("INSERT INTO outcome_probe VALUES(2,'committed')", [])
                    .map_err(|error| {
                        StorageError::driver(StorageCapability::Sql, "fixture", error)
                    })?;
                Ok(())
            })
            .await;
        assert!(
            matches!(result, Err(StorageError::WriteOutcomeUnknown { .. })),
            "issued COMMIT with lost successful reply must keep its own typed outcome: {result:?}"
        );
        let snapshot = probe.snapshot();
        assert_eq!(
            (snapshot.matched_requests, snapshot.reply_suppressed),
            (1, 1)
        );
        assert!(snapshot.commit_issued && snapshot.commit_succeeded && snapshot.autocommit);
        let independent = Connection::open(&path).unwrap();
        let value: String = independent
            .query_row("SELECT value FROM outcome_probe WHERE id=2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            value, "committed",
            "the missing reply must follow a real durable COMMIT"
        );
        let healthy = writer.send(|conn| Ok(conn.is_autocommit())).await.unwrap();
        assert!(
            healthy,
            "loss of one reply must not imply writer termination"
        );
    }
}

#[tokio::test]
async fn failed_commit_keeps_native_error_and_is_never_suppressed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failed.db");
    let pool = seeded_pool(&path, true, true);
    let writer = pool.writer_task_handle().unwrap().unwrap();
    let probe = writer
        .send_top_level(|conn| {
            let probe = conn.test_suppress_commit_reply_after_update("outcome_probe", 1);
            conn.test_authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Unknown,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .unwrap();
            Ok(probe)
        })
        .await
        .unwrap();
    let result = writer
        .send(|conn| {
            conn.execute("INSERT INTO outcome_probe VALUES(1,'rolled back')", [])
                .map_err(|error| StorageError::driver(StorageCapability::Sql, "fixture", error))?;
            Ok(())
        })
        .await;
    let error = result.expect_err("real denied COMMIT must refuse");
    assert!(
        !error.is_write_outcome_unknown(),
        "native COMMIT refusal is stronger than delivery uncertainty"
    );
    let failure =
        crate::error::native_write_failure(&error, khive_storage::SqliteWriteStage::Commit)
            .unwrap();
    assert_eq!(failure.primary_code, rusqlite::ffi::SQLITE_AUTH);
    assert!(
        !failure.settlement_unknown,
        "successful rollback remains known"
    );
    let snapshot = probe.snapshot();
    assert_eq!(
        (snapshot.matched_requests, snapshot.reply_suppressed),
        (1, 0)
    );
    assert!(snapshot.commit_issued && !snapshot.commit_succeeded);
    let count: i64 = Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM outcome_probe", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "failed COMMIT must preserve the real rollback result"
    );
}

#[tokio::test]
async fn reply_receipt_preserves_computed_errors_before_and_after_commit() {
    for issued in [false, true] {
        let (reply, reply_rx) = oneshot::channel::<Result<(), StorageError>>();
        let receipt = Arc::new(ReplyReceipt::default());
        receipt.commit_issued.store(issued, Ordering::Release);
        *receipt.completed() = Some(Err(StorageError::writer_task_terminated(
            WriterTaskRequestState::TransactionRolledBack,
        )));
        drop(reply);
        let result = PendingWrite { reply_rx, receipt }.wait().await;
        assert!(
            matches!(
                result,
                Err(StorageError::WriterTaskTerminated {
                    request_state: WriterTaskRequestState::TransactionRolledBack,
                    ..
                })
            ),
            "known rollback must outrank reply loss regardless of COMMIT evidence: {result:?}"
        );
    }
}

#[test]
fn raw_pooled_and_standalone_writes_refuse_strict_before_callbacks() {
    for strict in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let pool = seeded_pool(&dir.path().join("raw.db"), strict, false);
        let callbacks = AtomicUsize::new(0);
        let mut outcomes = Vec::new();
        outcomes.push(pool.writer().unwrap().autocommit(|conn| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            conn.execute("INSERT INTO outcome_probe VALUES(1,'pooled')", [])?;
            Ok(())
        }));
        outcomes.push(pool.writer().unwrap().transaction(|conn| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            conn.execute("INSERT INTO outcome_probe VALUES(2,'transaction')", [])?;
            Ok(())
        }));
        outcomes.push(pool.standalone_autocommit(|conn| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            conn.execute("INSERT INTO outcome_probe VALUES(3,'standalone')", [])?;
            Ok(())
        }));
        outcomes.push(pool.standalone_transaction(|conn| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            conn.execute(
                "INSERT INTO outcome_probe VALUES(4,'standalone transaction')",
                [],
            )?;
            Ok(())
        }));
        for result in outcomes {
            if strict {
                assert!(
                    matches!(result, Err(SqliteError::WriterRouteViolation { .. })),
                    "raw strict writes need a typed admission refusal: {result:?}"
                );
            } else {
                result.unwrap();
            }
        }
        let count: i64 = pool
            .reader()
            .unwrap()
            .query_row("SELECT count(*) FROM outcome_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            (callbacks.load(Ordering::SeqCst), count),
            if strict { (0, 0) } else { (4, 4) },
            "strict refusal precedes all four callbacks; default writes and reads stay valid"
        );
    }
}

#[tokio::test]
async fn strict_memory_sql_and_atomic_refuse_while_default_still_writes() {
    for strict in [false, true] {
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                write_routing_strict: strict,
                write_queue_enabled: Some(false),
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        let bridge = crate::sql_bridge::SqlBridge::new(Arc::clone(&pool), false);
        let writer = bridge.writer().await;
        if strict {
            assert!(
                matches!(writer, Err(StorageError::WriterOwnerUnavailable { .. })),
                "strict memory writer must not bypass owner resolution"
            );
        } else {
            writer
                .unwrap()
                .execute_script("CREATE TABLE memory_probe(value INTEGER)".into())
                .await
                .unwrap();
        }
        let called = Arc::new(AtomicBool::new(false));
        let inside = Arc::clone(&called);
        let result = bridge
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    inside.store(true, Ordering::SeqCst);
                    writer
                        .execute_script("CREATE TABLE atomic_probe(value INTEGER)".into())
                        .await?;
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                })
            }))
            .await;
        if strict {
            assert!(
                matches!(result, Err(StorageError::WriterOwnerUnavailable { .. })),
                "strict memory atomic must fail before operation"
            );
        } else {
            result.unwrap();
        }
        assert_eq!(called.load(Ordering::SeqCst), !strict);
    }
}

#[tokio::test]
async fn strict_stopped_owner_refuses_before_queue_acceptance() {
    let (tx, rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
    let writer = WriterTaskHandle {
        tx,
        backend_key: None,
        db: "strict stopped".into(),
        slow_write_threshold: None,
        enqueue_timeout: Duration::from_secs(1),
        admission_metrics: WriterAdmissionMetrics::new(),
        strict_routing: true,
    };
    let dir = tempfile::tempdir().unwrap();
    let pool = seeded_pool(&dir.path().join("stopped.db"), true, true);
    let live = pool
        .writer_task_for_write(Some(&writer), "stored_write")
        .unwrap()
        .expect("a live strict owner must resolve before the close race");
    drop(rx);
    let resolved = pool.writer_task_for_write(Some(&writer), "stored_write");
    assert!(
        matches!(resolved, Err(StorageError::WriterOwnerUnavailable { .. })),
        "cached stopped owner is a preadmission failure"
    );
    let ran = Arc::new(AtomicBool::new(false));
    let inside = Arc::clone(&ran);
    let result = live
        .send(move |_| {
            inside.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(
        matches!(result, Err(StorageError::WriterOwnerUnavailable { .. })),
        "closed reservation race retains strict owner taxonomy"
    );
    assert!(!ran.load(Ordering::SeqCst));
}

#[test]
fn strict_core_and_pack_schema_keep_closed_lifecycle_authority() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_with_pool_config(
        dir.path().join("schema.db"),
        PoolConfig {
            write_routing_strict: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        },
        None,
    )
    .unwrap();
    let version = backend.prepare_core_schema().unwrap();
    assert!(
        version > 0,
        "real core migrations must retain their closed owner"
    );
    raw_write_refuses(backend.pool());
    backend
        .apply_pack_ddl_statements_with_columns(
            &["CREATE TABLE IF NOT EXISTS pack_probe(value INTEGER)"],
            &[],
        )
        .unwrap();
    let exists: i64 = backend
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='pack_probe'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(exists, 1, "strict pack schema must execute its real DDL");
    raw_write_refuses(backend.pool());
    backend.notes().unwrap();
    backend.graph().unwrap();
    backend
        .register_embedding_model("fixture", "model", "v1", 3)
        .unwrap();
    raw_write_refuses(backend.pool());
}

#[tokio::test]
async fn strict_lazy_index_repair_remains_schema_only() {
    use crate::stores::index_repair::{run_indexed_read, IndexReadKind, IndexRepairContext};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let conn = Connection::open(&path).unwrap();
    crate::stores::note::ensure_notes_schema(&conn).unwrap();
    conn.execute_batch("DROP INDEX idx_notes_message_recipient_direction")
        .unwrap();
    drop(conn);
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_routing_strict: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let gates = std::array::from_fn(|_| Arc::new(StoreSchemaGate::default()));
    let repair = IndexRepairContext::new(Arc::clone(&pool), gates, IndexReadKind::Notes);
    let count = run_indexed_read(
        Arc::clone(&pool),
        Some(repair),
        StorageCapability::Notes,
        "index_fixture",
        |conn| {
            conn.query_row(
                "SELECT count(*) FROM notes INDEXED BY idx_notes_message_recipient_direction WHERE deleted_at IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
        },
    )
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "repair recreates the actual forced index without creating note rows"
    );
    raw_write_refuses(&pool);
}

#[tokio::test]
async fn strict_checkpoint_and_queued_vacuum_do_not_authorize_raw_writes() {
    let dir = tempfile::tempdir().unwrap();
    let pool = seeded_pool(&dir.path().join("maintenance.db"), true, true);
    pool.open_standalone_checkpoint()
        .unwrap()
        .passive()
        .unwrap();
    raw_write_refuses(&pool);
    let writer = pool.writer_task_handle().unwrap().unwrap();
    let value = writer
        .send(|conn| {
            conn.execute("INSERT INTO outcome_probe VALUES(1,'queue')", [])
                .map_err(|e| StorageError::driver(StorageCapability::Sql, "fixture", e))?;
            Ok(7)
        })
        .await
        .unwrap();
    assert_eq!(
        value, 7,
        "the real queue owner remains admitted in strict mode"
    );
    writer
        .send_vacuum_bounded(|conn| {
            conn.execute_batch("VACUUM")
                .map_err(|e| StorageError::driver(StorageCapability::Sql, "fixture_vacuum", e))?;
            Ok(())
        })
        .await
        .unwrap();
    raw_write_refuses(&pool);
    let count: i64 = pool
        .reader()
        .unwrap()
        .query_row("SELECT count(*) FROM outcome_probe", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1, "queued VACUUM preserves committed data");
}

#[test]
fn strict_attachment_cutover_keeps_closed_lifecycle_authority() {
    use crate::migrations::AttachmentCutoverStatus;
    use khive_storage::{Attachment, AttachmentSubstrate};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-cutover.db");
    let id = uuid::Uuid::new_v4();
    let content = khive_storage::blob::ContentRef::from_hex("a".repeat(64)).unwrap();
    let verified = khive_storage::blob::ContentRef::from_hex("b".repeat(64)).unwrap();
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("../sql/schema-migrations-table.sql"))
        .unwrap();
    // Build the actual package-local V20 schema before strict routing is enabled.
    // The test must perform the cutover, not an already-complete no-op.
    for migration in crate::migrations::MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 20)
    {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(migration.up).unwrap();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![migration.version, migration.name],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, entity_type, name, tags, created_at, updated_at, content_ref) \
         VALUES (?1, 'local', 'document', NULL, 'cutover owner', '[]', 1, 1, ?2)",
        rusqlite::params![id.to_string(), content.as_str()],
    )
    .unwrap();
    drop(conn);

    let backend = StorageBackend::sqlite_with_pool_config(
        &path,
        PoolConfig {
            write_routing_strict: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        },
        None,
    )
    .unwrap();
    let owner = crate::stores::blob::acquire_database_gc_owner_for_path_blocking(
        backend.sql().database_path(),
    )
    .unwrap();
    assert_eq!(
        backend.attachment_cutover_status().unwrap(),
        AttachmentCutoverStatus::Pending,
        "the strict lifecycle fixture must enter a real pending cutover"
    );
    raw_write_refuses(backend.pool());

    backend.stage_attachment_cutover(&owner).unwrap();
    assert_eq!(
        backend.attachment_cutover_status().unwrap(),
        AttachmentCutoverStatus::Incomplete
    );
    let backfilled: String = backend
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT content_ref FROM attachments WHERE record_uuid=?1 AND role='content'",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backfilled,
        content.as_str(),
        "strict cutover stage must preserve the actual legacy content attachment"
    );
    raw_write_refuses(backend.pool());

    backend
        .apply_verified_attachments(
            &owner,
            &[Attachment {
                record_uuid: id,
                substrate: AttachmentSubstrate::Entity,
                role: "verified-source".into(),
                content_ref: verified.clone(),
                media_type: Some("application/octet-stream".into()),
                size_bytes: Some(7),
                created_at: 2,
            }],
        )
        .unwrap();
    let stored: (String, String, i64, i64) = backend
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT content_ref, media_type, size_bytes, created_at FROM attachments \
             WHERE record_uuid=?1 AND role='verified-source'",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        stored,
        (
            verified.as_str().to_owned(),
            "application/octet-stream".into(),
            7,
            2
        ),
        "strict verified-attachment publication must persist its actual metadata"
    );
    raw_write_refuses(backend.pool());

    backend.finalize_attachment_cutover(&owner).unwrap();
    assert_eq!(
        backend.attachment_cutover_status().unwrap(),
        AttachmentCutoverStatus::Complete,
        "strict finalization must commit the actual V21 cutover"
    );
    let final_state: (i64, i64, i64, String) = backend
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT (SELECT max(version) FROM _schema_migrations), \
             (SELECT count(*) FROM pragma_table_info('entities') WHERE name='content_ref'), \
             (SELECT count(*) FROM attachments WHERE record_uuid=?1), \
             (SELECT name FROM entities WHERE id=?1)",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        final_state,
        (21, 0, 2, "cutover owner".into()),
        "strict cutover must retain the owner and both attachments while recording V21 and removing the legacy column"
    );
    raw_write_refuses(backend.pool());
}

#[test]
fn strict_service_migration_keeps_closed_lifecycle_authority() {
    use crate::migrations::{Migration, ServiceSchemaPlan};

    static STEPS: &[Migration] = &[Migration {
        id: "001_strict_service",
        up_sql: "CREATE TABLE service_probe(value INTEGER NOT NULL); INSERT INTO service_probe VALUES(7);",
        down_sql: None,
        is_already_applied: None,
    }];
    static PLAN: ServiceSchemaPlan = ServiceSchemaPlan {
        service: "strict_service_fixture",
        sqlite: STEPS,
        postgres: &[],
    };
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_with_pool_config(
        dir.path().join("service.db"),
        PoolConfig {
            write_routing_strict: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        },
        None,
    )
    .unwrap();
    for _ in 0..2 {
        backend.apply_schema(&PLAN).unwrap();
        let stored: (i64, i64, i64) = backend
            .pool()
            .reader()
            .unwrap()
            .query_row(
                "SELECT (SELECT count(*) FROM service_probe), \
                 (SELECT value FROM service_probe), \
                 (SELECT count(*) FROM _schema_versions WHERE service='strict_service_fixture' AND migration_id='001_strict_service')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            stored,
            (1, 7, 1),
            "strict service migration and replay must retain one actual effect and one version row"
        );
        raw_write_refuses(backend.pool());
    }
}

async fn assert_labelled_commit_reply_loss(use_statement_labels: bool) {
    let route = if use_statement_labels {
        "send_labelled"
    } else {
        "send_at"
    };
    for strict in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labelled-outcome.db");
        let pool = seeded_pool(&path, strict, true);
        let writer = pool.writer_task_handle().unwrap().unwrap();
        let probe = writer
            .send_top_level(|conn| {
                Ok(conn.test_suppress_commit_reply_after_update("outcome_probe", 1))
            })
            .await
            .unwrap();
        let write = |conn: &Connection| {
            conn.execute(
                "INSERT INTO outcome_probe VALUES(1,'labelled committed')",
                [],
            )
            .map_err(|error| {
                StorageError::driver(StorageCapability::Sql, "labelled_fixture", error)
            })?;
            Ok(())
        };
        // The test caller stays alive and awaits the real labelled submit path.
        let result = if use_statement_labels {
            writer
                .send_labelled(write, vec!["labelled_commit_probe".to_owned()])
                .await
        } else {
            writer.send_at(write, "labelled_commit_probe").await
        };
        let snapshot = probe.snapshot();
        assert_eq!(
            (snapshot.matched_requests, snapshot.reply_suppressed),
            (1, 1),
            "{route} must lose only its actual targeted successful reply"
        );
        assert!(
            snapshot.commit_issued && snapshot.commit_succeeded && snapshot.autocommit,
            "{route} must reach real successful COMMIT and autocommit before reply loss"
        );
        let independent = Connection::open(&path).unwrap();
        let committed: (i64, String) = independent
            .query_row(
                "SELECT count(*), (SELECT value FROM outcome_probe WHERE id=1) FROM outcome_probe",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            committed,
            (1, "labelled committed".into()),
            "{route} reply loss must leave exactly the literal committed row"
        );
        assert!(
            matches!(result, Err(StorageError::WriteOutcomeUnknown { .. })),
            "{route} must preserve issued-COMMIT receipt evidence after lost reply: {result:?}"
        );
        assert!(
            !writer.is_closed(),
            "{route} reply loss must not stop the owner"
        );
        let follow_up = |conn: &Connection| {
            conn.execute("INSERT INTO outcome_probe VALUES(2,'labelled healthy')", [])
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "labelled_follow_up", error)
                })?;
            Ok(())
        };
        let healthy = if use_statement_labels {
            writer
                .send_labelled(follow_up, vec!["labelled_healthy_probe".to_owned()])
                .await
        } else {
            writer.send_at(follow_up, "labelled_healthy_probe").await
        };
        assert!(
            healthy.is_ok(),
            "{route} must accept a later write: {healthy:?}"
        );
        let persisted: (i64, String, String) = independent
            .query_row(
                "SELECT count(*), (SELECT value FROM outcome_probe WHERE id=1), \
                 (SELECT value FROM outcome_probe WHERE id=2) FROM outcome_probe",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            persisted,
            (2, "labelled committed".into(), "labelled healthy".into()),
            "{route} must preserve the first commit and commit the later healthy write"
        );
        assert_eq!(probe.snapshot().reply_suppressed, 1);
    }
}

#[tokio::test]
async fn send_at_lost_commit_reply_preserves_receipt_and_writer() {
    assert_labelled_commit_reply_loss(false).await;
}

#[tokio::test]
async fn send_labelled_lost_commit_reply_preserves_receipt_and_writer() {
    assert_labelled_commit_reply_loss(true).await;
}
