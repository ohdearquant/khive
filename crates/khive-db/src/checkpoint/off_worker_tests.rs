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
