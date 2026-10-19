use super::*;
use std::sync::atomic::AtomicBool;

fn file_pool(path: &Path) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path.to_path_buf()),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            guard_deadline_ms: 100,
            ..Default::default()
        }),
        ..PoolConfig::for_test()
    })
    .unwrap()
}

fn row_count(pool: &ConnectionPool) -> i64 {
    pool.reader()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn standalone_autocommit_samples_at_each_execution_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = file_pool(&dir.path().join("autocommit.db"));
    let available = Arc::new(AtomicU64::new(101));
    let probe = Arc::clone(&available);
    pool.set_test_write_admission(100, move |_| Ok(probe.load(Ordering::SeqCst)));
    pool.standalone_autocommit(|conn| {
        conn.execute_batch("CREATE TABLE witness(value INTEGER)")?;
        Ok(())
    })
    .unwrap();

    // The pool and its earlier write succeeded with a healthy sample. Only
    // execution-time admission can observe this changed capacity.
    available.store(100, Ordering::SeqCst);
    let body_ran = AtomicBool::new(false);
    let refused = pool.standalone_autocommit(|conn| {
        body_ran.store(true, Ordering::SeqCst);
        conn.execute_batch("INSERT INTO witness VALUES(1)")?;
        Ok(())
    });
    assert!(matches!(refused, Err(SqliteError::CapacityFloor { .. })));
    assert!(!body_ran.load(Ordering::SeqCst));
    assert_eq!(row_count(&pool), 0);

    available.store(101, Ordering::SeqCst);
    pool.standalone_autocommit(|conn| {
        conn.execute_batch("INSERT INTO witness VALUES(2)")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(row_count(&pool), 1);
}

#[test]
fn standalone_transaction_samples_after_begin_before_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transaction.db");
    let mut pool = file_pool(&path);
    let samples = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        let competing = Connection::open(&path).unwrap();
        competing.busy_timeout(Duration::ZERO).unwrap();
        let error = competing.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _)
            if matches!(code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)));
        Ok(if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            100
        } else {
            101
        })
    });
    let body_ran = AtomicBool::new(false);
    let refused = pool.standalone_transaction(|_| {
        body_ran.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert!(matches!(refused, Err(SqliteError::CapacityFloor { .. })));
    assert!(!body_ran.load(Ordering::SeqCst));
    pool.standalone_transaction(|conn| {
        conn.execute_batch("CREATE TABLE witness(value INTEGER); INSERT INTO witness VALUES(1)")?;
        Ok(())
    })
    .expect("capacity refusal rolled back and released its lease");
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    assert_eq!(row_count(&pool), 1);
}

#[test]
fn standalone_scripts_preserve_complete_transactions_and_refuse_unfinished_ones() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("script.db"));
    pool.standalone_autocommit(|conn| {
        conn.execute_batch(
            "CREATE TABLE witness(value INTEGER); BEGIN IMMEDIATE; \
             INSERT INTO witness VALUES(1); COMMIT",
        )?;
        Ok(())
    })
    .unwrap();
    let incomplete = pool.standalone_autocommit(|conn| {
        conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO witness VALUES(2)")?;
        Ok(())
    });
    assert!(matches!(incomplete, Err(SqliteError::InvalidData(_))));
    assert_eq!(row_count(&pool), 1);

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.standalone_transaction(|conn| -> Result<(), SqliteError> {
            conn.execute_batch("INSERT INTO witness VALUES(3)")?;
            panic!("caller unwound before settlement");
        })
    }));
    assert!(panic.is_err());
    assert_eq!(row_count(&pool), 1);
    pool.standalone_transaction(|conn| {
        conn.execute_batch("INSERT INTO witness VALUES(4)")?;
        Ok(())
    })
    .expect("unwinding settled the original connection before releasing its lease");
    assert_eq!(row_count(&pool), 2);
}

#[test]
fn standalone_fts_merge_is_admitted_while_checkpoint_recovery_bypasses_the_floor() {
    use crate::fts_maintenance::{FtsMaintenanceConfig, FtsMaintenanceState};

    let dir = tempfile::tempdir().unwrap();
    let mut pool = file_pool(&dir.path().join("maintenance.db"));
    pool.standalone_autocommit(|conn| {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE fts_entities USING fts5(body); \
             CREATE VIRTUAL TABLE fts_notes USING fts5(body); \
             INSERT INTO fts_entities(fts_entities, rank) VALUES('automerge', 0); \
             INSERT INTO fts_notes(fts_notes, rank) VALUES('automerge', 0)",
        )?;
        Ok(())
    })
    .unwrap();
    for index in 0..4 {
        pool.standalone_autocommit(|conn| {
            for table in ["fts_entities", "fts_notes"] {
                conn.execute(
                    &format!("INSERT INTO {table}(body) VALUES(?1)"),
                    [format!("independently committed segment {index}")],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    let segments =
        crate::fts_maintenance::inspect_fts_segments(pool.reader().unwrap().conn()).unwrap();
    assert!(segments.entities.segment_count >= 2);
    assert!(segments.notes.segment_count >= 2);

    let available = Arc::new(AtomicU64::new(100));
    let samples = Arc::new(AtomicU64::new(0));
    let sampled = Arc::clone(&available);
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(sampled.load(Ordering::SeqCst))
    });
    let observation = pool.observe_test_statement_starts(1_024).unwrap();
    let merge_attempts = || {
        observation
            .started_statements()
            .unwrap()
            .into_iter()
            .filter(|statement| statement.sql.contains("VALUES('merge', ?1)"))
            .count()
    };
    let now = Instant::now();
    let config = FtsMaintenanceConfig {
        interval: Duration::from_secs(1),
        merge_pages: 1,
        ..FtsMaintenanceConfig::default()
    };
    let due = now + config.interval;
    let mut state = FtsMaintenanceState::new(now);
    assert!(pool.run_fts_maintenance(&config, &mut state, due).is_err());
    assert_eq!(samples.load(Ordering::SeqCst), 1);
    assert_eq!(merge_attempts(), 0, "refused FTS DML must never start");

    // Recovery must work under the very same refusal. The fixed capability
    // does not invoke ordinary write admission or expose its SQL connection.
    let checkpoint = pool.open_standalone_checkpoint().unwrap();
    assert_eq!(checkpoint.passive().unwrap().busy, 0);
    assert_eq!(checkpoint.truncate().unwrap().busy, 0);
    assert_eq!(samples.load(Ordering::SeqCst), 1);
    drop(checkpoint);

    // Positive control: the exact production path reaches one merge INSERT
    // when capacity recovers. A permanently disabled merge would fail here.
    available.store(101, Ordering::SeqCst);
    let mut recovered = FtsMaintenanceState::new(now);
    assert!(pool
        .run_fts_maintenance(&config, &mut recovered, due)
        .unwrap()
        .is_some());
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    assert_eq!(merge_attempts(), 1);
}

#[tokio::test]
async fn cancelling_an_observer_keeps_the_standalone_owner_and_lease_together() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(file_pool(&dir.path().join("cancelled.db")));
    pool.standalone_autocommit(|conn| {
        conn.execute_batch("CREATE TABLE witness(value INTEGER)")?;
        Ok(())
    })
    .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let owner_pool = Arc::clone(&pool);
    let owner = tokio::task::spawn_blocking(move || {
        owner_pool.standalone_transaction(|conn| {
            conn.execute_batch("INSERT INTO witness VALUES(1)")?;
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    entered_rx.await.unwrap();
    owner.abort();

    let contender_pool = Arc::clone(&pool);
    let body_ran = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&body_ran);
    let contender = tokio::task::spawn_blocking(move || {
        contender_pool.standalone_autocommit(|_| {
            observed.store(true, Ordering::SeqCst);
            Ok(())
        })
    })
    .await;
    // Unblock the owning closure before any assertion can unwind the test.
    release_tx.send(()).unwrap();
    let finished = owner.await;
    assert!(matches!(
        contender.unwrap(),
        Err(SqliteError::CapacityUnavailable {
            phase: khive_storage::CapacityUnavailablePhase::LockContended,
            ..
        })
    ));
    assert!(!body_ran.load(Ordering::SeqCst));
    finished.unwrap().unwrap();
    assert_eq!(row_count(&pool), 1);
    pool.standalone_autocommit(|_| Ok(())).unwrap();
}
