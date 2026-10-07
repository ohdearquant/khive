use super::off_worker::run_checkpoint_core_off_worker;
use super::tests::{checkpoint_conn, file_pool, wait_for};
use super::{
    checkpoint_once_core, run_checkpoint_task, truncate_attempts, CheckpointConfig, TruncateState,
};
use crate::pool::ConnectionPool;
use serial_test::serial;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `run_checkpoint_core_off_worker` moves the same `checkpoint_once_core`
/// call onto a blocking thread; driven on a genuinely multi-threaded
/// runtime against an identically-seeded database with TRUNCATE armed, it
/// must reach the same outcome and hand back the same escalation state as
/// calling the core directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn checkpoint_core_off_worker_matches_the_direct_call() {
    fn seeded_pool() -> (tempfile::TempDir, Arc<ConnectionPool>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = file_pool(&dir.path().join("checkpoint-off-worker.db"));
        {
            let writer = pool.writer().expect("writer");
            writer
                .conn()
                .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .expect("seed WAL frames");
        }
        (dir, pool)
    }

    let config = CheckpointConfig {
        truncate_high_water_pages: 0,
        ..CheckpointConfig::default()
    };

    let (_direct_dir, direct_pool) = seeded_pool();
    let direct_conn = checkpoint_conn(&direct_pool);
    let mut direct_state = TruncateState::default();
    let direct = checkpoint_once_core(&direct_pool, &direct_conn, &config, &mut direct_state)
        .expect("direct checkpoint core");

    let (_wrapped_dir, wrapped_pool) = seeded_pool();
    let wrapped_conn = checkpoint_conn(&wrapped_pool);
    let joined = run_checkpoint_core_off_worker(
        Arc::clone(&wrapped_pool),
        wrapped_conn,
        config.clone(),
        TruncateState::default(),
    )
    .await;
    let (_conn, wrapped_state, wrapped) = joined.expect("checkpoint core did not panic");
    let wrapped = wrapped.expect("wrapped checkpoint core");

    assert!(
        direct.wal_pages.is_some(),
        "the seeded WAL must yield a PASSIVE observation on the direct path"
    );
    assert!(
        direct_state.last_attempt.is_some(),
        "the armed TRUNCATE must have been attempted on the direct path"
    );
    assert_eq!(
        direct.wal_pages, wrapped.wal_pages,
        "the offloaded call must observe the same WAL pages as the direct call"
    );
    assert_eq!(
        direct.unavailable_reason, wrapped.unavailable_reason,
        "the offloaded call must classify the PASSIVE row like the direct call"
    );
    assert_eq!(
        direct.sidecar_attribution.is_some(),
        wrapped.sidecar_attribution.is_some(),
        "the offloaded call must hand back the same attribution request shape"
    );
    assert_eq!(
        direct_state.last_attempt.is_some(),
        wrapped_state.last_attempt.is_some(),
        "the escalation state returned by the offloaded call must record the attempt"
    );
    assert_eq!(
        direct_state.consecutive_failures, wrapped_state.consecutive_failures,
        "the offloaded call must leave the same TRUNCATE failure streak"
    );
}

/// An armed TRUNCATE busy-waits inside SQLite for up to
/// `truncate_busy_timeout` while a reader pins the WAL. The checkpoint
/// task runs that call on a blocking thread, so a runtime with a single
/// worker keeps polling its other tasks meanwhile: a 10 ms ticker beside
/// the task must never see a gap anywhere near the busy timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn truncate_busy_wait_does_not_starve_a_single_worker_runtime() {
    const TRUNCATE_BUSY: Duration = Duration::from_millis(1_000);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("truncate-starvation.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().expect("writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .expect("seed WAL frames");
    }
    // A read snapshot that predates the next commit: TRUNCATE has to wait
    // for it, so the busy wait below runs for its whole timeout.
    let reader = rusqlite::Connection::open(&path).expect("open reader");
    reader
        .execute_batch("BEGIN DEFERRED")
        .expect("begin read transaction");
    reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
        .expect("materialize the read snapshot");
    {
        let writer = pool.writer().expect("writer");
        writer
            .conn()
            .execute("INSERT INTO t VALUES (2)", [])
            .expect("append a WAL frame behind the reader snapshot");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let ticker_stop = Arc::clone(&stop);
    let ticker = tokio::spawn(async move {
        let mut last = Instant::now();
        let mut max_gap = Duration::ZERO;
        while !ticker_stop.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let now = Instant::now();
            max_gap = max_gap.max(now.duration_since(last));
            last = now;
        }
        max_gap
    });
    // Let the ticker settle before the checkpoint task shares its worker.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let before = truncate_attempts();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let started = Instant::now();
    let task = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        CheckpointConfig {
            interval: Duration::from_secs(60),
            truncate_high_water_pages: 0,
            truncate_busy_timeout: TRUNCATE_BUSY,
            ..CheckpointConfig::default()
        },
        None,
        shutdown_rx,
        true,
    ));
    let attempted = wait_for(Duration::from_secs(10), || truncate_attempts() > before).await;
    let waited = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    let max_gap = ticker.await.expect("ticker task");

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("checkpoint task should exit after shutdown")
        .expect("checkpoint task panicked");
    reader
        .execute_batch("ROLLBACK")
        .expect("release the read transaction");

    assert!(
        attempted,
        "the armed TRUNCATE attempt must complete within the deadline"
    );
    assert!(
        waited >= TRUNCATE_BUSY / 2,
        "fixture invalid: the armed TRUNCATE finished after {waited:?} without waiting out \
         its {TRUNCATE_BUSY:?} busy timeout on the pinned reader"
    );
    assert!(
        max_gap < TRUNCATE_BUSY / 2,
        "a single-worker runtime went {max_gap:?} between 10 ms ticker wakeups while the \
         TRUNCATE busy wait ran; the checkpoint call must not hold the worker thread for \
         the {TRUNCATE_BUSY:?} busy timeout"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_checkpoint_and_sidecar_recovery_bypass_an_active_floor() {
    let home = tempfile::tempdir().unwrap();
    if crate::test_process::run_in_child(|command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", home.path().join("sink"))
            .env("KHIVE_WALPIN_SIDECAR", "1")
            .env("KHIVE_WALPIN_CENSUS_BUDGET_MS", "0");
    }) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("scheduled-floor.db");
    let locks = root.join("locks");
    let mut pool = ConnectionPool::new(crate::pool::PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(locks.clone()),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(
            crate::disk_guard_config::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
        ),
        ..crate::pool::PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(0, |_| Ok(0));
    pool.writer()
        .unwrap()
        .execute_batch("CREATE TABLE payload (id INTEGER); INSERT INTO payload VALUES (1)")
        .unwrap();
    let wal = path.with_extension("db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let forbid = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = Arc::clone(&probes);
    let forbidden = Arc::clone(&forbid);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(
            !forbidden.load(Ordering::SeqCst),
            "recovery must not sample capacity"
        );
        Ok(0)
    });
    assert!(matches!(
        pool.writer(),
        Err(crate::SqliteError::CapacityFloor {
            available_bytes: 0,
            floor_bytes: 100,
            ..
        })
    ));
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    assert!(
        locks.is_dir(),
        "ordinary admission must create its private lease directory"
    );
    std::fs::remove_dir_all(&locks).unwrap();
    probes.store(0, Ordering::SeqCst);
    forbid.store(true, Ordering::SeqCst);
    let before = pool.writer_acquisition_snapshot();
    let sidecar = crate::walpin::sidecar_dir_for(pool.canonical_path().unwrap());
    let dead_pid = 2_000_000_000;
    assert!(!crate::walpin::is_process_alive(dead_pid));
    crate::walpin::write_beacon(
        &sidecar,
        &crate::walpin::WalpinBeacon {
            pid: dead_pid,
            process_role: "session".into(),
            started_at: 1,
            sweep_interval_ms: 5_000,
        },
    )
    .unwrap();
    let dead_beacon = crate::walpin::beacon_path(&sidecar, dead_pid);
    let live_beacon = crate::walpin::beacon_path(&sidecar, std::process::id());
    assert!(dead_beacon.exists());
    let pool = Arc::new(pool);
    let config = CheckpointConfig {
        interval: Duration::from_millis(10),
        truncate_high_water_pages: 0,
        warn_pages: u64::MAX,
        high_water_pages: u64::MAX,
        ..CheckpointConfig::default()
    };
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let mut task = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        config,
        None,
        shutdown_rx,
        false,
    ));
    let progressed = wait_for(Duration::from_secs(10), || {
        std::fs::metadata(&wal).is_ok_and(|m| m.len() == 0)
            && !dead_beacon.exists()
            && live_beacon.exists()
    })
    .await;
    // A healthy tick must repair a lost registration rather than permanently
    // treating its cached beacon_registered state as proof of a file.
    let removed = progressed && std::fs::remove_file(&live_beacon).is_ok();
    let recovered = removed && wait_for(Duration::from_secs(10), || live_beacon.exists()).await;
    let _ = shutdown.send(());
    let joined = tokio::time::timeout(Duration::from_secs(10), &mut task).await;
    if joined.is_err() {
        task.abort();
        let _ = task.await;
        panic!("checkpoint task did not finish shutdown");
    }
    joined.unwrap().expect("checkpoint task panicked");
    assert!(
        progressed,
        "armed scheduled TRUNCATE and dead-sidecar cleanup must run below floor"
    );
    assert!(
        recovered,
        "the same scheduled owner must restore its missing beacon below floor"
    );
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert!(!locks.exists(), "recovery must not acquire a volume lease");
    assert_eq!(pool.writer_acquisition_snapshot(), before);
    assert_eq!(
        pool.reader()
            .unwrap()
            .query_row("SELECT count(*) FROM payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn checkpoint_once_bypasses_an_active_floor_on_its_real_dedicated_connection() {
    let home = tempfile::tempdir().unwrap();
    if crate::test_process::run_in_child(|command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", home.path().join("sink"))
            .env("KHIVE_WALPIN_SIDECAR", "1")
            .env("KHIVE_WALPIN_CENSUS_BUDGET_MS", "0");
    }) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("once-floor.db");
    let locks = root.join("locks");
    let mut pool = ConnectionPool::new(crate::pool::PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(locks.clone()),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(
            crate::disk_guard_config::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
        ),
        ..crate::pool::PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(0, |_| Ok(0));
    pool.writer()
        .unwrap()
        .execute_batch("CREATE TABLE payload (id INTEGER); INSERT INTO payload VALUES (1)")
        .unwrap();
    let wal = path.with_extension("db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let forbid = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = Arc::clone(&probes);
    let forbidden = Arc::clone(&forbid);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(
            !forbidden.load(Ordering::SeqCst),
            "recovery must not sample capacity"
        );
        Ok(0)
    });
    assert!(matches!(
        pool.writer(),
        Err(crate::SqliteError::CapacityFloor {
            available_bytes: 0,
            floor_bytes: 100,
            ..
        })
    ));
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    assert!(
        locks.is_dir(),
        "ordinary admission must create its private lease directory"
    );
    std::fs::remove_dir_all(&locks).unwrap();
    probes.store(0, Ordering::SeqCst);
    forbid.store(true, Ordering::SeqCst);
    let before = pool.writer_acquisition_snapshot();
    let mut dedicated = super::CheckpointConnection::new();
    let conn = dedicated
        .ensure_open(&pool)
        .expect("dedicated recovery open bypasses admission");
    let mut state = TruncateState::default();
    let pages = super::checkpoint_once(
        &pool,
        conn,
        &CheckpointConfig {
            truncate_high_water_pages: 0,
            ..CheckpointConfig::default()
        },
        &mut state,
    )
    .unwrap();
    assert!(pages > 0, "the PASSIVE pass must observe the seeded WAL");
    assert!(
        state.last_attempt.is_some(),
        "the threshold must actually arm TRUNCATE"
    );
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert!(!locks.exists());
    assert_eq!(pool.writer_acquisition_snapshot(), before);
}
