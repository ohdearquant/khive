use super::{cycle_panic_seam, run_checkpoint_core_off_worker, PanickedCycle};
use crate::checkpoint::tests::{checkpoint_conn, file_pool, wait_for};
use crate::checkpoint::{
    checkpoint_skipped_ticks, checkpoint_timing, reset_checkpoint_metrics_for_tests,
    run_checkpoint_task, truncate_attempts, CheckpointConfig, TruncateState,
};
use crate::pool::ConnectionPool;
use serial_test::serial;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct CyclePanicSeamGuard;

impl Drop for CyclePanicSeamGuard {
    fn drop(&mut self) {
        cycle_panic_seam::uninstall();
    }
}

fn seeded_pool() -> (tempfile::TempDir, Arc<ConnectionPool>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = file_pool(&dir.path().join("panicked-cycle.db"));
    {
        let writer = pool.writer().expect("writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .expect("seed WAL frames");
    }
    (dir, pool)
}

/// Run the real checkpoint task with TRUNCATE armed on every tick and one
/// panic injected into the first cycle that decides to attempt it. Returns the
/// TRUNCATE attempts completed and the ticks skipped once two more cycles have
/// run after the panic.
async fn attempts_after_one_panicked_cycle(truncate_min_interval: Duration) -> (u64, u64) {
    let (_dir, pool) = seeded_pool();
    reset_checkpoint_metrics_for_tests();
    let canonical_path = pool
        .canonical_path()
        .expect("file-backed pool has a canonical path")
        .to_path_buf();
    cycle_panic_seam::install(canonical_path);
    let _seam_guard = CyclePanicSeamGuard;

    let attempts_before = truncate_attempts();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        CheckpointConfig {
            interval: Duration::from_millis(20),
            truncate_high_water_pages: 0,
            truncate_min_interval,
            ..CheckpointConfig::default()
        },
        None,
        shutdown_rx,
        true,
    ));
    let panicked = wait_for(Duration::from_secs(10), || checkpoint_skipped_ticks() > 0).await;
    let ticks = || checkpoint_timing(&pool).ticks;
    let target_ticks = ticks() + 2;
    let continued = wait_for(Duration::from_secs(10), || ticks() >= target_ticks).await;

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("checkpoint task should exit after shutdown")
        .expect("checkpoint task panicked");

    assert!(
        panicked,
        "the injected panic must surface as a skipped checkpoint tick"
    );
    assert!(
        continued,
        "the checkpoint task must keep ticking after the panicked cycle"
    );
    let attempts = truncate_attempts() - attempts_before;
    (attempts, checkpoint_skipped_ticks())
}

/// Hand `state` to a checkpoint cycle that panics once it has decided to
/// attempt TRUNCATE, and return what the wrapper gives back.
async fn run_panicking_cycle(state: TruncateState) -> Box<PanickedCycle> {
    let (_dir, pool) = seeded_pool();
    let canonical_path = pool
        .canonical_path()
        .expect("file-backed pool has a canonical path")
        .to_path_buf();
    cycle_panic_seam::install(canonical_path);
    let _seam_guard = CyclePanicSeamGuard;
    let conn = checkpoint_conn(&pool);
    let config = CheckpointConfig {
        truncate_high_water_pages: 0,
        ..CheckpointConfig::default()
    };
    let joined = run_checkpoint_core_off_worker(Arc::clone(&pool), conn, config, state).await;
    let Err(panicked) = joined else {
        panic!("the injected panic must surface as a panicked cycle");
    };
    assert!(
        panicked.join_error.is_panic(),
        "the injected failure must be a panic: {}",
        panicked.join_error
    );
    panicked
}

/// A cycle that panics after deciding to attempt TRUNCATE must leave the
/// cooldown armed: the next tick does not attempt TRUNCATE until the cooldown
/// has expired. The zero-cooldown run first shows the same fixture does
/// attempt TRUNCATE on the tick after the panic when the cooldown allows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn a_panic_after_a_truncate_decision_keeps_the_cooldown_armed() {
    let (attempts, skipped) = attempts_after_one_panicked_cycle(Duration::ZERO).await;
    assert_eq!(
        skipped, 1,
        "the panicked tick must still be counted as a skipped tick"
    );
    assert!(
        attempts > 0,
        "fixture invalid: with no cooldown the tick after the panic must attempt TRUNCATE"
    );

    let (attempts, skipped) = attempts_after_one_panicked_cycle(Duration::from_secs(600)).await;
    assert_eq!(
        skipped, 1,
        "the panicked tick must still be counted as a skipped tick"
    );
    assert_eq!(
        attempts, 0,
        "the ticks after a panicked attempt must wait out the TRUNCATE cooldown"
    );
}

/// The failure streak drives the one-shot escalated warning, so a panicked
/// cycle must not restart it from zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn a_panicked_cycle_hands_back_the_failure_streak() {
    let state = TruncateState {
        consecutive_failures: 2,
        ..TruncateState::default()
    };
    let panicked = run_panicking_cycle(state).await;
    assert_eq!(
        panicked.truncate_state.consecutive_failures, 2,
        "the failure streak must survive a panicked cycle"
    );
}

/// The full-scan reservation spaces the sidecar enumeration passes, so a
/// panicked cycle must not drop it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn a_panicked_cycle_hands_back_the_full_scan_reservation() {
    let reserved_at = Instant::now();
    let state = TruncateState {
        walpin_full_scan_last_attempt: Some(reserved_at),
        ..TruncateState::default()
    };
    let panicked = run_panicking_cycle(state).await;
    assert_eq!(
        panicked.truncate_state.walpin_full_scan_last_attempt,
        Some(reserved_at),
        "the full-scan reservation must survive a panicked cycle"
    );
}

/// A cycle that panics after deciding to attempt TRUNCATE is counted as that
/// attempt: the state it comes back with has its attempt time set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn a_panicked_cycle_counts_as_a_truncate_attempt_against_the_cooldown() {
    let before = Instant::now();
    let panicked = run_panicking_cycle(TruncateState::default()).await;
    let attempted_at = panicked.truncate_state.last_attempt;
    assert!(
        attempted_at.is_some_and(|attempt| attempt >= before),
        "a panicked cycle must arm the TRUNCATE cooldown"
    );
}
