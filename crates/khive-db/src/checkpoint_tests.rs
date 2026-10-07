use super::*;
use crate::pool::PoolConfig;
use crate::writer_task::WriterTaskHandle;
use rusqlite::hooks::{AuthAction, Authorization};
use serial_test::serial;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tracing::field::{Field, Visit};

static LIVE_CHECKPOINT_BUSY_HANDLER_ENTERED: AtomicBool = AtomicBool::new(false);

fn hold_checkpoint_lock_for_live_busy_probe(_attempt: i32) -> bool {
    LIVE_CHECKPOINT_BUSY_HANDLER_ENTERED.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(250));
    false
}

fn pr3409_active_interval(pool: &ConnectionPool) -> u64 {
    let key = checkpoint_db_key(pool);
    checkpoint_runs()
        .lock()
        .unwrap()
        .get(&key)
        .expect("an owner is active")
        .checkpoint_interval_ms
}

include!("checkpoint/environment_tests.rs");

include!("checkpoint_owner_interval_tests.rs");

#[test]
fn verify_3409_backward_wall_clock_step_bypasses_busy_gap_budget() {
    let mut entry = None;
    let start = Instant::now();
    advance_checkpoint_run_at(&mut entry, Some((0, 20, 7)), 100_000, start, 10);
    advance_checkpoint_run_at(
        &mut entry,
        Some((1, -1, -1)),
        100_005,
        start + Duration::from_millis(5),
        10,
    );
    advance_checkpoint_run_at(
        &mut entry,
        Some((0, 21, 7)),
        90_000,
        start + Duration::from_millis(50),
        10,
    );
    let observed = entry.expect("an entry exists after the third sample");
    assert_eq!(
        observed.run.first_observed_at_unix_ms, 90_000,
        "a backward wall-clock step must not let the busy-gap budget check \
             silently treat a large elapsed gap as zero and continue the stale run"
    );
}

#[test]
fn checkpoint_run_snapshot_uses_monotonic_age_with_future_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("monotonic_pin_age.db"));
    let _guard = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
    let key = checkpoint_db_key(&pool);
    let observed_at = Instant::now() - Duration::from_secs(2);
    {
        let mut runs = checkpoint_runs().lock().unwrap();
        let state = runs.get_mut(&key).expect("active checkpoint task");
        advance_checkpoint_run_at(
            &mut state.entry,
            Some((0, 20, 7)),
            u64::MAX,
            observed_at,
            10,
        );
    }
    let (status, age) = checkpoint_run_snapshot(&pool);
    assert_eq!(
        status,
        CheckpointRunStatus::Observed(CheckpointRun {
            frame: 7,
            first_observed_at_unix_ms: u64::MAX,
        })
    );
    assert!(age.expect("observed run has an age") >= Duration::from_secs(2));
}

#[test]
fn checkpoint_run_guard_captures_the_configured_interval_for_busy_spans() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("busy_span_interval.db"));
    let key = checkpoint_db_key(&pool);
    let guard = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
    let interval_ms = checkpoint_runs()
        .lock()
        .unwrap()
        .get(&key)
        .expect("active task has run state")
        .checkpoint_interval_ms;
    assert_eq!(interval_ms, 10);
    drop(guard);
    assert!(!checkpoint_runs().lock().unwrap().contains_key(&key));
}

#[test]
fn checkpoint_run_ends_on_full_backfill_before_a_later_pin() {
    let mut entry = None;
    advance_checkpoint_run(&mut entry, Some((0, 10, 7)), 100, 500);
    advance_checkpoint_run(&mut entry, Some((0, 12, 7)), 200, 500);
    assert_eq!(
        entry.map(|value| value.run),
        Some(CheckpointRun {
            frame: 7,
            first_observed_at_unix_ms: 100,
        })
    );

    advance_checkpoint_run(&mut entry, Some((0, 12, 12)), 300, 500);
    assert_eq!(entry, None, "a full backfill ends the prior run");

    advance_checkpoint_run(&mut entry, Some((0, 18, 15)), 400, 500);
    assert_eq!(
        entry.map(|value| value.run),
        Some(CheckpointRun {
            frame: 15,
            first_observed_at_unix_ms: 400,
        })
    );
}

#[test]
fn checkpoint_run_restarts_when_log_frame_count_decreases() {
    let mut entry = None;
    advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 100, 500);
    advance_checkpoint_run(&mut entry, Some((0, 22, 7)), 125, 500);
    advance_checkpoint_run(&mut entry, Some((1, -1, -1)), 150, 500);
    advance_checkpoint_run(&mut entry, Some((0, 21, 7)), 200, 500);

    assert_eq!(
        entry.map(|value| value.run),
        Some(CheckpointRun {
            frame: 7,
            first_observed_at_unix_ms: 200,
        }),
        "a lower log count breaks the sequence and starts a new run"
    );
}

#[test]
fn checkpoint_run_ends_on_error_negative_or_unpinned_results() {
    for result in [Some((0, -1, -1)), Some((0, 20, 20)), None] {
        let mut entry = None;
        advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 100, 500);
        advance_checkpoint_run(&mut entry, result, 200, 500);
        assert_eq!(entry, None, "result {result:?} must end the run");
    }
}

#[test]
fn checkpoint_run_keeps_held_pin_across_busy_rows_without_using_their_columns() {
    let mut entry = None;
    advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 100, 500);
    let first = entry.expect("qualifying row begins a run");
    advance_checkpoint_run(&mut entry, Some((1, -1, -1)), 500, 500);
    advance_checkpoint_run(&mut entry, Some((1, 100, 99)), 700, 500);
    assert_eq!(entry.expect("busy is neutral").run, first.run);

    advance_checkpoint_run(&mut entry, Some((0, 21, 7)), 900, 500);
    let resumed = entry.expect("the next informative row continues the run");
    assert_eq!(resumed.run, first.run);
    assert!(!resumed.busy_since_last_informative);
    assert_eq!(resumed.last_log_frames, 21);
}

#[test]
fn checkpoint_run_survives_frequent_busy_results_for_a_one_second_pin() {
    let mut entry = None;
    advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 100, 20);
    for step in 1..=70 {
        let at = 100 + step * 16;
        advance_checkpoint_run(&mut entry, Some((1, -1, -1)), at - 8, 20);
        advance_checkpoint_run(&mut entry, Some((0, 20 + step as i64, 7)), at, 20);
    }
    let run = entry
        .expect("the held pin remains observable through busy rows")
        .run;
    assert_eq!(run.frame, 7);
    assert_eq!(run.first_observed_at_unix_ms, 100);
}

#[test]
fn checkpoint_run_restarts_only_after_busy_span_exceeds_two_configured_intervals() {
    let mut entry = None;
    advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 100, 10);
    advance_checkpoint_run(&mut entry, Some((1, -1, -1)), 105, 10);
    advance_checkpoint_run(&mut entry, Some((0, 21, 7)), 120, 10);
    assert_eq!(
        entry
            .expect("busy span at the bound preserves the run")
            .run
            .first_observed_at_unix_ms,
        100
    );

    advance_checkpoint_run(&mut entry, Some((1, -1, -1)), 125, 10);
    advance_checkpoint_run(&mut entry, Some((0, 22, 7)), 141, 10);
    assert_eq!(
        entry
            .expect("busy span past the bound starts a new run")
            .run
            .first_observed_at_unix_ms,
        141
    );
}

#[test]
fn checkpoint_run_long_nonbusy_interval_does_not_trigger_busy_span_bound() {
    let mut entry = None;
    advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 100, 500);
    advance_checkpoint_run(&mut entry, Some((0, 21, 7)), 1_101, 500);
    assert_eq!(
        entry
            .expect("the busy bound only applies after a busy row")
            .run
            .first_observed_at_unix_ms,
        100
    );
}

#[test]
#[serial(
    checkpoint_skip_metrics,
    khive_walpin_census_budget_env,
    checkpoint_live_busy
)]
fn live_concurrent_busy_checkpoints_preserve_a_reader_pin_run() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_CENSUS_BUDGET_MS", "10");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live_concurrent_busy_pin.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER); INSERT INTO t VALUES (0)")
            .unwrap();
        assert_eq!(query_truncate_observation(writer.conn()).unwrap().busy, 0);
        writer
            .conn()
            .execute("INSERT INTO t VALUES (1)", [])
            .unwrap();
    }

    // The reader sees a WAL frame before another writer advances the log.
    // Holding a snapshot of an already-backfilled database would not pin it.
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN DEFERRED").unwrap();
    let visible_rows: i64 = reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows, 2);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (2)", [])
            .unwrap();
    }

    // Use real SQLite PASSIVE rows with the same tracker and diagnostic
    // path as the periodic checkpoint task. The 500 ms configured interval
    // gives a one-second neutral-busy bound; observations below are faster.
    let _run_owner = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(500));
    let observer = rusqlite::Connection::open(&path).unwrap();
    let first = query_checkpoint_observation(&observer).unwrap();
    assert_eq!(first.busy, 0);
    assert!(first.checkpointed_frames > 0);
    assert!(first.log_frames > first.checkpointed_frames);
    let CheckpointRunStatus::Observed(first_run) = record_checkpoint_run_result(
        &pool,
        Some((first.busy, first.log_frames, first.checkpointed_frames)),
    ) else {
        panic!("the held reader must start a checkpoint run");
    };

    // SQLite holds the CKPT lock while a TRUNCATE waits on this reader's
    // read mark. Its bounded busy handler signals when PASSIVE probes can
    // collide with that lock and return actual busy rows (often -1/-1).
    LIVE_CHECKPOINT_BUSY_HANDLER_ENTERED.store(false, Ordering::SeqCst);
    let contender_path = path.clone();
    let contender = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(contender_path).unwrap();
        conn.busy_handler(Some(hold_checkpoint_lock_for_live_busy_probe))
            .unwrap();
        query_truncate_observation(&conn).unwrap()
    });
    let lock_deadline = Instant::now() + Duration::from_secs(2);
    while !LIVE_CHECKPOINT_BUSY_HANDLER_ENTERED.load(Ordering::SeqCst)
        && Instant::now() < lock_deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        LIVE_CHECKPOINT_BUSY_HANDLER_ENTERED.load(Ordering::SeqCst),
        "TRUNCATE must wait on the held reader while holding the CKPT lock"
    );

    let mut busy_rows = 0;
    for _ in 0..10 {
        let sample = query_checkpoint_observation(&observer).unwrap();
        let status = record_checkpoint_run_result(
            &pool,
            Some((sample.busy, sample.log_frames, sample.checkpointed_frames)),
        );
        if sample.busy != 0 {
            busy_rows += 1;
            assert_eq!(
                status,
                CheckpointRunStatus::Observed(first_run),
                "a real busy PASSIVE row must leave the first run intact"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let contender_result = contender.join().expect("TRUNCATE contender must finish");
    assert_ne!(contender_result.busy, 0, "the held reader blocks TRUNCATE");
    assert!(
        busy_rows > 0,
        "the test must observe real concurrent busy rows"
    );

    let resumed = query_checkpoint_observation(&observer).unwrap();
    assert_eq!(resumed.busy, 0);
    assert_eq!(resumed.checkpointed_frames, first_run.frame);
    assert!(resumed.log_frames >= first.log_frames);
    assert_eq!(
        record_checkpoint_run_result(
            &pool,
            Some((
                resumed.busy,
                resumed.log_frames,
                resumed.checkpointed_frames
            )),
        ),
        CheckpointRunStatus::Observed(first_run),
        "the first informative row after a bounded busy span resumes the run"
    );

    let aging_started = Instant::now();
    while aging_started.elapsed() < Duration::from_millis(1_050) {
        {
            let writer = pool.writer().unwrap();
            writer
                .conn()
                .execute("INSERT INTO t VALUES (3)", [])
                .unwrap();
        }
        let sample = query_checkpoint_observation(&observer).unwrap();
        assert_eq!(sample.busy, 0);
        assert_eq!(sample.checkpointed_frames, first_run.frame);
        assert!(sample.log_frames > sample.checkpointed_frames);
        assert_eq!(
            record_checkpoint_run_result(
                &pool,
                Some((sample.busy, sample.log_frames, sample.checkpointed_frames)),
            ),
            CheckpointRunStatus::Observed(first_run)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let aged = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let probe = aged
        .checkpoint_probe
        .as_ref()
        .expect("diagnostic probe row");
    assert_eq!(probe.busy, 0);
    assert_eq!(
        aged.checkpoint_pin.oldest_pinned_frame,
        Some(first_run.frame)
    );
    assert_eq!(aged.checkpoint_pin.oldest_pinned_frame_run, Some(first_run));
    assert_eq!(
        aged.checkpoint_pin.pin_depth,
        Some(probe.log_frames - first_run.frame)
    );

    reader.execute_batch("ROLLBACK").unwrap();
    let drained = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    assert_eq!(drained.checkpoint_pin.oldest_pinned_frame, None);
    assert_eq!(drained.checkpoint_pin.pin_depth, None);
}

/// The bundled SQLite reuses the slot at the greatest read mark when all
/// frame-carrying slots are occupied. An upgrade that changes this must
/// fail here before the oldest-frame report relies on that behavior.
#[test]
fn wal_read_mark_slots_keep_the_fourth_frame_for_a_later_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read_mark_slots.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER)")
            .unwrap();
        query_truncate_observation(writer.conn()).unwrap();
    }

    let observer = rusqlite::Connection::open(&path).unwrap();
    let mut readers = Vec::new();
    let mut previous_log = 0;
    for value in 0..4 {
        {
            let writer = pool.writer().unwrap();
            writer
                .conn()
                .execute("INSERT INTO t VALUES (?1)", [value])
                .unwrap();
        }
        // Take the snapshot before the checkpoint. If PASSIVE first copies
        // every frame, this reader can use slot 0 (the main database),
        // allowing the next write to restart the WAL at frame 1.
        let reader = rusqlite::Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN DEFERRED").unwrap();
        let rows: i64 = reader
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
            .unwrap();
        assert_eq!(rows, i64::from(value) + 1);
        readers.push(reader);

        let observation = query_checkpoint_observation(&observer).unwrap();
        assert!(observation.log_frames > previous_log);
        previous_log = observation.log_frames;
    }
    let pinned_frame = previous_log;

    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (4)", [])
            .unwrap();
    }
    let fifth_reader = rusqlite::Connection::open(&path).unwrap();
    fifth_reader.execute_batch("BEGIN DEFERRED").unwrap();
    let rows: i64 = fifth_reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(rows, 5, "the fifth snapshot sees the newly committed frame");
    drop(readers);

    let observation = query_checkpoint_observation(&observer).unwrap();
    assert_eq!(observation.busy, 0);
    assert_eq!(observation.checkpointed_frames, pinned_frame);
    assert!(observation.log_frames > pinned_frame);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_census_budget_env)]
async fn db_diagnostics_reports_a_reader_pin_after_one_second_and_clears_after_backfill() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_CENSUS_BUDGET_MS", "10");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("diagnostic_pin_run.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER); INSERT INTO t VALUES (0)")
            .unwrap();
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task_pool = Arc::clone(&pool);
    let task = tokio::spawn(run_checkpoint_task(
        task_pool,
        CheckpointConfig {
            interval: Duration::from_millis(2_500),
            ..Default::default()
        },
        None,
        shutdown_rx,
        true,
    ));
    tokio::time::timeout(Duration::from_secs(4), async {
        while routine_wal_observation(&pool).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the checkpoint task must record its initial sample");

    // A reader opened immediately after a full checkpoint may use WAL
    // slot 0 and cannot pin the next frame. Give it an uncheckpointed
    // frame to read before the writer appends another one.
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (1)", [])
            .unwrap();
    }
    let mut reader = ReaderProcess::spawn(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (2)", [])
            .unwrap();
    }

    let first = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let first_probe = first.checkpoint_probe.as_ref().expect("probe row");
    assert!(first_probe.checkpointed_frames > 0);
    assert_eq!(
        first.checkpoint_pin.backfill_ceiling,
        Some(first_probe.checkpointed_frames)
    );
    assert!(first_probe.log_frames > first_probe.checkpointed_frames);
    assert_eq!(first.checkpoint_pin.oldest_pinned_frame, None);
    assert_eq!(first.checkpoint_pin.pin_depth, None);
    assert!(first
        .checkpoint_pin
        .oldest_pinned_frame_unavailable_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("less than one second")));

    tokio::time::sleep(Duration::from_millis(1_010)).await;
    let aged = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let aged_probe = aged.checkpoint_probe.as_ref().expect("probe row");
    assert_eq!(
        aged.checkpoint_pin.backfill_ceiling,
        Some(aged_probe.checkpointed_frames)
    );
    assert_eq!(
        aged.checkpoint_pin.oldest_pinned_frame,
        Some(aged_probe.checkpointed_frames)
    );
    assert_eq!(
        aged.checkpoint_pin.pin_depth,
        Some(aged_probe.log_frames - aged_probe.checkpointed_frames)
    );
    let first_run = aged
        .checkpoint_pin
        .oldest_pinned_frame_run
        .expect("matching run accompanies the reported frame");
    assert_eq!(first_run.frame, aged_probe.checkpointed_frames);

    reader.release();
    let previous_tick = routine_wal_observation(&pool)
        .expect("the checkpoint task has an initial sample")
        .observed_at_unix_ms;
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if routine_wal_observation(&pool)
                .is_some_and(|sample| sample.observed_at_unix_ms > previous_tick)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a later checkpoint tick must fully backfill after the reader ends");
    let drained = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    assert_eq!(drained.checkpoint_pin.backfill_ceiling, None);
    assert!(drained
        .checkpoint_pin
        .backfill_ceiling_unavailable_reason
        .is_some());
    assert_eq!(drained.checkpoint_pin.oldest_pinned_frame, None);
    assert_eq!(drained.checkpoint_pin.pin_depth, None);
    assert_eq!(drained.checkpoint_pin.oldest_pinned_frame_run, None);

    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("INSERT INTO t VALUES (3); INSERT INTO t VALUES (4)")
            .unwrap();
    }
    let second_reader = rusqlite::Connection::open(&path).unwrap();
    second_reader.execute_batch("BEGIN DEFERRED").unwrap();
    second_reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
        .unwrap();
    let second_pin_started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (5)", [])
            .unwrap();
    }
    let second_first = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let second_first_probe = second_first.checkpoint_probe.as_ref().expect("probe row");
    assert!(second_first_probe.checkpointed_frames > 0);
    assert!(second_first_probe.log_frames > second_first_probe.checkpointed_frames);
    assert_eq!(second_first.checkpoint_pin.oldest_pinned_frame, None);
    tokio::time::sleep(Duration::from_millis(1_010)).await;
    let second_aged = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let second_aged_probe = second_aged.checkpoint_probe.as_ref().expect("probe row");
    let second_run = second_aged
        .checkpoint_pin
        .oldest_pinned_frame_run
        .expect("the second pin must begin a distinct observed run");
    assert_eq!(
        second_aged.checkpoint_pin.oldest_pinned_frame,
        Some(second_aged_probe.checkpointed_frames)
    );
    // A full backfill ended the earlier run above. WAL frame numbers may
    // restart, so distinctness is established by the new observation time.
    assert!(second_run.first_observed_at_unix_ms > first_run.first_observed_at_unix_ms);
    assert!(
        second_run.first_observed_at_unix_ms >= second_pin_started_at_unix_ms,
        "the new run begins after the second reader starts"
    );
    second_reader.execute_batch("ROLLBACK").unwrap();

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("checkpoint task shutdown must finish")
        .expect("checkpoint task must not panic");
}

include!("checkpoint/churn_tests.rs");

#[test]
fn db_diagnostics_reports_short_reader_backfill_ceiling_without_a_pin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("diagnostic_short_reader_gap.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER); INSERT INTO t VALUES (1)")
            .unwrap();
    }

    // The reader snapshots an uncheckpointed frame. A later commit must
    // remain beyond that snapshot when the diagnostic PASSIVE probe runs.
    // No checkpoint task races this fixture or can age the gap into a pin.
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN DEFERRED").unwrap();
    let visible_rows: i64 = reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows, 1);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (2)", [])
            .unwrap();
    }

    let report = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let probe = report.checkpoint_probe.as_ref().expect("probe row");
    assert_eq!(probe.busy, 0);
    assert!(probe.checkpointed_frames > 0);
    assert!(probe.log_frames > probe.checkpointed_frames);
    assert_eq!(
        report.checkpoint_pin.backfill_ceiling,
        Some(probe.checkpointed_frames)
    );
    assert_eq!(report.checkpoint_pin.oldest_pinned_frame, None);
    assert_eq!(report.checkpoint_pin.pin_depth, None);

    reader.execute_batch("ROLLBACK").unwrap();
    let drained = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    assert_eq!(drained.checkpoint_pin.backfill_ceiling, None);
}

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    message: Option<String>,
    open_tx_count: Option<u64>,
    oldest_tx_age_secs: Option<String>,
    elapsed_us: Option<u64>,
    busy: Option<i64>,
    backfill_gap_frames: Option<i64>,
    legacy_wal_pin_depth: Option<i64>,
    oldest_tx_label: Option<String>,
    tx_label: Option<String>,
    census_only: Option<String>,
}

#[derive(Default)]
struct CapturedEventVisitor(CapturedEvent);

impl Visit for CapturedEventVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "open_tx_count" => self.0.open_tx_count = Some(value),
            "elapsed_us" => self.0.elapsed_us = Some(value),
            _ => {}
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        match field.name() {
            "busy" => self.0.busy = Some(value),
            "backfill_gap_frames" => self.0.backfill_gap_frames = Some(value),
            "wal_pin_depth" => self.0.legacy_wal_pin_depth = Some(value),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.0.message = Some(value.to_string()),
            "oldest_tx_label" => self.0.oldest_tx_label = Some(value.to_string()),
            "tx_label" => self.0.tx_label = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let formatted = format!("{value:?}");
        let cleaned = formatted
            .trim_start_matches('"')
            .trim_end_matches('"')
            .to_string();
        match field.name() {
            "message" => self.0.message = Some(cleaned),
            "oldest_tx_label" => self.0.oldest_tx_label = Some(cleaned),
            "tx_label" => self.0.tx_label = Some(cleaned),
            "census_only" => self.0.census_only = Some(cleaned),
            "oldest_tx_age_secs" => self.0.oldest_tx_age_secs = Some(cleaned),
            _ => {}
        }
    }
}

/// Minimal `tracing::Subscriber` that captures events into a thread-local
/// vec, installed as the thread-local default for the duration of one
/// test closure via `tracing::subscriber::with_default`. Mirrors the
/// capture subscriber in `khive-runtime/src/pack.rs`'s gate-dispatch tests.
struct CaptureSubscriber {
    events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = CapturedEventVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Builds two `OldestSpan`s that differ ONLY in age, off one real
/// registration so the id and origin are the ones the tick would carry.
/// The ages straddle the threshold passed to the function under test.
fn spans_straddling(
    threshold: Duration,
) -> (
    khive_storage::tx_registry::OldestSpan,
    khive_storage::tx_registry::OldestSpan,
) {
    let _handle = khive_storage::tx_registry::register(Some("writer_task_tx".to_string()));
    let (id, _age, _label) = khive_storage::tx_registry::oldest().expect("a registration is open");
    let base = khive_storage::tx_registry::OldestSpan {
        id,
        age: Duration::ZERO,
        label: Some("writer_task_tx".to_string()),
        origin: khive_storage::tx_registry::TxOrigin::Unscoped,
    };
    let aged = khive_storage::tx_registry::OldestSpan {
        age: threshold + Duration::from_secs(1),
        ..base.clone()
    };
    let young = khive_storage::tx_registry::OldestSpan {
        // The capture that produced this fix: 5.8 ms, a writer.
        age: Duration::from_micros(5_849),
        ..base
    };
    (aged, young)
}

fn capture<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    tracing::subscriber::with_default(subscriber, f);
    let events = buffer.lock().unwrap();
    events.clone()
}

#[test]
fn truncate_no_progress_warn_reports_nonempty_registry_snapshot_facts() {
    let snapshot = [
        (Duration::from_secs(2), Some("younger-entry".to_string())),
        (Duration::from_secs(7), Some("older-entry".to_string())),
    ];
    let events = capture(|| log_truncate_no_progress_warn(6003, 6003, &snapshot));
    assert_eq!(events.len(), 3, "one summary and both captured entries");
    let summary = &events[0];
    assert_eq!(summary.open_tx_count, Some(2));
    assert_eq!(summary.oldest_tx_age_secs.as_deref(), Some("Some(7.0)"));
    let message = summary.message.as_deref().expect("summary message");
    assert_eq!(
        message,
        "WAL TRUNCATE attempt made no progress; open transactions observed in this process's registry"
    );
    assert!(!message.contains("pinning"));
    assert!(!message.contains("long-lived reader"));
    assert_eq!(events[1].tx_label.as_deref(), Some("younger-entry"));
    assert_eq!(events[2].tx_label.as_deref(), Some("older-entry"));
    assert!(events[1..].iter().all(|event| {
        event.message.as_deref() == Some("WAL high-water: open transaction registry entry")
    }));
}

#[test]
fn truncate_no_progress_warn_reports_empty_process_registry_without_pin_claim() {
    let events = capture(|| log_truncate_no_progress_warn(6003, 6003, &[]));
    assert_eq!(
        events.len(),
        1,
        "an empty snapshot has no entries to enumerate"
    );
    let summary = &events[0];
    assert_eq!(summary.open_tx_count, Some(0));
    assert_eq!(summary.oldest_tx_age_secs.as_deref(), Some("None"));
    let message = summary.message.as_deref().expect("summary message");
    assert_eq!(
        message,
        "WAL TRUNCATE attempt made no progress; no open transaction in this process's registry"
    );
    assert!(!message.contains("pinning"));
    assert!(!message.contains("long-lived reader"));
    let nonempty = capture(|| {
        log_truncate_no_progress_warn(6003, 6003, &[(Duration::ZERO, None)]);
    });
    assert_ne!(summary.message, nonempty[0].message);
    assert_eq!(nonempty[0].open_tx_count, Some(1));
    assert_eq!(nonempty[0].oldest_tx_age_secs.as_deref(), Some("Some(0.0)"));
}

/// An entry at or past the threshold: the WARN names it, its age and its
/// label. This is the branch the old unconditional text was right about.
#[test]
#[serial(tx_registry)]
fn high_water_warn_names_an_aged_registered_transaction_without_asserting_a_pin() {
    let threshold = Duration::from_secs(30);
    let (aged, _young) = spans_straddling(threshold);

    let events = capture(|| log_wal_high_water_warn(6003, 6000, Some(&aged), threshold));

    let message = events
        .iter()
        .find_map(|e| e.message.clone())
        .expect("one WARN is emitted");
    assert!(
        message.contains("in-process registered transaction is older")
            && message.contains("may hold a snapshot")
            && !message.contains("is pinning a snapshot"),
        "an aged entry is evidence of age, not proof of a pin: {message:?}"
    );
    assert_eq!(
        events.iter().find_map(|e| e.oldest_tx_label.clone()),
        Some("writer_task_tx".to_string()),
        "the named entry is the one handed in"
    );
}

/// A young in-process entry does not rule out a reader in another process.
#[test]
#[serial(tx_registry)]
fn high_water_warn_keeps_external_reader_and_write_rate_hypotheses_when_young() {
    let threshold = Duration::from_secs(30);
    let (aged, young) = spans_straddling(threshold);

    let young_message = capture(|| log_wal_high_water_warn(6003, 6000, Some(&young), threshold))
        .iter()
        .find_map(|e| e.message.clone())
        .expect("one WARN is emitted");
    let aged_message = capture(|| log_wal_high_water_warn(6003, 6000, Some(&aged), threshold))
        .iter()
        .find_map(|e| e.message.clone())
        .expect("one WARN is emitted");

    // One fixture cannot demonstrate a branch: the two must differ.
    assert_ne!(
        young_message, aged_message,
        "the two registry states must produce different text"
    );
    assert!(
        young_message.contains("no in-process transaction older than the age threshold")
            && young_message.contains("a reader in another process may hold the snapshot")
            && young_message.contains("writes may outpace PASSIVE checkpoints"),
        "got {young_message:?}"
    );
    assert!(
        !young_message.contains("is pinning a snapshot"),
        "the young branch must not assert a pin, got {young_message:?}"
    );
    assert!(
        !young_message.contains("long-lived reader"),
        "the young branch must not name a reader, got {young_message:?}"
    );
}

/// An empty local registry still cannot exclude an external reader.
#[test]
#[serial(tx_registry)]
fn high_water_warn_with_an_empty_registry_keeps_external_reader_visible() {
    let events = capture(|| log_wal_high_water_warn(6003, 6000, None, Duration::from_secs(30)));
    let message = events
        .iter()
        .find_map(|e| e.message.clone())
        .expect("one WARN is emitted");
    assert!(
        message.contains("no in-process transaction older than the age threshold")
            && message.contains("a reader in another process may hold the snapshot")
            && message.contains("writes may outpace PASSIVE checkpoints"),
        "got {message:?}"
    );
    assert_eq!(
        events.iter().find_map(|e| e.oldest_tx_label.clone()),
        Some("<none>".to_string()),
        "an absent entry is labelled as absent, never as unlabeled"
    );
}

/// `log_tx_registry_oldest_debug` names the oldest open registry entry.
/// See crates/khive-db/docs/api/checkpoint.md#log_tx_registry_oldest_debug_reports_oldest_open_entry
#[test]
#[serial(tx_registry)]
fn log_tx_registry_oldest_debug_reports_oldest_open_entry() {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };

    let _handle = khive_storage::tx_registry::register(Some("checkpoint_tick_test".to_string()));

    let oldest = khive_storage::tx_registry::oldest().map(|(id, age, label)| {
        khive_storage::tx_registry::OldestSpan {
            id,
            age,
            label,
            origin: khive_storage::tx_registry::TxOrigin::Unscoped,
        }
    });
    let expected_label = oldest
        .as_ref()
        .and_then(|s| s.label.clone())
        .unwrap_or_else(|| "<unlabeled>".to_string());

    tracing::subscriber::with_default(subscriber, || {
        log_tx_registry_oldest_debug(100, oldest.as_ref());
    });

    let events = buffer.lock().unwrap();
    assert!(
        events.iter().any(|e| {
            e.message.as_deref()
                == Some("WAL checkpoint tick: oldest open transaction registry entry")
                && e.oldest_tx_label.as_deref() == Some(expected_label.as_str())
        }),
        "expected a log line naming the open registry entry's label, got: {events:?}"
    );
}

/// ADR-091 Plank 0: the oldest-entry WARN and the
/// high-water snapshot-enumeration WARN are gated by `crossing_warn` at
/// the call site (mirroring the WAL-threshold WARNs), so driving two
/// consecutive above-threshold ticks through that same gate must produce
/// exactly one of each — never a repeat on the second tick.
#[test]
#[serial(tx_registry)]
fn registry_warns_fire_on_crossing_and_do_not_repeat() {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };

    let _handle =
        khive_storage::tx_registry::register(Some("registry_warn_crossing_test".to_string()));
    let oldest = khive_storage::tx_registry::oldest().map(|(id, age, label)| {
        khive_storage::tx_registry::OldestSpan {
            id,
            age,
            label,
            origin: khive_storage::tx_registry::TxOrigin::Unscoped,
        }
    });

    let mut was_above_warn = false;
    let mut was_above_high_water = false;

    tracing::subscriber::with_default(subscriber, || {
        // Tick 1: below→above crossing for both bands — both WARNs fire.
        if crossing_warn(true, &mut was_above_warn) {
            log_tx_registry_oldest_warn(6000, oldest.as_ref());
        }
        if crossing_warn(true, &mut was_above_high_water) {
            log_tx_registry_snapshot_warn(6000);
        }

        // Tick 2: still above both thresholds — neither must repeat.
        if crossing_warn(true, &mut was_above_warn) {
            log_tx_registry_oldest_warn(6000, oldest.as_ref());
        }
        if crossing_warn(true, &mut was_above_high_water) {
            log_tx_registry_snapshot_warn(6000);
        }
    });

    let events = buffer.lock().unwrap();

    // `tracing::subscriber::with_default` scopes capture to THIS thread for
    // the duration of the closure, so `events` contains only the two
    // `log_tx_registry_oldest_warn` calls made above — no concurrent test's
    // log calls land in this buffer. This lets the crossing/no-repeat
    // assertion match on message text alone: unlike the "names MY label"
    // assertion in the sibling test above, WHICH label `oldest()` reports
    // is irrelevant here (a concurrent write path elsewhere in the binary
    // may transiently be the registry's genuine oldest entry) — only the
    // fire-once-per-crossing COUNT is under test.
    let oldest_warn_count = events
        .iter()
        .filter(|e| {
            e.message.as_deref()
                == Some("WAL checkpoint tick: oldest open transaction registry entry")
        })
        .count();
    assert_eq!(
        oldest_warn_count, 1,
        "oldest-entry WARN must fire exactly once across two above-threshold ticks, got: {events:?}"
    );

    let snapshot_warn_count = events
        .iter()
        .filter(|e| {
            e.message.as_deref() == Some("WAL high-water: open transaction registry entry")
                && e.tx_label.as_deref() == Some("registry_warn_crossing_test")
        })
        .count();
    assert_eq!(
        snapshot_warn_count, 1,
        "high-water snapshot WARN must fire exactly once across two above-threshold ticks, got: {events:?}"
    );
}

/// ADR-091 Plank 1: `log_tx_age_emission` emits the correct message text
/// and carries the entry's label, for both the `Warn` and `Stale` rungs.
#[test]
fn log_tx_age_emission_carries_label_for_both_rungs() {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };

    tracing::subscriber::with_default(subscriber, || {
        log_tx_age_emission(&TxAgeEmission {
            rung: TxAgeRung::Warn,
            age: Duration::from_secs(45),
            label: Some("plank1_warn_test".to_string()),
        });
        log_tx_age_emission(&TxAgeEmission {
            rung: TxAgeRung::Stale,
            age: Duration::from_secs(150),
            label: Some("plank1_stale_test".to_string()),
        });
    });

    let events = buffer.lock().unwrap();
    assert!(
        events.iter().any(|e| {
            e.message.as_deref()
                == Some("ADR-091 Plank 1: open transaction registry entry exceeded soft-cap age")
                && e.tx_label.as_deref() == Some("plank1_warn_test")
        }),
        "expected a Warn-rung log line naming the entry, got: {events:?}"
    );
    assert!(
        events.iter().any(|e| {
            e.message.as_deref().is_some_and(|m| {
                m.starts_with(
                    "ADR-091 Plank 1: open transaction registry entry exceeded the cooperative",
                )
            }) && e.tx_label.as_deref() == Some("plank1_stale_test")
        }),
        "expected a Stale-rung log line naming the entry, got: {events:?}"
    );
}

pub(super) fn file_pool(path: &std::path::Path) -> Arc<ConnectionPool> {
    let cfg = PoolConfig {
        path: Some(path.to_path_buf()),
        ..PoolConfig::for_test()
    };
    Arc::new(ConnectionPool::new(cfg).expect("pool open"))
}

async fn writer_task_wal_autocheckpoint_pages(handle: &WriterTaskHandle) -> u32 {
    handle
        .send_top_level(|conn| {
            conn.pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, u32>(0))
                .map_err(|error| khive_storage::error::StorageError::Pool {
                    operation: "test_wal_autocheckpoint".into(),
                    message: error.to_string(),
                })
        })
        .await
        .expect("query writer-task connection pragma")
}

/// Test helper: open the same dedicated standalone connection
/// `run_checkpoint_task` opens in production, for tests that drive
/// `checkpoint_once` directly.
pub(super) fn checkpoint_conn(pool: &ConnectionPool) -> rusqlite::Connection {
    pool.open_standalone_writer()
        .expect("open dedicated checkpoint connection")
}

struct TruncateReportHookGuard;

impl Drop for TruncateReportHookGuard {
    fn drop(&mut self) {
        truncate_report_test_sync::uninstall();
    }
}

#[cfg(unix)]
struct WalpinAttributionHookGuard;

#[cfg(unix)]
impl Drop for WalpinAttributionHookGuard {
    fn drop(&mut self) {
        walpin_attribution_test_sync::uninstall();
    }
}

#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
#[serial(khive_walpin_sidecar_env)]
async fn diagnostic_legacy_forecast_matches_housekeeping_with_distinct_cadences() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let pool = file_pool(&root.path().join("forecast.db"));
    let path = pool.canonical_path().unwrap();
    let checkpoint_interval = Duration::from_millis(500);
    let session_interval = SessionSweepConfig::default().interval;
    assert_eq!(session_interval, Duration::from_secs(5));
    let state = TruncateState::default();
    assert_eq!(state.legacy_walpin_fallback_interval, session_interval);
    let sidecar = WalpinSidecarState::new(Some(path), true, "daemon", checkpoint_interval)
        .expect("enabled fixture sidecar");
    crate::walpin::ensure_sidecar_dir(&sidecar.dir).unwrap();
    let mut paths = Vec::new();
    for (pid, age) in [(2_000_000_001, 5), (2_000_000_002, 40)] {
        assert!(!crate::walpin::is_process_alive(pid));
        let temp = sidecar.dir.join(format!(".{pid}.beacon.tmp"));
        std::fs::write(
            &temp,
            serde_json::to_vec(&serde_json::json!({
                "pid": pid, "process_role": "session", "started_at": 1
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&temp)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(age))
            .unwrap();
        paths.push(temp);
    }
    let fast = crate::walpin::inspect_live(&sidecar.dir, checkpoint_interval).unwrap();
    assert_eq!(
        fast.cleanup_would_reap, 2,
        "control must distinguish the cadences"
    );
    let forecast = crate::diagnostics::wal_pin_attribution(path, session_interval);
    assert_eq!(forecast.sidecar_listing_truncated, Some(false));
    assert_eq!(forecast.sidecar_entries_cleanup_would_reap, Some(1));
    assert!(
        paths.iter().all(|path| path.exists()),
        "inspection retains evidence"
    );

    let cleanup = sidecar
        .reap_dead_entries_bounded(state.legacy_walpin_fallback_interval)
        .await
        .expect("housekeeping report");
    assert_eq!(
        Some(cleanup.orphan_temps_reaped),
        forecast.sidecar_entries_cleanup_would_reap
    );
    assert!(paths[0].exists(), "the temp inside the 15s window remains");
    assert!(!paths[1].exists(), "the trusted older temp is reaped");
}

#[test]
#[cfg(unix)]
fn walpin_full_scan_cadence_refreshes_first_then_reuses_until_boundary() {
    let cadence = Duration::from_secs(30);
    let started_at = Instant::now();
    let mut state = TruncateState::with_walpin_full_scan_cadence(cadence);

    assert!(matches!(
        state.plan_walpin_attribution_at(started_at),
        WalpinFullScanPlan::Refresh { .. }
    ));
    state.cache_walpin_attribution(
        crate::walpin::WalpinReport::default(),
        Ok(crate::walpin::CensusResult::default()),
        started_at,
    );

    assert!(matches!(
        state.plan_walpin_attribution_at(started_at + cadence - Duration::from_nanos(1)),
        WalpinFullScanPlan::Cached(_)
    ));
    assert!(matches!(
        state.plan_walpin_attribution_at(started_at + cadence),
        WalpinFullScanPlan::Refresh { .. }
    ));
}

#[test]
#[cfg(unix)]
fn walpin_full_scan_failure_retries_only_after_cadence() {
    let cadence = Duration::from_secs(30);
    let started_at = Instant::now();
    let mut state = TruncateState::with_walpin_full_scan_cadence(cadence);

    assert!(matches!(
        state.plan_walpin_attribution_at(started_at),
        WalpinFullScanPlan::Refresh { .. }
    ));
    // No cache update models either an enumeration error or a panicked
    // blocking worker. The attempt itself still owns the cadence slot.
    assert!(matches!(
        state.plan_walpin_attribution_at(started_at + cadence - Duration::from_nanos(1)),
        WalpinFullScanPlan::Suppressed
    ));
    assert!(matches!(
        state.plan_walpin_attribution_at(started_at + cadence),
        WalpinFullScanPlan::Refresh { .. }
    ));
}

#[test]
#[cfg(unix)]
fn cached_walpin_report_is_diagnostic_only_even_when_fully_attributed() {
    let report = crate::walpin::WalpinReport::default();
    assert!(
        report.fully_attributed(),
        "the fixture must otherwise license the sharp conclusion"
    );
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };

    tracing::subscriber::with_default(subscriber, || {
        log_walpin_sidecar_report(
            &report,
            Ok(crate::walpin::CensusResult::default()),
            WalpinReportFreshness::Cached {
                age: Duration::from_secs(1),
            },
        );
    });

    let events = buffer.lock().unwrap();
    assert!(
        events.iter().any(|event| {
            event.message.as_deref()
                == Some(
                    "cached WAL-pin attribution is diagnostic-only; fully-attributed \
                         conclusion is not licensed",
                )
        }),
        "cached attribution must declare its fail-closed status: {events:?}"
    );
    assert!(
        !events.iter().any(|event| {
            event.message.as_deref().is_some_and(|message| {
                message.starts_with("ADR-091 Amendment 2 Plank B: every live PID is reporting")
            })
        }),
        "cached attribution must never authorize the fully-attributed conclusion: {events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn progressing_truncate_releases_full_scan_reservation_to_housekeeping() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("walpin-progress-reservation.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }
    let conn = checkpoint_conn(&pool);
    let mut state = TruncateState::default();
    let config = CheckpointConfig {
        truncate_high_water_pages: 0,
        truncate_min_interval: Duration::ZERO,
        ..CheckpointConfig::default()
    };

    assert!(
        maybe_truncate(&pool, &conn, &config, u64::MAX, &mut state).is_none(),
        "a progressing TRUNCATE must not schedule no-progress attribution"
    );
    assert!(
        state.housekeeping_due(),
        "unused pre-TRUNCATE reservation must be restored before housekeeping"
    );
    let sidecar = WalpinSidecarState::new(pool.canonical_path(), true, "daemon", config.interval)
        .expect("file-backed test sidecar");
    assert!(
        run_walpin_housekeeping_if_due(&sidecar, &mut state, DEFAULT_SESSION_SWEEP_INTERVAL,).await,
        "the production housekeeping arm must consume one full scan"
    );
    assert!(state.walpin_cached_attribution.is_some());
}

#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn erroring_truncate_releases_full_scan_reservation_to_housekeeping() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("walpin-error-reservation.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }
    let conn = checkpoint_conn(&pool);
    conn.authorizer(Some(
        |context: rusqlite::hooks::AuthContext<'_>| match context.action {
            AuthAction::Pragma { pragma_name, .. }
                if pragma_name.eq_ignore_ascii_case("wal_checkpoint") =>
            {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        },
    ))
    .unwrap();
    let mut state = TruncateState::default();
    let config = CheckpointConfig {
        truncate_high_water_pages: 0,
        truncate_min_interval: Duration::ZERO,
        ..CheckpointConfig::default()
    };

    assert!(
        maybe_truncate(&pool, &conn, &config, u64::MAX, &mut state).is_none(),
        "an erroring TRUNCATE must not schedule no-progress attribution"
    );
    conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(
        state.housekeeping_due(),
        "failed TRUNCATE must restore its unused full-scan reservation"
    );
    let sidecar = WalpinSidecarState::new(pool.canonical_path(), true, "daemon", config.interval)
        .expect("file-backed test sidecar");
    assert!(
        run_walpin_housekeeping_if_due(&sidecar, &mut state, DEFAULT_SESSION_SWEEP_INTERVAL,).await,
        "the production housekeeping arm must consume one full scan"
    );
    assert!(state.walpin_cached_attribution.is_some());
}

struct ReaderProcess {
    child: std::process::Child,
    _stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl ReaderProcess {
    fn spawn(db_path: &std::path::Path) -> Self {
        use std::io::BufRead;
        use std::process::Stdio;

        let mut child = std::process::Command::new(
            std::env::current_exe().expect("resolve current test executable"),
        )
        .args([
            "--exact",
            "checkpoint::tests::walpin_transient_reader_process_helper",
            "--nocapture",
        ])
        .env("KHIVE_CHECKPOINT_READER_HELPER_PATH", db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn transient WAL reader helper");

        let stdout = child.stdout.take().expect("capture helper stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .expect("read transient reader readiness signal");
            assert!(bytes > 0, "reader helper exited before readiness signal");
            if line.contains("KHIVE_CHECKPOINT_READER_READY") {
                break;
            }
        }
        Self {
            child,
            _stdout: reader,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn release(&mut self) {
        use std::io::Write;

        let mut stdin = self.child.stdin.take().expect("helper stdin is available");
        stdin
            .write_all(b"release\n")
            .expect("release transient reader");
        drop(stdin);
        let status = self.child.wait().expect("wait for transient reader helper");
        assert!(status.success(), "transient reader helper failed: {status}");
    }
}

impl Drop for ReaderProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn walpin_transient_reader_process_helper() {
    use std::io::Write;

    let Some(path) = std::env::var_os("KHIVE_CHECKPOINT_READER_HELPER_PATH") else {
        return;
    };
    let conn = rusqlite::Connection::open(path).expect("helper opens database");
    conn.execute_batch("BEGIN DEFERRED; SELECT * FROM t;")
        .expect("helper pins a read snapshot");
    println!("KHIVE_CHECKPOINT_READER_READY");
    std::io::stdout().flush().expect("flush readiness signal");
    let mut release = String::new();
    std::io::stdin()
        .read_line(&mut release)
        .expect("wait for release signal");
    conn.execute_batch("COMMIT")
        .expect("helper releases read snapshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
#[serial(checkpoint_skip_metrics, khive_walpin_census_budget_env)]
async fn db_diagnostics_keeps_a_holder_started_after_the_run_observation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("post_observation_holder.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER); INSERT INTO t VALUES (0)")
            .unwrap();
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        CheckpointConfig {
            interval: Duration::from_secs(60),
            ..Default::default()
        },
        None,
        shutdown_rx,
        true,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while routine_wal_observation(&pool).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the checkpoint task must record its initial sample");

    let earlier_reader = rusqlite::Connection::open(&path).unwrap();
    earlier_reader.execute_batch("BEGIN DEFERRED").unwrap();
    earlier_reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
        .unwrap();
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute("INSERT INTO t VALUES (1)", [])
            .unwrap();
    }

    let first = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let first_probe = first.checkpoint_probe.as_ref().expect("probe row");
    assert!(first_probe.log_frames > first_probe.checkpointed_frames);
    let run = checkpoint_run_status(&pool);
    let first_observed_at_unix_ms = match run {
        CheckpointRunStatus::Observed(run) => run.first_observed_at_unix_ms,
        other => panic!("diagnostic probe must establish a checkpoint run: {other:?}"),
    };

    let mut later_reader = ReaderProcess::spawn(&path);
    let later_pid = later_reader.pid();
    let report = crate::diagnostics::collect(
        &pool,
        crate::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let later_start = report
        .wal_pin
        .census_process_start_times
        .iter()
        .find(|process| process.pid == later_pid)
        .expect("a post-observation holder must remain in the census");
    assert_eq!(report.wal_pin.reporting_process_is_holder, Some(true));
    assert!(
        report.wal_pin.census_holder_pids.contains(&later_pid),
        "the later process remains a confirmed holder"
    );
    assert_eq!(
        later_start.process_start_time_secs,
        crate::walpin::process_start_time_secs(later_pid)
    );
    assert!(later_start.process_start_time_secs.is_some());
    assert_eq!(later_start.process_start_time_unavailable_reason, None);
    #[cfg(target_os = "linux")]
    assert_eq!(report.wal_pin.start_time_resolution_secs, Some(2));
    #[cfg(target_os = "macos")]
    assert_eq!(report.wal_pin.start_time_resolution_secs, Some(1));
    assert!(first_observed_at_unix_ms > 0);
    assert!(!report
        .wal_pin
        .census_process_start_times
        .iter()
        .any(|process| process.pid == later_pid && process.process_start_time_secs.is_none()));

    later_reader.release();
    earlier_reader.execute_batch("ROLLBACK").unwrap();
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("checkpoint task shutdown must finish")
        .expect("checkpoint task must not panic");
}

#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
#[serial(
    checkpoint_skip_metrics,
    khive_walpin_sidecar_env,
    walpin_attribution_async,
    walpin_report_seam
)]
async fn no_progress_report_keeps_holder_released_after_truncate_timeout() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("transient-reader.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .expect("seed WAL before reader snapshot");
    }

    let mut reader = ReaderProcess::spawn(&path);
    let reader_pid = reader.pid();
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch("INSERT INTO t VALUES (2);")
            .expect("append WAL behind reader snapshot");
    }

    let canonical_path = pool
        .canonical_path()
        .expect("file-backed pool has canonical path")
        .to_path_buf();
    let (reached_rx, proceed_tx) = truncate_report_test_sync::install(canonical_path.clone());
    let _hook_guard = TruncateReportHookGuard;
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let checkpoint_pool = Arc::clone(&pool);
    let dedicated_conn = checkpoint_conn(&checkpoint_pool);
    let checkpoint_events = Arc::clone(&buffer);
    let checkpoint = std::thread::spawn(move || {
        let subscriber = CaptureSubscriber {
            events: checkpoint_events,
        };
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let mut state = TruncateState::default();
        let result = checkpoint_once_core(
            &checkpoint_pool,
            &dedicated_conn,
            &CheckpointConfig {
                truncate_high_water_pages: 0,
                truncate_min_interval: Duration::ZERO,
                truncate_busy_timeout: Duration::from_millis(50),
                ..CheckpointConfig::default()
            },
            &mut state,
        );
        (result, state)
    });

    reached_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("TRUNCATE must report no progress while the reader is pinned");
    reader.release();
    let post_attempt_census =
        crate::walpin::census_holders(&canonical_path).expect("post-attempt holder census");
    assert!(
        !post_attempt_census.holders.contains(&reader_pid),
        "released reader PID must be absent from a post-attempt census"
    );
    proceed_tx
        .send(())
        .expect("allow no-progress reporting to continue");
    let (checkpoint_result, mut state) = checkpoint.join().expect("checkpoint thread");
    let outcome = checkpoint_result.expect("checkpoint succeeds");
    assert!(
        outcome.sidecar_attribution.is_some(),
        "the synchronous checkpoint result must carry a separate attribution request"
    );
    assert!(
        !state.sidecar_attribution_attempted_this_tick,
        "capturing a request is not the same as attempting its directory enumeration"
    );

    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let attribution_attempted =
        complete_walpin_attribution(outcome.sidecar_attribution, &mut state)
            .await
            .expect("deferred attribution succeeds");
    assert!(
        attribution_attempted,
        "a no-progress attribution pass must suppress the redundant healthy-housekeeping \
             pass for the same tick"
    );

    let events = buffer.lock().expect("captured events");
    let summaries: Vec<_> = events
        .iter()
        .filter(|event| {
            event.message.as_deref().is_some_and(|message| {
                message.starts_with("WAL TRUNCATE attempt made no progress;")
            })
        })
        .collect();
    assert_eq!(
        summaries.len(),
        1,
        "the real no-progress path emits one summary"
    );
    let summary = summaries[0];
    assert!(summary.open_tx_count.is_some());
    assert!(summary.oldest_tx_age_secs.is_some());
    let message = summary.message.as_deref().expect("summary message");
    assert!(message.contains("in this process's registry"));
    assert!(!message.contains("pinning"));
    assert!(!message.contains("long-lived reader"));
    assert!(
        events.iter().any(|event| {
            event
                .census_only
                .as_deref()
                .is_some_and(|pids| pids.contains(&reader_pid.to_string()))
        }),
        "the no-progress report must retain PID {reader_pid} from the pre-attempt census: {events:?}"
    );
}

/// The 512-entry attribution walk must run on Tokio's blocking pool and
/// the async owner must await it before the report is consumed. A paused
/// real enumeration proves all three facts without a timing sleep: its
/// thread differs from the current-thread runtime, the completion future
/// remains pending, and the report-use counter stays zero until release.
#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
#[serial(walpin_attribution_async)]
async fn no_progress_attribution_is_off_runtime_and_awaited_before_report_use() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().expect("tempdir");
    let sidecar_dir = dir.path().join("checkpoint.db.walpin");
    let (reached_rx, proceed_tx, report_counter) =
        walpin_attribution_test_sync::install_pause(sidecar_dir.clone());
    let _hook_guard = WalpinAttributionHookGuard;

    let runtime_thread = std::thread::current().id();
    let mut state = TruncateState::default();
    let request = Some(WalpinAttributionRequest::Fresh {
        dir: sidecar_dir,
        census: Ok(crate::walpin::CensusResult::default()),
        legacy_fallback_interval: DEFAULT_SESSION_SWEEP_INTERVAL,
        previous_last_attempt: None,
    });
    let completion = tokio::spawn(async move {
        let result = complete_walpin_attribution(request, &mut state).await;
        (result, state)
    });

    let blocking_thread = reached_rx
        .await
        .expect("spawn_blocking attribution reached test seam");
    assert_ne!(
        blocking_thread, runtime_thread,
        "sidecar enumeration must not execute on the current-thread Tokio runtime worker"
    );
    assert!(
        !completion.is_finished(),
        "the async attribution owner must await the still-paused blocking enumeration"
    );
    assert_eq!(
        report_counter.load(Ordering::SeqCst),
        0,
        "the attribution report must not be consumed before enumeration completes"
    );

    proceed_tx
        .send(())
        .expect("release blocking attribution enumeration");
    let (result, state) = completion.await.expect("attribution task joins");
    assert_eq!(result, Ok(true));
    assert_eq!(
        report_counter.load(Ordering::SeqCst),
        1,
        "the completed enumeration must feed exactly one report use"
    );
    assert!(state.sidecar_attribution_attempted_this_tick);
    assert!(
        !state.housekeeping_due(),
        "completed attribution must suppress same-tick housekeeping"
    );
}

/// A panicked blocking worker is not flattened into success. The tick is
/// still marked attempted because the worker may have partially walked
/// the directory before failing, so starting housekeeping afterward
/// would violate the one-pass bound.
#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
#[serial(walpin_attribution_async)]
async fn no_progress_attribution_join_failure_is_honest_and_suppresses_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sidecar_dir = dir.path().join("checkpoint.db.walpin");
    walpin_attribution_test_sync::install_panic(sidecar_dir.clone());
    let _hook_guard = WalpinAttributionHookGuard;

    let mut state = TruncateState::default();
    let request = Some(WalpinAttributionRequest::Fresh {
        dir: sidecar_dir,
        census: Ok(crate::walpin::CensusResult::default()),
        legacy_fallback_interval: DEFAULT_SESSION_SWEEP_INTERVAL,
        previous_last_attempt: None,
    });

    let error = complete_walpin_attribution(request, &mut state)
        .await
        .expect_err("injected worker panic must surface as failure");
    assert!(
        matches!(error, WalpinAttributionFailure::Worker(_)),
        "join failure must retain its worker classification: {error:?}"
    );
    assert!(state.sidecar_attribution_attempted_this_tick);
    assert!(
        !state.housekeeping_due(),
        "an indeterminate partial pass must not authorize a second scan"
    );
}

/// Structural guard for the accepted ADR split: the synchronous SQLite
/// checkpoint core only schedules attribution, the sole direct
/// `enumerate_live` call is nested in an awaited `spawn_blocking`, and the
/// task completes it before either housekeeping or lifecycle outcome use.
#[test]
#[cfg(unix)]
#[serial(checkpoint_skip_metrics)]
fn async_checkpoint_source_keeps_enumeration_behind_awaited_spawn_blocking() {
    fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        source
            .split_once(start)
            .unwrap_or_else(|| panic!("missing source marker {start:?}"))
            .1
            .split_once(end)
            .unwrap_or_else(|| panic!("missing source marker {end:?}"))
            .0
    }

    let source = include_str!("checkpoint.rs");
    let checkpoint_core = section(
        source,
        "fn checkpoint_once_core(",
        "/// Evaluate and, if due, attempt a TRUNCATE escalation",
    );
    let truncate_core = section(
        source,
        "fn maybe_truncate(",
        "#[cfg(test)]\nmod truncate_report_test_sync",
    );
    let report_logger = section(
        source,
        "fn log_walpin_sidecar_report(",
        "/// ADR-091 Amendment 2 Plank C",
    );
    for (name, body) in [
        ("checkpoint_once_core", checkpoint_core),
        ("maybe_truncate", truncate_core),
        ("log_walpin_sidecar_report", report_logger),
    ] {
        assert!(
            !body.contains("enumerate_live("),
            "{name} must not perform direct sidecar enumeration"
        );
    }

    let async_completion = section(
        source,
        "async fn complete_walpin_attribution(",
        "/// When a TRUNCATE attempt makes no progress",
    );
    let spawn = async_completion
        .find("tokio::task::spawn_blocking")
        .expect("completion must spawn blocking work");
    let enumerate = async_completion
        .find("crate::walpin::enumerate_live")
        .expect("blocking closure must perform the attribution enumeration");
    let awaited = async_completion[enumerate..]
        .find(".await")
        .map(|offset| enumerate + offset)
        .expect("blocking worker must be awaited");
    assert!(spawn < enumerate && enumerate < awaited);

    let task = section(
        source,
        "pub async fn run_checkpoint_task(",
        "/// Whether a `CheckpointOutcomeRecorded` transition should be enqueued",
    );
    let checkpoint = task
        .find("run_checkpoint_core_off_worker(")
        .expect("checkpoint core call");
    let completion = task
        .find("complete_walpin_attribution(")
        .expect("awaited attribution completion");
    let housekeeping = task
        .find("run_walpin_housekeeping_if_due(")
        .expect("fallback housekeeping");
    let outcome = task
        .find("observe_checkpoint_pressure_tick(")
        .expect("lifecycle outcome use");
    assert!(
        checkpoint < completion && completion < housekeeping && housekeeping < outcome,
        "tick ordering must be checkpoint -> awaited attribution -> housekeeping decision -> outcome"
    );
    let housekeeping_helper = section(
        source,
        "async fn run_walpin_housekeeping_if_due(",
        "fn now_epoch_secs()",
    );
    assert!(
        housekeeping_helper.contains("reap_dead_entries_bounded(legacy_fallback_interval)"),
        "the ordered housekeeping arm must retain the bounded full scan"
    );

    // The outcome decision itself moved into the extracted per-tick
    // helper; the emit gate must still be consulted there, so the
    // ordering assertion above remains transitively about the same
    // lifecycle decision it always pinned.
    let pressure_tick = section(
        source,
        "fn observe_checkpoint_pressure_tick(",
        "/// ADR-091 Plank 0",
    );
    assert!(
        pressure_tick.contains("checkpoint_outcome_should_emit"),
        "extracted pressure tick helper must gate on the lifecycle emit decision"
    );
}

/// `run_fts_maintenance_off_worker` moves the same `run_if_due` call onto
/// a blocking thread; driven on a genuinely multi-threaded runtime
/// against a fragmented fixture, it must reach the same step outcome as
/// calling `run_if_due` directly on an identically-seeded fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_maintenance_off_worker_matches_the_direct_call() {
    fn fragmented_fixture() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fts-off-worker.db");
        let conn = rusqlite::Connection::open(&path).expect("open sqlite");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE fts_entities USING fts5(namespace UNINDEXED, subject_id UNINDEXED, title, body, tokenize='trigram');
                 CREATE VIRTUAL TABLE fts_notes USING fts5(namespace UNINDEXED, subject_id UNINDEXED, title, body, tokenize='trigram');
                 INSERT INTO fts_entities(fts_entities, rank) VALUES('automerge', 0);
                 INSERT INTO fts_notes(fts_notes, rank) VALUES('automerge', 0);",
        )
        .expect("create FTS fixtures");
        // Round-robin starts at table index 0 (`fts_entities`); fragment
        // that one so the very first due call has real merge work to do.
        for index in 0..80 {
            let body = format!(
                "segment fixture {index} keeps enough repeated production recall text to span pages {}",
                "memory query corpus ".repeat(40)
            );
            conn.execute(
                "INSERT INTO fts_entities(namespace, subject_id, title, body) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "local",
                    format!("id-{index}"),
                    format!("title {index}"),
                    body
                ],
            )
            .expect("one autocommit FTS write");
        }
        (dir, conn)
    }

    let config = crate::fts_maintenance::FtsMaintenanceConfig {
        enabled: true,
        interval: Duration::ZERO,
        merge_pages: 8,
        minimum_segments: 2,
    };

    let (_direct_dir, direct_conn) = fragmented_fixture();
    let mut direct_state = crate::fts_maintenance::FtsMaintenanceState::new(Instant::now());
    let direct_step = crate::fts_maintenance::run_if_due(
        &direct_conn,
        &config,
        &mut direct_state,
        Instant::now(),
    )
    .expect("direct maintenance step")
    .expect("fragmented fixture has a due step");

    let (_wrapped_dir, wrapped_conn) = fragmented_fixture();
    let wrapped_state = crate::fts_maintenance::FtsMaintenanceState::new(Instant::now());
    let (_conn, _state, wrapped_result) =
        run_fts_maintenance_off_worker(wrapped_conn, config, wrapped_state, Instant::now())
            .await
            .expect("maintenance step did not panic");
    let wrapped_step = wrapped_result
        .expect("wrapped maintenance step")
        .expect("fragmented fixture has a due step");

    assert_eq!(
        direct_step, wrapped_step,
        "the spawn_blocking wrapper must produce the same step outcome as calling \
             run_if_due directly"
    );
    assert_eq!(direct_step.table, "fts_entities");
}

// `checkpoint_once` -> `query_wal_pages` writes the process-wide
// `LAST_WAL_PAGES` gauge and resets `CHECKPOINT_CONSECUTIVE_SKIPS`
// (see the reset-discipline comment on `reset_checkpoint_metrics_for_tests`
// above) — this must join the `checkpoint_skip_metrics` group so it can
// never interleave with a test asserting on those same gauges.
#[test]
#[serial(checkpoint_skip_metrics)]
fn checkpoint_once_succeeds_on_file_backed_pool() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal_test.db");
    let pool = file_pool(&path);

    // Create a table so the DB is not completely empty.
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);")
            .unwrap();
        writer
            .conn()
            .execute_batch("INSERT INTO t VALUES (1);")
            .unwrap();
    }

    let conn = checkpoint_conn(&pool);
    checkpoint_once(
        &pool,
        &conn,
        &CheckpointConfig::default(),
        &mut TruncateState::default(),
    )
    .expect("checkpoint_once must succeed against a healthy dedicated connection");
}

/// In-memory pools have no on-disk file to open a second, dedicated
/// standalone connection against — this is exactly the precondition that
/// makes `CheckpointConnection::ensure_open` return `None` and
/// `run_checkpoint_task` report the tick `Skipped`, so `checkpoint_once`
/// is never even called for one.
#[test]
fn open_standalone_writer_fails_on_in_memory_pool() {
    let cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(cfg).expect("in-memory pool"));
    assert!(
        pool.open_standalone_writer().is_err(),
        "an in-memory pool must not be able to open a dedicated checkpoint connection"
    );
}

/// `CheckpointConnection::ensure_open` must open its dedicated connection
/// through the untracked standalone boundary, both on the initial open
/// and on a reopen after the connection is dropped — the checkpoint task
/// is exempt infrastructure, not a request-traffic writer acquisition
/// (ADR-136 D1 gate 5).
#[test]
fn ensure_open_does_not_move_writer_acquisition_counters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint_ensure_open.db");
    let pool = file_pool(&path);

    let before_first_open = pool.writer_acquisition_snapshot();
    let mut checkpoint_conn = CheckpointConnection::new();
    checkpoint_conn
        .ensure_open(&pool)
        .expect("dedicated checkpoint connection must open against a file-backed pool");
    assert_eq!(
        pool.writer_acquisition_snapshot(),
        before_first_open,
        "the checkpoint connection's initial open must not count as a writer acquisition"
    );

    checkpoint_conn.conn = None;
    let before_reopen = pool.writer_acquisition_snapshot();
    checkpoint_conn
        .ensure_open(&pool)
        .expect("dedicated checkpoint connection must reopen after invalidation");
    assert_eq!(
        pool.writer_acquisition_snapshot(),
        before_reopen,
        "reopening the checkpoint connection must not count as a writer acquisition either"
    );
}

#[test]
fn checkpoint_connection_disables_wal_autocheckpoint_on_open_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint_autocheckpoint.db");
    let pool = file_pool(&path);
    let mut checkpoint_conn = CheckpointConnection::new();

    let initial: u32 = checkpoint_conn
        .ensure_open(&pool)
        .expect("dedicated checkpoint connection must open")
        .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
        .expect("read initial autocheckpoint setting");
    assert_eq!(initial, 0);

    checkpoint_conn.conn = None;
    let reopened: u32 = checkpoint_conn
        .ensure_open(&pool)
        .expect("dedicated checkpoint connection must reopen")
        .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
        .expect("read reopened autocheckpoint setting");
    assert_eq!(reopened, 0);
}

#[tokio::test(flavor = "current_thread")]
#[serial(checkpoint_skip_metrics)]
async fn failed_checkpoint_claim_keeps_existing_writer_task_on_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failed_claim_writer_task.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            checkout_timeout: Duration::from_millis(1),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("pool open"),
    );
    let writer_task = pool
        .writer_task_handle()
        .expect("writer-task resolution")
        .expect("writer task enabled");
    assert_eq!(
        writer_task_wal_autocheckpoint_pages(&writer_task).await,
        crate::pool::FALLBACK_WAL_AUTOCHECKPOINT_PAGES
    );

    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder_pool = Arc::clone(&pool);
    let holder = tokio::task::spawn_blocking(move || {
        let _held_writer = holder_pool.try_checkpoint_nowait().expect("claim writer");
        held_tx.send(()).expect("signal held pooled writer");
        release_rx.recv().expect("release held pooled writer");
    });
    held_rx.await.expect("pooled writer holder started");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    drop(shutdown_tx);
    run_checkpoint_task(
        Arc::clone(&pool),
        CheckpointConfig {
            interval: Duration::from_secs(60),
            ..CheckpointConfig::default()
        },
        None,
        shutdown_rx,
        true,
    )
    .await;

    assert_eq!(pool.writer_acquisition_snapshot().timeouts, 1);
    assert_eq!(
        writer_task_wal_autocheckpoint_pages(&writer_task).await,
        crate::pool::FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
        "failed pooled-writer claim must not partially propagate ownership"
    );
    release_tx.send(()).expect("release pooled writer");
    holder.await.expect("pooled writer holder joined");
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_task_exits_on_shutdown_signal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal_task_shutdown.db");
    let pool = file_pool(&path);

    // Use a very short interval so the task ticks quickly in the test.
    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        ..Default::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

    shutdown_tx.send(()).expect("send shutdown signal");

    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");
}

#[cfg(unix)]
#[tokio::test]
#[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn healthy_checkpoint_tick_reaps_a_dead_walpin_beacon_without_truncate() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("healthy_sidecar_reap.db");
    let pool = file_pool(&path);
    let sidecar_dir =
        crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed pool"));

    let dead_pid = 2_000_000_000;
    let dead_beacon = crate::walpin::WalpinBeacon {
        pid: dead_pid,
        process_role: "session".to_string(),
        started_at: 1,
        sweep_interval_ms: 5_000,
    };
    crate::walpin::write_beacon(&sidecar_dir, &dead_beacon)
        .expect("seed a crashed process's orphan beacon");
    let dead_beacon_path = crate::walpin::beacon_path(&sidecar_dir, dead_pid);
    assert!(dead_beacon_path.exists(), "orphan fixture must exist");

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: u64::MAX,
        high_water_pages: u64::MAX,
        truncate_high_water_pages: u64::MAX,
        ..CheckpointConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

    let reaped = wait_for(Duration::from_secs(2), || !dead_beacon_path.exists()).await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    assert!(
        reaped,
        "the ordinary healthy tick must reap positively dead sidecar residue independently of \
             TRUNCATE diagnostics"
    );
}

/// Regression #774: exits via watch-signal even with a live event_store
/// pool clone (rules out a strong-count-based exit condition). See
/// crates/khive-db/docs/api/checkpoint.md#checkpoint_task_exits_via_shutdown_signal_with_live_event_store_pool_clone
#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_task_exits_via_shutdown_signal_with_live_event_store_pool_clone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal_task_event_store.db");
    let pool = file_pool(&path);

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        ..Default::default()
    };

    let event_store: Arc<dyn khive_storage::EventStore> =
        Arc::new(crate::stores::event::SqlEventStore::new_scoped(
            Arc::clone(&pool),
            true,
            "local".to_string(),
        ));
    // A second, independent sibling clone of `pool` outlives this test
    // function's own binding — mirrors `StorageBackend` retaining
    // `self.pool` alongside the `SqlEventStore` it hands to the
    // checkpoint task in production.
    let sibling_pool_clone = Arc::clone(&pool);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        pool,
        cfg,
        Some(CheckpointLifecycleOwner::new(event_store, "local")),
        shutdown_rx,
        true,
    ));

    // Confirm strong_count is well above 1 — the old check would spin
    // forever here — before proving the new signal-based exit works
    // regardless.
    assert!(
        Arc::strong_count(&sibling_pool_clone) > 1,
        "test setup must reproduce the multi-owner shape the bug depends on"
    );

    shutdown_tx.send(()).expect("send shutdown signal");

    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect(
            "checkpoint task should exit within 1s via the watch signal, \
                 even with a live sibling Arc<ConnectionPool> clone held by \
                 the event store",
        )
        .expect("checkpoint task panicked");
}

/// Regression: a high-water tick must NOT block behind an active read
/// transaction (isomorphism guarantee — fails if `checkpoint_once`
/// regresses to TRUNCATE). See
/// crates/khive-db/docs/api/checkpoint.md#checkpoint_high_water_does_not_block_behind_reader
#[test]
#[serial(checkpoint_skip_metrics)]
fn checkpoint_high_water_does_not_block_behind_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("high_water_test.db");

    // busy_timeout = 2000ms: a TRUNCATE regression blocks ~2s, while the
    // assertion below uses the midpoint of that gap instead of a noise-floor
    // threshold. The checkpoint config carries the same 2000ms default.
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            busy_timeout: Duration::from_millis(2000),
            ..PoolConfig::for_test()
        })
        .expect("pool open"),
    );

    // Write data so the WAL has frames to checkpoint.
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    // Open a reader and start a real read transaction so it holds a WAL
    // snapshot. An idle connection (no BEGIN) does NOT pin frames and would
    // not cause TRUNCATE to wait — the transaction is required for isomorphism.
    let reader = pool.reader().expect("reader");
    reader
        .conn()
        .execute_batch("BEGIN DEFERRED; SELECT * FROM t;")
        .expect("begin read tx");

    // Write another row AFTER the snapshot is established. These new WAL
    // frames are now pinned by the open reader snapshot — TRUNCATE cannot
    // reclaim them without waiting; PASSIVE skips them and returns immediately.
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("INSERT INTO t VALUES (2);")
            .unwrap();
    }

    let checkpoint_config = CheckpointConfig::default();
    let conn = checkpoint_conn(&pool);
    let start = std::time::Instant::now();
    checkpoint_once(
        &pool,
        &conn,
        &checkpoint_config,
        &mut TruncateState::default(),
    )
    .expect("checkpoint_once must succeed against a healthy dedicated connection");
    let elapsed = start.elapsed();

    // Commit and release the read snapshot only after checkpoint_once returns.
    reader.conn().execute_batch("COMMIT;").ok();
    drop(reader);

    // PASSIVE returns without waiting for the reader snapshot. A TRUNCATE
    // regression would block for the configured busy timeout (2000ms), so
    // use the midpoint of that gap rather than a noise-floor threshold.
    let max_elapsed = checkpoint_config.truncate_busy_timeout / 2;
    assert!(
        elapsed < max_elapsed,
        "checkpoint_once with active reader snapshot took {:?}; expected <{:?} \
             (PASSIVE must not block on readers; a TRUNCATE regression would block \
             for the configured {:?})",
        elapsed,
        max_elapsed,
        checkpoint_config.truncate_busy_timeout
    );
}

/// Regression: a Skipped tick must NOT reset `was_above_high_water`. See
/// crates/khive-db/docs/api/checkpoint.md#skipped_tick_does_not_reset_high_water_crossing_state
#[test]
fn skipped_tick_does_not_reset_high_water_crossing_state() {
    let mut was_above = false;

    // First observed tick: above threshold — fires WARN, sets was_above=true.
    assert!(
        crossing_warn(true, &mut was_above),
        "should fire on first crossing"
    );
    assert!(was_above);

    // Simulate several skipped ticks: crossing state must remain true.
    // (In the task, Skipped causes `continue` so crossing_warn is never called.)
    // We verify by calling crossing_warn with the SAME above=true value, which
    // is what Observed(high_count) would produce — but a Skipped tick skips
    // the call entirely, so was_above stays as-is. Test the invariant directly:
    // if we leave was_above unchanged (no call at all), was_above remains true.
    assert!(was_above, "was_above must stay true across skipped ticks");

    // Another observed tick still above threshold — must NOT re-fire.
    let fired = crossing_warn(true, &mut was_above);
    assert!(!fired, "WARN must not re-fire while still above threshold");

    // Observed tick below threshold — resets was_above.
    let fired = crossing_warn(false, &mut was_above);
    assert!(!fired);
    assert!(!was_above);

    // Next observed tick above threshold — fires again (legitimate new crossing).
    let fired = crossing_warn(true, &mut was_above);
    assert!(fired, "WARN must fire again on a new below→above crossing");
}

/// Regression: warn_pages WARN fires once on crossing, not every tick.
///
/// Before the fix, the WARN was emitted inside `checkpoint_once` on every tick
/// while WAL sat in the warn band — log spam under sustained moderate pressure.
/// With the fix, `crossing_warn` gates the WARN on the first in-band tick only;
/// subsequent ticks while still in the band return false.
#[test]
fn warn_pages_fires_once_on_crossing_not_every_tick() {
    let mut was_above_warn = false;

    // Simulate three consecutive ticks with WAL in the warn band.
    let fired_1 = crossing_warn(true, &mut was_above_warn);
    let fired_2 = crossing_warn(true, &mut was_above_warn);
    let fired_3 = crossing_warn(true, &mut was_above_warn);

    assert!(fired_1, "WARN must fire on the first in-band tick");
    assert!(
        !fired_2,
        "WARN must not fire on the second consecutive in-band tick"
    );
    assert!(
        !fired_3,
        "WARN must not fire on the third consecutive in-band tick"
    );

    // Drop below warn band — resets state.
    crossing_warn(false, &mut was_above_warn);
    assert!(!was_above_warn);

    // Re-enter warn band — fires again.
    let fired_reentry = crossing_warn(true, &mut was_above_warn);
    assert!(
        fired_reentry,
        "WARN must fire again on re-entry into warn band"
    );
}

// ADR-091 Plank 2: TRUNCATE escalation state machine tests.

/// Trigger threshold: once `wal_pages` (as observed by `checkpoint_once`) is
/// at/above `truncate_high_water_pages` and no prior attempt has run, the
/// escalation fires and stamps `last_attempt`.
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn truncate_attempts_when_high_water_crossed_with_no_prior_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncate_trigger.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    let config = CheckpointConfig {
        // Force the escalation to arm regardless of the tiny WAL this test
        // actually produces — isolates the trigger-threshold behavior from
        // needing to stuff 20,000 real WAL pages.
        truncate_high_water_pages: 0,
        truncate_min_interval: Duration::from_secs(300),
        ..CheckpointConfig::default()
    };
    let mut state = TruncateState::default();

    assert!(
        state.last_attempt.is_none(),
        "precondition: no attempt has run yet"
    );

    let conn = checkpoint_conn(&pool);
    checkpoint_once(&pool, &conn, &config, &mut state)
        .expect("checkpoint_once must succeed against a healthy dedicated connection");
    assert!(
        state.last_attempt.is_some(),
        "an attempt must be stamped once the high-water threshold is crossed"
    );
}

/// Below-threshold skip: `wal_pages < truncate_high_water_pages` must never
/// stamp `last_attempt` — only an actual attempt advances it.
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn truncate_does_not_attempt_below_high_water() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncate_below_threshold.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    // Effectively unreachable threshold for this test's tiny WAL.
    let config = CheckpointConfig {
        truncate_high_water_pages: u64::MAX,
        ..CheckpointConfig::default()
    };
    let mut state = TruncateState::default();

    let conn = checkpoint_conn(&pool);
    checkpoint_once(&pool, &conn, &config, &mut state)
        .expect("checkpoint_once must succeed against a healthy dedicated connection");

    assert!(
        state.last_attempt.is_none(),
        "a below-threshold tick must never stamp last_attempt"
    );
}

/// Min-interval skip: once an attempt has run, a subsequent tick that is
/// still above threshold but within `truncate_min_interval` must skip
/// without re-stamping `last_attempt` (the timestamp must not move).
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn truncate_min_interval_skip_does_not_restamp_last_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncate_min_interval.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    let config = CheckpointConfig {
        truncate_high_water_pages: 0,
        truncate_min_interval: Duration::from_secs(300),
        ..CheckpointConfig::default()
    };
    let mut state = TruncateState::default();
    let conn = checkpoint_conn(&pool);

    checkpoint_once(&pool, &conn, &config, &mut state)
        .expect("checkpoint_once must succeed against a healthy dedicated connection");
    let first_attempt = state.last_attempt.expect("first tick must attempt");

    // Second tick, immediately after, on the SAME dedicated connection
    // (mirroring how `run_checkpoint_task` reuses one connection across
    // ticks): still above threshold, but the min-interval has clearly
    // not elapsed — must skip and leave last_attempt exactly as it was.
    checkpoint_once(&pool, &conn, &config, &mut state)
        .expect("checkpoint_once must succeed against a healthy dedicated connection");
    let second_attempt = state.last_attempt.expect("attempt timestamp must persist");

    assert_eq!(
        first_attempt, second_attempt,
        "a tick within truncate_min_interval must not re-stamp last_attempt"
    );
}

/// The fix this module exists to prove: holding the POOL's writer mutex
/// (via `pool.try_writer()`, exactly like a concurrent write in
/// progress) must NOT cause `checkpoint_once` to skip — PASSIVE (and, if
/// armed, TRUNCATE) run on the task's own dedicated connection, which
/// never contends with the pool writer at all. Before the dedicated-
/// connection fix, this same setup made `checkpoint_once` return
/// `Skipped` via `try_writer_nowait()`. See also the standalone
/// integration reproducer in `tests/checkpoint_dedicated_connection.rs`,
/// which demonstrates the converse: a fat WAL held busy by
/// `checkpoint_once` no longer blocks a concurrent `pool.writer()`
/// admission either.
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn checkpoint_once_proceeds_and_can_attempt_truncate_while_pool_writer_held() {
    reset_checkpoint_metrics_for_tests();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncate_busy_skip.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    let conn = checkpoint_conn(&pool);

    // Hold the POOL's writer mutex for the duration of the checkpoint_once
    // call, acquired BEFORE the call so the dedicated connection cannot
    // possibly race a still-free writer.
    let _held = pool.try_writer().unwrap();

    let config = CheckpointConfig {
        truncate_high_water_pages: 0,
        ..CheckpointConfig::default()
    };
    let mut state = TruncateState::default();

    checkpoint_once(&pool, &conn, &config, &mut state).expect(
        "checkpoint_once must observe normally on its own dedicated connection even \
             while a concurrent caller holds the pool's writer mutex",
    );

    assert!(
        state.last_attempt.is_some(),
        "a threshold-armed tick must still evaluate (and attempt) TRUNCATE even while \
             the pool writer is held — the dedicated connection is unaffected by it"
    );
    assert_eq!(
        checkpoint_skipped_ticks(),
        0,
        "a busy pool writer must no longer count as a skipped checkpoint tick"
    );
    assert_eq!(
        checkpoint_consecutive_skips(),
        0,
        "a busy pool writer must not bump the consecutive-skip run length"
    );
}

/// Regression guard for #845 (a recurrence of the #828 shared-statics
/// race): every test in this module that calls `checkpoint_once`,
/// `checkpoint_once_core`, or `run_checkpoint_task` — all funnel through
/// `query_wal_pages`, which
/// writes the process-wide `LAST_WAL_PAGES` / `CHECKPOINT_*` atomics —
/// must be tagged with a `#[serial(...)]` group that includes
/// `checkpoint_skip_metrics`. Before #828, six such call sites carried no
/// serial tag at all: cargo's default test thread pool ran them
/// concurrently with `busy_writer_skips_both_passive_and_truncate`, and an
/// untagged tick's `query_wal_pages` call clobbered the gauges between
/// this test's warmup tick and its skip assertion (`left: Some(0), right:
/// Some(3)` on CI — the two ticks never actually raced against each
/// other, a third test's tick did). This scans the module's own source so
/// a future test that calls either function without the tag fails this
/// assertion instead of flaking on a loaded CI runner.
#[test]
#[serial(checkpoint_skip_metrics)]
fn all_checkpoint_metrics_callers_are_serial_tagged() {
    let sources = [
        include_str!("checkpoint.rs"),
        include_str!("checkpoint_tests.rs"),
        include_str!("checkpoint/churn_tests.rs"),
        include_str!("checkpoint_owner_interval_tests.rs"),
    ]
    .join("\n");
    let lines: Vec<&str> = sources.lines().collect();

    let attr_starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim();
            t == "#[test]" || t.starts_with("#[tokio::test")
        })
        .map(|(i, _)| i)
        .collect();

    let mut offenders = Vec::new();

    for (idx, &start) in attr_starts.iter().enumerate() {
        let end = attr_starts.get(idx + 1).copied().unwrap_or(lines.len());
        let span = &lines[start..end];

        let touches_shared_metrics = span.iter().any(|l| {
            l.contains("checkpoint_once(")
                || l.contains("checkpoint_once_core(")
                || l.contains("run_checkpoint_task(")
        });
        if !touches_shared_metrics {
            continue;
        }

        // Rustfmt splits long multi-key attributes across lines, so scan
        // the whole attribute instead of requiring the group on `#[serial(`.
        let mut in_serial_attr = false;
        let has_group_tag = span.iter().any(|line| {
            let trimmed = line.trim();
            if !in_serial_attr {
                in_serial_attr = trimmed.starts_with("#[serial(");
            }
            if !in_serial_attr {
                return false;
            }

            let has_group = trimmed
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(|token| token == "checkpoint_skip_metrics");
            if trimmed.ends_with(")]") {
                in_serial_attr = false;
            }
            has_group
        });

        if !has_group_tag {
            let name = span
                .iter()
                .find_map(|l| {
                    let t = l.trim_start();
                    let t = t.strip_prefix("pub(crate) ").unwrap_or(t);
                    let t = t.strip_prefix("pub ").unwrap_or(t);
                    let t = t.strip_prefix("async ").unwrap_or(t);
                    t.strip_prefix("fn ")
                        .map(|rest| rest.split(['(', '<']).next().unwrap_or("").trim())
                })
                .unwrap_or("<unknown test>");
            offenders.push(name.to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "these tests call checkpoint_once/checkpoint_once_core/run_checkpoint_task (which write the \
             process-wide LAST_WAL_PAGES/CHECKPOINT_* atomics via query_wal_pages) but \
             are not tagged #[serial(checkpoint_skip_metrics)] (or a group including it); \
             an untagged caller running concurrently on cargo's default test thread pool \
             can clobber those atomics mid-assertion in another test (the #828/#845 race): \
             {offenders:?}"
    );
}

/// Observation branch: a checkpoint tick that is actually observed
/// (dedicated connection available) must close out a prior skip streak,
/// resetting the consecutive-skip counter to 0 without touching the
/// lifetime total. Drives `note_checkpoint_skipped()` directly for the
/// skipped ticks — exactly what `run_checkpoint_task` calls when
/// `CheckpointConnection::ensure_open` returns `None` — rather than
/// through pool-writer contention, which (since the dedicated-connection
/// fix) no longer produces a skipped tick at all.
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn observed_tick_resets_consecutive_skips_but_not_lifetime_total() {
    reset_checkpoint_metrics_for_tests();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("skip_then_observe.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    // Two consecutive skipped ticks (dedicated connection unavailable).
    note_checkpoint_skipped();
    note_checkpoint_skipped();
    assert_eq!(checkpoint_skipped_ticks(), 2);
    assert_eq!(checkpoint_consecutive_skips(), 2);

    // Now the dedicated connection is available: an observed tick must
    // reset the streak.
    let conn = checkpoint_conn(&pool);
    let mut state = TruncateState::default();
    checkpoint_once(&pool, &conn, &CheckpointConfig::default(), &mut state)
        .expect("checkpoint_once must succeed against a healthy dedicated connection");

    assert_eq!(
        checkpoint_skipped_ticks(),
        2,
        "an observed tick must not change the lifetime skipped-tick total"
    );
    assert_eq!(
        checkpoint_consecutive_skips(),
        0,
        "an observed tick must reset the consecutive-skip run length"
    );
}

/// Edge-triggered escalation WARN: `note_truncate_outcome` fires exactly
/// once, on the third consecutive attempt that fails to clear
/// `warn_pages`, and does not repeat on a fourth consecutive failure. A
/// single attempt that clears `warn_pages` resets the counter.
#[test]
#[serial(checkpoint_skip_metrics)]
fn note_truncate_outcome_warns_once_at_third_consecutive_failure() {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };

    let config = CheckpointConfig {
        warn_pages: 2000,
        ..CheckpointConfig::default()
    };
    let mut state = TruncateState::default();

    tracing::subscriber::with_default(subscriber, || {
        // Three consecutive attempts that fail to clear warn_pages.
        note_truncate_outcome(&config, Some(5000), &mut state);
        note_truncate_outcome(&config, Some(5000), &mut state);
        note_truncate_outcome(&config, Some(5000), &mut state);
        // A fourth consecutive failure must not re-fire the escalation.
        note_truncate_outcome(&config, Some(5000), &mut state);
    });

    assert_eq!(state.consecutive_failures, 4);

    let events = buffer.lock().unwrap();
    let escalation_count = events
        .iter()
        .filter(|e| {
            e.message.as_deref()
                == Some("WAL TRUNCATE has failed to clear WAL pressure for 3 consecutive attempts")
        })
        .count();
    assert_eq!(
        escalation_count, 1,
        "escalation WARN must fire exactly once at the 3rd consecutive failure, got: {events:?}"
    );

    // A clearing attempt resets the counter.
    note_truncate_outcome(&config, Some(100), &mut state);
    assert_eq!(
        state.consecutive_failures, 0,
        "an attempt that clears warn_pages must reset the consecutive-failure counter"
    );
}

// ADR-091 #617: graduated severity ladder state-machine tests.

fn severity_test_config() -> CheckpointConfig {
    CheckpointConfig {
        warn_pages: 100,
        warn_sustained_cycles: 3,
        ..CheckpointConfig::default()
    }
}

/// INFO rung: a below→above crossing emits exactly one INFO and no WARN
/// (default `warn_sustained_cycles = 3`, only one above-warn tick here).
#[test]
fn severity_ladder_info_on_first_crossing_no_warn() {
    let config = severity_test_config();
    let mut state = CheckpointSeverityState::default();

    let below = state.observe_wal_pages(10, &config);
    assert!(below.is_empty(), "below-warn tick must emit nothing");

    let above = state.observe_wal_pages(150, &config);
    assert_eq!(
        above,
        vec![CheckpointSeverityEmission {
            rung: CheckpointSeverityRung::Info,
            wal_pages: 150,
            threshold_pages: 100,
            consecutive_cycles: 1,
        }],
        "first below->above crossing must emit exactly one INFO and no WARN"
    );
}

/// WARN rung: `warn_sustained_cycles` (3) consecutive above-warn ticks
/// emit WARN exactly on the third tick, not before and not repeated after.
#[test]
fn severity_ladder_warn_on_third_consecutive_cycle() {
    let config = severity_test_config();
    let mut state = CheckpointSeverityState::default();

    let tick1 = state.observe_wal_pages(150, &config);
    assert_eq!(tick1.len(), 1);
    assert_eq!(tick1[0].rung, CheckpointSeverityRung::Info);

    let tick2 = state.observe_wal_pages(150, &config);
    assert!(
        tick2.is_empty(),
        "second consecutive above-warn tick must emit nothing yet"
    );

    let tick3 = state.observe_wal_pages(150, &config);
    assert_eq!(
        tick3,
        vec![CheckpointSeverityEmission {
            rung: CheckpointSeverityRung::Warn,
            wal_pages: 150,
            threshold_pages: 100,
            consecutive_cycles: 3,
        }],
        "WARN must fire exactly on the third consecutive above-warn tick"
    );

    let tick4 = state.observe_wal_pages(150, &config);
    assert!(
        tick4.is_empty(),
        "WARN must not repeat on a fourth consecutive above-warn tick"
    );
}

/// Re-arm: after a WARN episode drains below warn_pages, a fresh episode
/// of `warn_sustained_cycles` above-warn ticks must WARN again.
#[test]
fn severity_ladder_rearms_warn_after_drain() {
    let config = severity_test_config();
    let mut state = CheckpointSeverityState::default();

    // First episode reaches WARN.
    for _ in 0..3 {
        state.observe_wal_pages(150, &config);
    }
    assert!(state.warn_emitted_for_episode);

    // Drain below warn_pages: resets the episode.
    let drain = state.observe_wal_pages(10, &config);
    assert!(drain.is_empty(), "a draining tick must emit nothing");

    // Second episode: INFO on first tick, no WARN until the third again.
    let reentry = state.observe_wal_pages(150, &config);
    assert_eq!(reentry.len(), 1);
    assert_eq!(reentry[0].rung, CheckpointSeverityRung::Info);

    let mid = state.observe_wal_pages(150, &config);
    assert!(mid.is_empty());

    let second_warn = state.observe_wal_pages(150, &config);
    assert_eq!(
        second_warn,
        vec![CheckpointSeverityEmission {
            rung: CheckpointSeverityRung::Warn,
            wal_pages: 150,
            threshold_pages: 100,
            consecutive_cycles: 3,
        }],
        "a fresh elevation episode after a drain must WARN again"
    );
}

/// False-positive guard: three isolated single-tick crossings, each
/// followed by a drain, must never reach WARN — only INFO fires each time.
#[test]
fn severity_ladder_isolated_crossings_never_warn() {
    let config = severity_test_config();
    let mut state = CheckpointSeverityState::default();

    for _ in 0..3 {
        let crossing = state.observe_wal_pages(150, &config);
        assert_eq!(
            crossing.len(),
            1,
            "each isolated crossing must emit exactly one INFO"
        );
        assert_eq!(crossing[0].rung, CheckpointSeverityRung::Info);

        let drain = state.observe_wal_pages(10, &config);
        assert!(drain.is_empty(), "the drain tick must emit nothing");
    }

    assert!(
        !state.warn_emitted_for_episode,
        "isolated single-tick crossings must never accumulate into a WARN"
    );
}

/// ALARM rung: the existing TRUNCATE-attempt gate is the ADR-091 ALARM
/// tier. `observe_wal_pages` never produces it; this test documents and
/// locks in that boundary so a future change can't silently reroute
/// ALARM through the INFO/WARN ladder.
#[test]
fn severity_ladder_never_emits_alarm() {
    let config = CheckpointConfig {
        warn_pages: 100,
        warn_sustained_cycles: 1,
        ..CheckpointConfig::default()
    };
    let mut state = CheckpointSeverityState::default();

    for wal_pages in [150, 200, 250, u64::MAX] {
        let emissions = state.observe_wal_pages(wal_pages, &config);
        assert!(
            emissions
                .iter()
                .all(|e| e.rung != CheckpointSeverityRung::Alarm),
            "observe_wal_pages must never emit the ALARM rung, got: {emissions:?}"
        );
    }
}

// ADR-091 Plank 1: `TxAgeSweepState` background-sweep state-machine tests.
// Pure unit tests mirroring the severity-ladder tests above — no I/O.

fn tx_age_test_config() -> CheckpointConfig {
    CheckpointConfig {
        tx_warn_secs: Duration::from_secs(30),
        tx_max_age_secs: Duration::from_secs(120),
        ..CheckpointConfig::default()
    }
}

/// Synthetic identity for `TxAgeSweepState::observe`'s pure unit tests
/// below, which exercise identity-change detection without paying for a
/// real `tx_registry::register` call. `TxId`'s wrapped value is public
/// exactly to support this (see its doc comment in `khive-storage`).
fn tx_id(n: u64) -> khive_storage::tx_registry::TxId {
    khive_storage::tx_registry::TxId(n)
}

/// No open entry: nothing fires, and any prior latch state clears.
#[test]
fn tx_age_sweep_empty_registry_emits_nothing() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    let emissions = state.observe(None, config.tx_warn_secs, config.tx_max_age_secs);
    assert!(emissions.is_empty(), "no open entry must emit nothing");
}

/// A fresh entry (age below both thresholds) emits nothing.
#[test]
fn tx_age_sweep_fresh_entry_emits_nothing() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    let emissions = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(5),
            Some("fresh_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert!(emissions.is_empty(), "a fresh entry must emit nothing");
}

/// Below→above crossing of `tx_warn_secs` fires exactly one `Warn`
/// emission carrying the entry's label; it must not repeat on a second
/// tick that is still above `tx_warn_secs` but below `tx_max_age_secs`.
#[test]
fn tx_age_sweep_warn_fires_once_on_crossing() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    let tick1 = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(45),
            Some("stale_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        tick1,
        vec![TxAgeEmission {
            rung: TxAgeRung::Warn,
            age: Duration::from_secs(45),
            label: Some("stale_span".to_string()),
        }],
        "crossing tx_warn_secs must emit exactly one Warn"
    );

    let tick2 = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(50),
            Some("stale_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert!(
        tick2.is_empty(),
        "Warn must not repeat while the entry stays in the warn band"
    );
}

/// Crossing `tx_max_age_secs` fires `Stale`; a further tick still above
/// the cap must not repeat it.
#[test]
fn tx_age_sweep_stale_fires_once_on_crossing() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    // Drive through the warn crossing first, matching real elapsed-time
    // progression (an entry ages through the warn band before the max).
    state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(45),
            Some("stuck_writer_task_tx".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );

    let tick = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(130),
            Some("stuck_writer_task_tx".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        tick,
        vec![TxAgeEmission {
            rung: TxAgeRung::Stale,
            age: Duration::from_secs(130),
            label: Some("stuck_writer_task_tx".to_string()),
        }],
        "crossing tx_max_age_secs must emit exactly one Stale"
    );

    let tick_repeat = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(200),
            Some("stuck_writer_task_tx".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert!(
        tick_repeat.is_empty(),
        "Stale must not repeat while the entry stays above tx_max_age_secs"
    );
}

/// An entry already stale the first time the sweep observes it (e.g.
/// right after process start with a pre-existing registry entry) crosses
/// both rungs on the same tick.
#[test]
fn tx_age_sweep_already_stale_entry_emits_both_rungs_same_tick() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    let tick = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(300),
            Some("ancient_tx".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        tick,
        vec![
            TxAgeEmission {
                rung: TxAgeRung::Warn,
                age: Duration::from_secs(300),
                label: Some("ancient_tx".to_string()),
            },
            TxAgeEmission {
                rung: TxAgeRung::Stale,
                age: Duration::from_secs(300),
                label: Some("ancient_tx".to_string()),
            },
        ],
        "an already-stale entry must cross both rungs on its first observed tick"
    );
}

/// Re-arm: once the stale entry closes (registry reports a fresher
/// oldest entry, or none at all), a future stale span must fire again.
#[test]
fn tx_age_sweep_rearms_after_entry_clears() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(150),
            Some("first_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );

    // The stale span closed; nothing is open now.
    let cleared = state.observe(None, config.tx_warn_secs, config.tx_max_age_secs);
    assert!(cleared.is_empty(), "a clearing tick must emit nothing");

    // A fresh entry (unrelated span) is now oldest — still below threshold.
    let fresh = state.observe(
        Some((
            tx_id(2),
            Duration::from_secs(2),
            Some("second_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert!(fresh.is_empty(), "a fresh oldest entry must emit nothing");

    // That second span goes stale in turn — must WARN again (re-armed).
    let rewarn = state.observe(
        Some((
            tx_id(2),
            Duration::from_secs(35),
            Some("second_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        rewarn,
        vec![TxAgeEmission {
            rung: TxAgeRung::Warn,
            age: Duration::from_secs(35),
            label: Some("second_span".to_string()),
        }],
        "a fresh stale episode after a clear must Warn again"
    );
}

/// Fix: an already-stale entry replacing a stale one on the next tick,
/// with no intervening clear, must still emit both rungs. See
/// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_stale_replacement_without_intervening_clear_still_names_new_entry
#[test]
fn tx_age_sweep_stale_replacement_without_intervening_clear_still_names_new_entry() {
    let config = tx_age_test_config();
    let mut state = TxAgeSweepState::default();

    let tick_a = state.observe(
        Some((
            tx_id(1),
            Duration::from_secs(300),
            Some("stale_entry_a".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        tick_a.len(),
        2,
        "entry A must cross both rungs on its first observed tick, got: {tick_a:?}"
    );

    // B replaces A as the oldest entry on the VERY NEXT tick — already
    // stale itself, with no intervening None/below-threshold tick.
    let tick_b = state.observe(
        Some((
            tx_id(2),
            Duration::from_secs(400),
            Some("stale_entry_b".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        tick_b,
        vec![
            TxAgeEmission {
                rung: TxAgeRung::Warn,
                age: Duration::from_secs(400),
                label: Some("stale_entry_b".to_string()),
            },
            TxAgeEmission {
                rung: TxAgeRung::Stale,
                age: Duration::from_secs(400),
                label: Some("stale_entry_b".to_string()),
            },
        ],
        "a same-tick identity change to an already-stale successor must re-emit both \
             rungs naming the NEW entry, got: {tick_b:?}"
    );
}

/// Closes the loop from env var to actual emitted rung. See
/// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_uses_configured_thresholds_not_hardcoded_defaults
#[test]
fn tx_age_sweep_uses_configured_thresholds_not_hardcoded_defaults() {
    let config = CheckpointConfig {
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(2),
        ..CheckpointConfig::default()
    };
    let mut state = TxAgeSweepState::default();

    let tick = state.observe(
        Some((
            tx_id(1),
            Duration::from_millis(5),
            Some("fast_cap_span".to_string()),
        )),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert_eq!(
        tick.len(),
        2,
        "a millisecond-scale cap must cross both rungs immediately, got: {tick:?}"
    );
}

/// Integration-level regression for the incident this ADR fixes. See
/// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx_age_sweep_reader_pin.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    // Open a real read transaction so it holds a WAL snapshot (same
    // isomorphism as `checkpoint_high_water_does_not_block_behind_reader`),
    // AND register it in tx_registry — the telemetry a real long-lived
    // reader call site (e.g. `graph_traverse_read`) is expected to carry.
    let reader = pool.reader().expect("reader");
    reader
        .conn()
        .execute_batch("BEGIN DEFERRED; SELECT * FROM t;")
        .expect("begin read tx");
    let _tx_handle =
        khive_storage::tx_registry::register(Some("tx_age_sweep_reader_pin_test".to_string()));

    // Drive writes past high_water_pages while the reader snapshot pins
    // the WAL tail — PASSIVE cannot reclaim these frames.
    let config = CheckpointConfig {
        high_water_pages: 1,
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(1),
        ..CheckpointConfig::default()
    };
    {
        let writer = pool.try_writer().unwrap();
        for i in 0..50 {
            writer
                .conn()
                .execute_batch(&format!("INSERT INTO t VALUES ({i});"))
                .unwrap();
        }
    }

    let conn = checkpoint_conn(&pool);
    let wal_pages = checkpoint_once(&pool, &conn, &config, &mut TruncateState::default())
        .expect("checkpoint_once must succeed against a healthy dedicated connection");
    assert!(
        wal_pages >= config.high_water_pages,
        "test setup must actually drive wal_pages ({wal_pages}) past high_water_pages \
             ({}) for this regression to mean anything",
        config.high_water_pages
    );

    // The Plank 1 sweep, given the SAME registry state, must name the
    // pinning reader at the Stale rung. The handle's age must exceed the
    // 1ms `tx_max_age_secs` cap deterministically: the inserts plus one
    // PASSIVE checkpoint above can complete in under a millisecond on a
    // warm page cache, so sleep past the cap instead of assuming
    // the elapsed work already crossed it.
    std::thread::sleep(Duration::from_millis(5));
    // `tx_registry` is a process-wide singleton shared by every test in
    // this binary (cargo runs `#[test]`s in parallel threads of the same
    // process): `#[serial(tx_registry)]` only excludes other tests that
    // carry the same key, not every production write path elsewhere in
    // the crate (e.g. `graph_upsert_edges`) that also calls `register()`
    // as ordinary telemetry. If one of those happens to still be open and
    // was registered before this test's own handle, raw `oldest()` would
    // return THAT entry instead of the fixture's reader — see #926. Look
    // up this test's own entry by its known label instead of trusting
    // global `oldest()`, so the assertion is immune to that noise.
    let our_entry = khive_storage::tx_registry::snapshot()
        .into_iter()
        .find(|(_, label)| label.as_deref() == Some("tx_age_sweep_reader_pin_test"))
        .expect("this test's own tx_registry entry must still be open");
    let mut tx_age_state = TxAgeSweepState::default();
    let emissions = tx_age_state.observe(
        Some((tx_id(1), our_entry.0, our_entry.1)),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert!(
        emissions.iter().any(|e| e.rung == TxAgeRung::Stale
            && e.label.as_deref() == Some("tx_age_sweep_reader_pin_test")),
        "expected a Stale emission naming the pinning reader, got: {emissions:?}"
    );

    reader.conn().execute_batch("COMMIT;").ok();
    drop(reader);
    drop(_tx_handle);
}

/// Regression #926: reproduces the exact tx_registry race directly. See
/// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_own_entry_survives_concurrent_older_registration
#[test]
#[serial(tx_registry, checkpoint_skip_metrics)]
fn tx_age_sweep_own_entry_survives_concurrent_older_registration() {
    let _decoy = khive_storage::tx_registry::register(Some("decoy_unrelated_span".to_string()));
    std::thread::sleep(Duration::from_millis(2));
    let _own = khive_storage::tx_registry::register(Some("this_test_own_span".to_string()));
    std::thread::sleep(Duration::from_millis(5));

    // Confirm the race condition is actually reproduced: an entry older
    // than this test's own span must currently lead the process-wide
    // registry. Another concurrently running test may have registered an
    // entry before the decoy, so do not assume the decoy is globally
    // oldest; the required invariant is only that our span is not.
    let global_oldest = khive_storage::tx_registry::oldest().expect("registry not empty");
    assert_ne!(
        global_oldest.2.as_deref(),
        Some("this_test_own_span"),
        "test setup must reproduce the race: an older, unrelated entry must be \
             the current global oldest, got: {global_oldest:?}"
    );

    let our_entry = khive_storage::tx_registry::snapshot()
        .into_iter()
        .find(|(_, label)| label.as_deref() == Some("this_test_own_span"))
        .expect("this test's own tx_registry entry must still be open");

    let config = CheckpointConfig {
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(1),
        ..CheckpointConfig::default()
    };
    let mut state = TxAgeSweepState::default();
    let emissions = state.observe(
        Some((tx_id(2), our_entry.0, our_entry.1)),
        config.tx_warn_secs,
        config.tx_max_age_secs,
    );
    assert!(
        emissions
            .iter()
            .any(|e| e.rung == TxAgeRung::Stale
                && e.label.as_deref() == Some("this_test_own_span")),
        "expected a Stale emission naming this test's own span despite an older, \
             unrelated concurrent registration, got: {emissions:?}"
    );
}

// ADR-094: `CheckpointOutcomeRecorded` lifecycle event tests.

#[derive(Clone, Copy)]
enum FakeAppendBehavior {
    Record,
    Fail,
}

struct FakeEventStore {
    events: std::sync::Mutex<Vec<khive_storage::Event>>,
    append_attempts: std::sync::atomic::AtomicUsize,
    append_behavior: FakeAppendBehavior,
}

impl Default for FakeEventStore {
    fn default() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            append_attempts: std::sync::atomic::AtomicUsize::new(0),
            append_behavior: FakeAppendBehavior::Record,
        }
    }
}

impl FakeEventStore {
    fn failing() -> Self {
        Self {
            append_behavior: FakeAppendBehavior::Fail,
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl khive_storage::EventStore for FakeEventStore {
    async fn append_event(&self, event: khive_storage::Event) -> khive_storage::StorageResult<()> {
        self.append_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.append_behavior {
            FakeAppendBehavior::Record => {
                self.events.lock().unwrap().push(event);
                Ok(())
            }
            FakeAppendBehavior::Fail => Err(khive_storage::StorageError::Internal(
                "synthetic checkpoint lifecycle append failure".to_string(),
            )),
        }
    }

    async fn append_events(
        &self,
        events: Vec<khive_storage::Event>,
    ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
        let count = events.len() as u64;
        self.events.lock().unwrap().extend(events);
        Ok(khive_storage::BatchWriteSummary {
            attempted: count,
            affected: count,
            ..khive_storage::BatchWriteSummary::default()
        })
    }

    async fn get_event(
        &self,
        id: uuid::Uuid,
    ) -> khive_storage::StorageResult<Option<khive_storage::Event>> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }

    async fn query_events(
        &self,
        _filter: khive_storage::EventFilter,
        _page: khive_storage::PageRequest,
    ) -> khive_storage::StorageResult<khive_storage::Page<khive_storage::Event>> {
        unimplemented!("not exercised by the checkpoint lifecycle-event tests")
    }

    async fn count_events(
        &self,
        _filter: khive_storage::EventFilter,
    ) -> khive_storage::StorageResult<u64> {
        Ok(self.events.lock().unwrap().len() as u64)
    }
}

/// Pure decision-table coverage for every input combination
/// `checkpoint_outcome_should_emit` can see: elevation, sustained
/// pressure, recovery, and repeated healthy observations.
#[test]
fn checkpoint_outcome_should_emit_covers_all_transitions() {
    assert!(
        checkpoint_outcome_should_emit(true, false),
        "first elevated tick must emit"
    );
    assert!(
        !checkpoint_outcome_should_emit(true, true),
        "sustained elevated ticks must aggregate in memory instead of writing the WAL"
    );
    assert!(
        checkpoint_outcome_should_emit(false, true),
        "the single drain row (elevated -> healthy) must emit"
    );
    assert!(
        !checkpoint_outcome_should_emit(false, false),
        "an ordinary below-warn tick must not emit"
    );
}

/// Regression #1838: under a persistent WAL pin, lifecycle persistence
/// must scale with pressure-state transitions, not checkpoint attempts.
#[test]
fn persistent_pressure_lifecycle_rows_are_o_state_transitions() {
    let observations = [true; 128].into_iter().chain([false]).chain([false; 128]);
    let mut was_elevated = false;
    let writes = observations
        .filter(|above_warn| {
            let emit = checkpoint_outcome_should_emit(*above_warn, was_elevated);
            if emit {
                was_elevated = *above_warn;
            }
            emit
        })
        .count();

    assert_eq!(
        writes, 2,
        "one elevation row plus one recovery summary must cover any number of attempts"
    );
}

#[test]
fn checkpoint_pressure_episode_retains_recovery_summary() {
    let mut episode = CheckpointPressureEpisode::start(2_500);
    episode.observe(2_300);
    episode.observe(8_100);
    episode.observe(4_000);

    assert_eq!(episode.elevated_ticks, 4);
    assert_eq!(episode.peak_wal_pages, 8_100);
}

/// Drives [`observe_checkpoint_pressure_tick`] through a fixed sequence
/// of `(above_warn, wal_pages)` ticks, faking `try_emit` per call index
/// (0-based across the whole sequence) via `fail_on`. Returns every
/// payload that was reported as successfully delivered, in delivery
/// order.
fn drive_pressure_ticks(
    config: &CheckpointConfig,
    ticks: &[(bool, u64)],
    mut fail_on: impl FnMut(usize) -> bool,
) -> Vec<khive_storage::CheckpointOutcomeRecordedPayload> {
    let mut event_elevation_open = false;
    let mut pressure_episode: Option<CheckpointPressureEpisode> = None;
    let mut pending_recovery: Option<khive_storage::CheckpointOutcomeRecordedPayload> = None;
    let mut delivered = Vec::new();
    let mut call_index = 0usize;
    for &(above_warn, wal_pages) in ticks {
        observe_checkpoint_pressure_tick(
            above_warn,
            wal_pages,
            false,
            false,
            config,
            &mut event_elevation_open,
            &mut pressure_episode,
            &mut pending_recovery,
            |payload| {
                let idx = call_index;
                call_index += 1;
                if fail_on(idx) {
                    false
                } else {
                    delivered.push(payload);
                    true
                }
            },
        );
    }
    delivered
}

#[test]
fn busy_row_does_not_close_or_rearm_an_observed_pressure_episode() {
    let config = CheckpointConfig {
        warn_pages: 10,
        high_water_pages: 15,
        warn_sustained_cycles: 2,
        ..CheckpointConfig::default()
    };
    let rows = [
        RawCheckpointObservation {
            busy: 0,
            log_frames: 20,
            checkpointed_frames: 5,
        },
        RawCheckpointObservation {
            busy: 1,
            log_frames: -1,
            checkpointed_frames: -1,
        },
        RawCheckpointObservation {
            busy: 0,
            log_frames: 20,
            checkpointed_frames: 5,
        },
        RawCheckpointObservation {
            busy: 0,
            log_frames: 5,
            checkpointed_frames: 5,
        },
    ];
    let ticks: Vec<(bool, u64)> = rows
        .iter()
        .filter_map(|row| observed_wal_pages(*row).ok())
        .map(|pages| (pages >= config.warn_pages, pages))
        .collect();
    assert_eq!(ticks, vec![(true, 20), (true, 20), (false, 5)]);

    let elevated = drive_pressure_ticks(&config, &ticks[..2], |_| false);
    assert_eq!(
        elevated.len(),
        1,
        "busy must not emit a recovery or new elevation"
    );
    assert!(elevated[0].above_warn);
    let complete = drive_pressure_ticks(&config, &ticks, |_| false);
    assert_eq!(complete.len(), 2, "one elevation and one real recovery");
    assert!(!complete[1].above_warn);
    assert_eq!(complete[1].episode_elevated_ticks, Some(2));

    let mut severity = CheckpointSeverityState::default();
    let mut was_above_high_water = false;
    let mut high_water_warnings = 0;
    let mut rungs = Vec::new();
    for row in rows {
        if let Ok(pages) = observed_wal_pages(row) {
            high_water_warnings += usize::from(crossing_warn(
                pages >= config.high_water_pages,
                &mut was_above_high_water,
            ));
            rungs.extend(
                severity
                    .observe_wal_pages(pages, &config)
                    .into_iter()
                    .map(|e| e.rung),
            );
        }
    }
    assert_eq!(high_water_warnings, 1);
    assert_eq!(
        rungs,
        vec![CheckpointSeverityRung::Info, CheckpointSeverityRung::Warn]
    );
    assert!(
        !was_above_high_water,
        "only the measured recovery rearms high-water"
    );
    assert_eq!(
        observed_wal_pages(RawCheckpointObservation {
            busy: 1,
            log_frames: 100,
            checkpointed_frames: 99,
        }),
        Err(CheckpointUnavailableReason::Busy),
        "a populated busy row is neutral too"
    );
    assert_eq!(
        observed_wal_pages(RawCheckpointObservation {
            busy: 0,
            log_frames: -1,
            checkpointed_frames: -1,
        }),
        Ok(0),
        "a nonbusy row with no WAL is a measured zero"
    );
    assert_eq!(
        observed_wal_pages(RawCheckpointObservation {
            busy: 0,
            log_frames: 20,
            checkpointed_frames: -1,
        }),
        Err(CheckpointUnavailableReason::InconsistentFrames),
        "a malformed frame pair must not be clamped into a measurement"
    );
    assert_eq!(
        observed_wal_pages(RawCheckpointObservation {
            busy: 0,
            log_frames: 5,
            checkpointed_frames: 6,
        }),
        Err(CheckpointUnavailableReason::InconsistentFrames),
        "a checkpointed count above the log count is inconsistent"
    );
}

#[test]
#[serial(checkpoint_skip_metrics)]
fn inconsistent_nonbusy_row_is_not_reported_as_sqlite_busy() {
    reset_checkpoint_metrics_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("inconsistent_checkpoint_row.db"));
    let conn = checkpoint_conn(&pool);
    let row = RawCheckpointObservation {
        busy: 0,
        log_frames: 20,
        checkpointed_frames: -1,
    };
    assert_eq!(
        observed_wal_pages(row),
        Err(CheckpointUnavailableReason::InconsistentFrames)
    );
    test_arm_passive_row(&pool, row);

    let mut result = None;
    let events = capture(|| {
        result = Some(checkpoint_once(
            &pool,
            &conn,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        ));
    });
    let error = result
        .expect("checkpoint_once ran")
        .expect_err("an inconsistent nonbusy row cannot be measured");
    let rusqlite::Error::SqliteFailure(code, Some(message)) = error else {
        panic!("expected an explicit SQLite error for inconsistent frames");
    };
    assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_ERROR);
    assert!(message.contains("inconsistent frame pair"));
    assert!(!message.contains("busy"));
    assert!(events.iter().any(|event| {
        event.message.as_deref()
            == Some("WAL PASSIVE checkpoint returned an inconsistent frame pair; frame observation unavailable")
            && event.busy == Some(0)
    }));
    assert_eq!(checkpoint_timing(&pool).busy_ticks, 0);
    assert!(test_take_passive_row(&pool).is_none());
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn periodic_task_skips_a_passive_busy_row_after_opening_its_connection() {
    reset_checkpoint_metrics_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("periodic_busy_row.db"));
    {
        let writer = pool.writer().expect("seed WAL database");
        writer
            .conn()
            .execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1)")
            .expect("seed row");
    }
    // The real periodic task owns its dedicated connection. Inject only
    // its first SQLite-shaped PASSIVE return row, not a missing connection
    // or a hand-built CheckpointTick, so the production mapping to
    // `Skipped` and the skip counter are both exercised.
    test_arm_passive_row(
        &pool,
        RawCheckpointObservation {
            busy: 1,
            log_frames: 10,
            checkpointed_frames: 5,
        },
    );
    let skipped_before = checkpoint_skipped_ticks();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        CheckpointConfig {
            interval: Duration::from_millis(10),
            truncate_high_water_pages: u64::MAX,
            ..CheckpointConfig::default()
        },
        None,
        shutdown_rx,
        false,
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while checkpoint_timing(&pool).busy_ticks == 0 || checkpoint_skipped_ticks() == skipped_before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "periodic task did not classify the busy PASSIVE row as a skipped tick"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown_tx.send(()).expect("stop checkpoint task");
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("checkpoint task stops")
        .expect("checkpoint task does not panic");
    assert!(test_take_passive_row(&pool).is_none());
    assert!(checkpoint_timing(&pool).busy_ticks >= 1);
    assert!(checkpoint_skipped_ticks() > skipped_before);
}

/// #1857 regression: a dropped recovery handoff must not fold the next,
/// separate, pressure incident into the closed episode's aggregate — and
/// the undelivered recovery is a BARRIER, so episode 2's opening must not
/// be delivered ahead of episode 1's recovery (ADR-094: consumers assert
/// on the ordered event history).
///
/// Sequence: episode 1 opens and sustains for 3 ticks, then its recovery
/// row is dropped (call index 1) and two retries are also dropped (call
/// indices 2 and 3) — the second of them on the tick where episode 2
/// begins, so the barrier defers episode 2's opening. Once the worker
/// frees up, episode 1's delayed recovery delivers first, then episode
/// 2's opening (reflecting its state at emission time), then episode 2's
/// recovery.
#[test]
#[serial(checkpoint_skip_metrics)]
fn dropped_recovery_handoff_does_not_merge_pressure_episodes() {
    let config = CheckpointConfig {
        warn_pages: 1_000,
        ..CheckpointConfig::default()
    };
    let ticks = [
        (true, 1_500), // call 0: episode 1 opens — delivered
        (true, 1_800), // sustained, no emit attempt
        (true, 2_000), // sustained, no emit attempt
        (false, 500),  // call 1: episode 1 recovery — DROPPED
        (false, 400),  // call 2: retry episode 1 recovery — DROPPED
        (true, 3_000), // call 3: retry — DROPPED; barrier defers episode 2's opening
        (true, 3_500), // call 4: retry — delivered; call 5: episode 2 opens — delivered
        (false, 300),  // call 6: episode 2 recovery — delivered
    ];

    let delivered = drive_pressure_ticks(&config, &ticks, |idx| matches!(idx, 1..=3));

    assert_eq!(
        delivered.len(),
        4,
        "expected episode-1 open, episode-1 delayed recovery, episode-2 open, \
             episode-2 recovery: {delivered:?}"
    );

    let ep1_open = &delivered[0];
    assert!(ep1_open.above_warn);
    assert_eq!(ep1_open.episode_elevated_ticks, Some(1));
    assert_eq!(ep1_open.episode_peak_wal_pages, Some(1_500));

    let ep1_recovery = &delivered[1];
    assert!(
        !ep1_recovery.above_warn,
        "episode 1's recovery must be delivered BEFORE episode 2's opening; \
             an opening in this slot means the barrier failed: {delivered:?}"
    );
    assert_eq!(
        ep1_recovery.episode_elevated_ticks,
        Some(3),
        "episode 1's delayed recovery must report only its own 3 elevated ticks, \
             not ticks absorbed from episode 2"
    );
    assert_eq!(ep1_recovery.episode_peak_wal_pages, Some(2_000));

    let ep2_open = &delivered[2];
    assert!(ep2_open.above_warn);
    assert_eq!(
        ep2_open.episode_elevated_ticks,
        Some(2),
        "episode 2 opens fresh (never continuing episode 1's count), deferred one \
             tick by the barrier, so its opening reports 2 elevated ticks"
    );
    assert_eq!(ep2_open.episode_peak_wal_pages, Some(3_500));

    let ep2_recovery = &delivered[3];
    assert!(!ep2_recovery.above_warn);
    assert_eq!(
        ep2_recovery.episode_elevated_ticks,
        Some(2),
        "episode 2's recovery must report only its own 2 elevated ticks"
    );
    assert_eq!(ep2_recovery.episode_peak_wal_pages, Some(3_500));
}

/// Degenerate barrier arm: an episode whose entire lifetime falls inside
/// the window where an earlier recovery is still undelivered is discarded
/// rather than reported out of order — from any consumer's view it never
/// opened, so no stale opening or recovery for it may surface after the
/// queue frees. The loss itself is counted and logged at the discard
/// site; this test pins the delivered-history shape.
#[test]
#[serial(checkpoint_skip_metrics)]
fn episode_elapsed_entirely_behind_barrier_is_discarded_not_reordered() {
    let config = CheckpointConfig {
        warn_pages: 1_000,
        ..CheckpointConfig::default()
    };
    let ticks = [
        (true, 1_500), // call 0: episode 1 opens — delivered
        (false, 500),  // call 1: episode 1 recovery — DROPPED
        (true, 9_000), // call 2: retry — DROPPED; barrier defers episode 2's opening
        (false, 400),  // call 3: retry — DROPPED; episode 2 discarded behind barrier
        (false, 300),  // call 4: retry — delivered
        (true, 2_500), // call 5: episode 3 opens — delivered
        (false, 200),  // call 6: episode 3 recovery — delivered
    ];

    let delivered = drive_pressure_ticks(&config, &ticks, |idx| matches!(idx, 1..=3));

    let peaks: Vec<_> = delivered
        .iter()
        .map(|payload| (payload.above_warn, payload.episode_peak_wal_pages))
        .collect();
    assert_eq!(
        peaks,
        vec![
            (true, Some(1_500)),  // episode 1 open
            (false, Some(1_500)), // episode 1 delayed recovery
            (true, Some(2_500)),  // episode 3 open — episode 2 (peak 9_000) never surfaces
            (false, Some(2_500)), // episode 3 recovery
        ],
        "an episode elapsed entirely behind the barrier must not surface late or \
             out of order: {delivered:?}"
    );
}

/// ASCII-simple control for the regression above: the identical tick
/// sequence with no queue drops must report the same two episodes
/// separately (and promptly), confirming the merge in the drop case is
/// caused by the drop, not by the tick sequence itself.
#[test]
fn no_dropped_handoff_reports_two_separate_episodes() {
    let config = CheckpointConfig {
        warn_pages: 1_000,
        ..CheckpointConfig::default()
    };
    let ticks = [
        (true, 1_500),
        (true, 1_800),
        (true, 2_000),
        (false, 500),
        (false, 400),
        (true, 3_000),
        (true, 3_500),
        (false, 300),
    ];

    let delivered = drive_pressure_ticks(&config, &ticks, |_idx| false);

    assert_eq!(delivered.len(), 4, "{delivered:?}");
    assert_eq!(
        (
            delivered[0].above_warn,
            delivered[0].episode_elevated_ticks,
            delivered[0].episode_peak_wal_pages
        ),
        (true, Some(1), Some(1_500)),
        "episode 1 open"
    );
    assert_eq!(
        (
            delivered[1].above_warn,
            delivered[1].episode_elevated_ticks,
            delivered[1].episode_peak_wal_pages
        ),
        (false, Some(3), Some(2_000)),
        "episode 1 recovery"
    );
    assert_eq!(
        (
            delivered[2].above_warn,
            delivered[2].episode_elevated_ticks,
            delivered[2].episode_peak_wal_pages
        ),
        (true, Some(1), Some(3_000)),
        "episode 2 open"
    );
    assert_eq!(
        (
            delivered[3].above_warn,
            delivered[3].episode_elevated_ticks,
            delivered[3].episode_peak_wal_pages
        ),
        (false, Some(2), Some(3_500)),
        "episode 2 recovery"
    );
}

#[test]
#[serial(checkpoint_skip_metrics)]
fn pressure_diagnostics_count_observations_and_transitions_separately() {
    reset_checkpoint_metrics_for_tests();

    note_checkpoint_pressure_observation(true, false);
    note_checkpoint_pressure_observation(true, true);
    note_checkpoint_pressure_observation(true, true);
    note_checkpoint_pressure_observation(false, true);
    note_checkpoint_pressure_observation(false, false);

    assert_eq!(checkpoint_pressure_elevated_ticks(), 3);
    assert_eq!(checkpoint_pressure_episodes_started(), 1);
    assert_eq!(checkpoint_pressure_episodes_recovered(), 1);
    assert_eq!(checkpoint_lifecycle_append_attempts(), 0);
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_task_emits_one_opening_for_persistent_pressure() {
    reset_checkpoint_metrics_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcome_emit.db");
    let pool = file_pool(&path);

    // warn_pages: 0 means any observed WAL page count (even 0) is
    // "elevated" for the duration this config is active.
    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: 0,
        ..CheckpointConfig::default()
    };
    let store = Arc::new(FakeEventStore::default());
    let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        pool,
        cfg,
        Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
        shutdown_rx,
        true,
    ));

    let progressed = wait_for(Duration::from_secs(10), || {
        checkpoint_pressure_elevated_ticks() >= 10
    })
    .await;
    let emitted = wait_for(Duration::from_secs(10), || {
        !store.events.lock().unwrap().is_empty()
    })
    .await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    let events = store.events.lock().unwrap();
    assert!(
        progressed,
        "the simulated persistent-pressure episode must span at least ten checkpoint ticks"
    );
    assert!(
        emitted,
        "an always-elevated config must append one CheckpointOutcomeRecorded event \
             within the poll deadline"
    );
    assert_eq!(
        checkpoint_lifecycle_append_attempts(),
        1,
        "primary-store lifecycle writes must stay O(state transitions), not O(attempts)"
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload_schema_version, 2);
    assert_eq!(events[0].payload["episode_elevated_ticks"], 1);
    assert_eq!(
        events[0].payload["episode_peak_wal_pages"],
        events[0].payload["wal_pages"]
    );
    assert!(
        events
            .iter()
            .all(|e| e.kind == khive_types::EventKind::CheckpointOutcomeRecorded),
        "every appended event must be CheckpointOutcomeRecorded, got: {events:?}"
    );
    assert!(
        events.iter().all(|e| e.namespace == "local"),
        "events must be stamped with the namespace passed to run_checkpoint_task"
    );
}

/// Regression #1434/#1838: a lifecycle append may wait five seconds for
/// its sink writer, while checkpoint observations must continue without
/// enqueueing one new row per elevated tick.
#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_cycles_and_task_shutdown_do_not_wait_for_a_contended_lifecycle_writer() {
    reset_checkpoint_metrics_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcome_contended_sink.db");
    let checkpoint_pool = file_pool(&path);

    let event_pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            checkout_timeout: Duration::from_secs(5),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("event pool"),
    );
    {
        let writer = event_pool.try_writer().expect("initialize event schema");
        crate::stores::event::ensure_events_schema(writer.conn()).expect("initialize event schema");
    }
    let event_store: Arc<dyn khive_storage::EventStore> = Arc::new(
        crate::stores::event::SqlEventStore::new_scoped(Arc::clone(&event_pool), false, "local"),
    );
    let held_event_writer = event_pool
        .try_writer()
        .expect("hold the event-store writer");

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: 0,
        ..CheckpointConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        checkpoint_pool,
        cfg,
        Some(CheckpointLifecycleOwner::new(event_store, "local")),
        shutdown_rx,
        true,
    ));

    let progressed = wait_for(Duration::from_secs(2), || {
        checkpoint_pressure_elevated_ticks() >= 10
    })
    .await;
    assert!(
        progressed,
        "checkpoint observations must continue while the lifecycle append is contended"
    );
    assert_eq!(checkpoint_lifecycle_append_attempts(), 1);
    assert_eq!(checkpoint_lifecycle_enqueue_drops(), 0);

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect(
            "the run_checkpoint_task handle must not wait for the event store's \
                 five-second writer checkout",
        )
        .expect("checkpoint task panicked");

    // The bound above is deliberately checkpoint-task-local. Aborting the
    // lifecycle worker cannot cancel the `spawn_blocking` checkout already
    // admitted by `SqlEventStore`; release its fixture contention only
    // after the `run_checkpoint_task` handle has returned.
    drop(held_event_writer);
}

/// A sink error is observable without turning a sustained pressure
/// episode into a retrying primary-store write loop.
#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_task_continues_after_lifecycle_append_failure() {
    reset_checkpoint_metrics_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcome_failing_sink.db");
    let pool = file_pool(&path);
    let store = Arc::new(FakeEventStore::failing());
    let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _tracing_guard = tracing::subscriber::set_default(subscriber);

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: 0,
        ..CheckpointConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        pool,
        cfg,
        Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
        shutdown_rx,
        true,
    ));

    let progressed = wait_for(Duration::from_secs(2), || {
        checkpoint_pressure_elevated_ticks() >= 10
    })
    .await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should remain responsive after sink failure")
        .expect("checkpoint task panicked");

    assert!(
        progressed,
        "a failed append must not terminate or stall the checkpoint task"
    );
    assert_eq!(
        store
            .append_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "a persistent pressure state must not retry one primary-store append per tick"
    );
    assert_eq!(checkpoint_lifecycle_append_attempts(), 1);
    assert_eq!(checkpoint_lifecycle_append_failures(), 1);
    let captured = buffer.lock().unwrap().clone();
    assert!(
        captured
            .iter()
            .any(|event| event.message.as_deref()
                == Some("checkpoint lifecycle event append failed")),
        "lifecycle append failures must remain observable; got: {:?}",
        captured
    );
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn secondary_checkpoint_task_with_lifecycle_ownership_emits_outcome_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secondary_outcome.db");
    let pool = file_pool(&path);
    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: 0,
        ..CheckpointConfig::default()
    };
    let store = Arc::new(FakeEventStore::default());
    let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        pool,
        cfg,
        Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
        shutdown_rx,
        false,
    ));

    // Poll for the first emitted event instead of a fixed sleep (same
    // slowdown-flake class as the stale-sweep test above).
    let emitted = wait_for(Duration::from_secs(10), || {
        !store.events.lock().unwrap().is_empty()
    })
    .await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    assert!(
        emitted,
        "a designated secondary lifecycle owner must append outcome events within the poll \
             deadline"
    );
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_task_emits_nothing_while_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcome_no_emit.db");
    let pool = file_pool(&path);

    // An unreachable warn_pages threshold for this test's tiny WAL: every
    // tick stays below warn, so no event should ever be appended.
    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: u64::MAX,
        ..CheckpointConfig::default()
    };
    let store = Arc::new(FakeEventStore::default());
    let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        pool,
        cfg,
        Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
        shutdown_rx,
        true,
    ));

    tokio::time::sleep(Duration::from_millis(60)).await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    assert!(
        store.events.lock().unwrap().is_empty(),
        "a config that never crosses warn_pages must never append a lifecycle event"
    );
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn checkpoint_task_with_no_event_store_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcome_none_store.db");
    let pool = file_pool(&path);

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: 0,
        ..CheckpointConfig::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

    tokio::time::sleep(Duration::from_millis(40)).await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");
}

// Fix: task-level regressions
// that actually spawn `run_checkpoint_task` and capture its `tracing`
// output, so the wiring at the `tx_age_state.observe(...)` call site
// itself is under test — the pure `TxAgeSweepState` unit tests above
// stay green even if that call site is deleted; these do not. All three
// share `#[serial(tx_registry, checkpoint_skip_metrics)]`: `tx_registry`
// because they read the process-wide registry singleton (see the
// `log_tx_registry_oldest_debug_reports_oldest_open_entry` doc comment
// above for why other tests in this same binary can transiently touch
// it too), `checkpoint_skip_metrics` because they spawn the real task
// that updates the module's skip-tracking atomics.

/// (1) A stale labeled entry with a healthy WAL: the spawned task itself
/// must sweep and escalate it to `Stale`, with WAL-pressure thresholds
/// set unreachably high so only the age sweep — never the WAL-pressure
/// ladder — could be responsible for the captured emission.
#[tokio::test]
#[serial(tx_registry, checkpoint_skip_metrics)]
async fn checkpoint_task_sweeps_stale_registry_entry_while_wal_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx_age_sweep_task_healthy_wal.db");
    let pool = file_pool(&path);

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _tracing_guard = tracing::subscriber::set_default(subscriber);

    let _tx_handle = khive_storage::tx_registry::register(Some(
        "checkpoint_task_healthy_wal_sweep_test".to_string(),
    ));

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        warn_pages: u64::MAX,
        high_water_pages: u64::MAX,
        truncate_high_water_pages: u64::MAX,
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(1),
        ..CheckpointConfig::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

    // Poll for the sweep instead of a fixed sleep: a fixed wall-clock
    // budget assumes the spawned task completes its tick (registry scan
    // + walpin sidecar heartbeat) within that window, which widens and
    // flakes under slowdown (coverage instrumentation, contended CI
    // runners) — see `wait_for`'s doc comment for the same reasoning
    // applied to the sibling walpin tests.
    let swept = wait_for(Duration::from_secs(10), || {
        buffer.lock().unwrap().iter().any(|e| {
            e.tx_label.as_deref() == Some("checkpoint_task_healthy_wal_sweep_test")
                && e.message
                    .as_deref()
                    .is_some_and(|m| m.contains("stale-op cap"))
        })
    })
    .await;

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    drop(_tx_handle);

    let events = buffer.lock().unwrap();
    assert!(
        swept,
        "expected the spawned task to sweep and escalate the stale registry entry \
             to Stale on its own within the poll deadline, got: {events:?}"
    );
}

/// (2) An empty registry must never produce a Plank 1 age emission from
/// the real spawned task, mirroring the pure
/// `tx_age_sweep_empty_registry_emits_nothing` unit test above.
#[tokio::test]
#[serial(tx_registry, checkpoint_skip_metrics)]
async fn checkpoint_task_emits_no_age_alert_for_an_empty_registry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx_age_sweep_task_empty_registry.db");
    let pool = file_pool(&path);

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _tracing_guard = tracing::subscriber::set_default(subscriber);

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(1),
        ..CheckpointConfig::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

    tokio::time::sleep(Duration::from_millis(40)).await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    let events = buffer.lock().unwrap();
    assert!(
        events.iter().all(|e| e
            .message
            .as_deref()
            .is_none_or(|m| !m.contains("ADR-091 Plank 1"))),
        "an empty registry must never produce a Plank 1 age emission, got: {events:?}"
    );
}

/// (3) High-finding regression: a Skipped tick must NOT silence the age
/// sweep. Since the dedicated-connection fix, holding the pool's writer
/// mutex no longer produces a Skipped tick at all (see
/// `checkpoint_once_proceeds_and_can_attempt_truncate_while_pool_writer_held`),
/// so this drives Skipped the way it now actually happens in production:
/// a read-only pool, on which `ConnectionPool::open_standalone_writer`
/// always fails, so `CheckpointConnection::ensure_open` can never open a
/// dedicated connection and every tick reports `Skipped`. Asserts the age
/// alert still fires across several such ticks alongside a stale
/// registered entry. Before the original fix (#845 predecessor), the
/// sweep call sat after the `Skipped` early-continue and never ran here;
/// this regression must keep holding under the new skip mechanism too.
#[tokio::test]
#[serial(tx_registry, checkpoint_skip_metrics)]
async fn checkpoint_task_sweeps_stale_entry_even_when_dedicated_connection_is_unavailable_every_tick(
) {
    reset_checkpoint_metrics_for_tests();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx_age_sweep_task_conn_unavailable.db");
    {
        // Seed the schema with an ordinary read-write pool, then drop it
        // (releasing its connections) before reopening the same file
        // read-only below.
        let seed_pool = file_pool(&path);
        let writer = seed_pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);")
            .unwrap();
    }

    #[cfg(unix)]
    {
        khive_storage::test_support::freeze_snapshot_sidecars(&path);
    }

    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            read_only: true,
            ..PoolConfig::for_test()
        })
        .expect("read-only pool open"),
    );
    assert!(
        pool.open_standalone_writer().is_err(),
        "test precondition: a read-only pool must never be able to open a dedicated \
             checkpoint connection"
    );

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _tracing_guard = tracing::subscriber::set_default(subscriber);

    let _tx_handle = khive_storage::tx_registry::register(Some(
        "checkpoint_task_conn_unavailable_sweep_test".to_string(),
    ));

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(1),
        ..CheckpointConfig::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        cfg,
        None,
        shutdown_rx,
        true,
    ));

    // Wait until the task has actually recorded a Skipped tick rather
    // than sleeping a fixed real-time budget: each tick also does
    // registry queries and sidecar filesystem writes, so under
    // instrumented (coverage) or loaded runners a fixed sleep races the
    // first completed tick. Bounded, fail-loud.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while checkpoint_skipped_ticks() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "test setup must actually drive at least one Skipped tick for this \
                 regression to mean anything (none within 10s)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    loop {
        let events = buffer.lock().unwrap().clone();
        if events.iter().any(|e| {
            e.tx_label.as_deref() == Some("checkpoint_task_conn_unavailable_sweep_test")
                && e.message
                    .as_deref()
                    .is_some_and(|m| m.contains("stale-op cap"))
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected the age sweep to fire even though every tick's dedicated \
                 connection was unavailable within 10s, got: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");
    drop(_tx_handle);
}

// ── ADR-091 Amendment 2: Plank A (session sweep), Plank B (walpin
// sidecar), Plank C (backfill-gap probe) ─────────────────────────────

#[tokio::test]
async fn session_sweep_task_exits_on_shutdown_signal() {
    let cfg = SessionSweepConfig {
        interval: Duration::from_millis(10),
        ..SessionSweepConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_session_sweep_task(Vec::new(), cfg, shutdown_rx));

    shutdown_tx.send(()).expect("send shutdown signal");

    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("session sweep task should exit within 1s")
        .expect("session sweep task panicked");
}

/// Bounded condition poll for filesystem effects of the async sweep
/// task — fixed sleeps flake under parallel test load because sidecar
/// writes fsync.
pub(super) async fn wait_for(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

#[tokio::test]
#[serial(khive_walpin_sidecar_env)]
async fn walpin_observe_drops_beacon_when_heartbeat_write_fails() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("observe_gate.db");
    let sidecar_dir = crate::walpin::sidecar_dir_for(&db_path);

    let mut state = WalpinSidecarState::new(
        Some(db_path.as_path()),
        true,
        "session",
        Duration::from_millis(500),
    )
    .expect("sidecar enabled for a file-backed path");
    let pid = std::process::id();
    state.register_beacon().await;
    let beacon_path = sidecar_dir.join(format!("{pid}.beacon"));
    let before = std::fs::metadata(&beacon_path)
        .expect("register_beacon must create the beacon file")
        .modified()
        .unwrap();

    // Force the heartbeat write to fail without touching directory
    // permissions (which would confound with the dir-mode validation):
    // occupy the exclusive-create temp name with a directory, so the
    // tolerant unlink and the O_EXCL create both fail.
    let obstruction = sidecar_dir.join(format!(".{pid}.json.tmp"));
    std::fs::create_dir(&obstruction).unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;
    let over_threshold = Some(khive_storage::tx_registry::OldestSpan {
        id: khive_storage::tx_registry::TxId(1),
        age: Duration::from_secs(60),
        label: None,
        origin: khive_storage::tx_registry::TxOrigin::Unscoped,
    });
    state
        .observe(over_threshold.clone(), Duration::from_secs(30))
        .await;

    assert!(
        !sidecar_dir.join(format!("{pid}.json")).exists(),
        "heartbeat write must have failed"
    );
    // Skipping the refresh alone would leave `before` fresh inside the
    // three-tick window; the fail-closed contract removes the beacon.
    assert!(
        !beacon_path.exists(),
        "a failed heartbeat write must remove the beacon — a still-fresh \
             beacon with no heartbeat would classify registered-silent \
             (before-mtime {before:?})"
    );

    // Recovery: clear the obstruction; the next over-threshold tick
    // writes the heartbeat and re-registers the beacon.
    std::fs::remove_dir(&obstruction).unwrap();
    state.observe(over_threshold, Duration::from_secs(30)).await;
    assert!(
        sidecar_dir.join(format!("{pid}.json")).exists(),
        "heartbeat must land once the write path recovers"
    );
    assert!(
        beacon_path.exists(),
        "beacon must re-register on the first healthy tick after removal"
    );
}

#[tokio::test]
#[serial(khive_walpin_sidecar_env)]
async fn walpin_observe_touches_mtime_without_rewriting_body_when_content_unchanged() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("observe_touch.db");
    let sidecar_dir = crate::walpin::sidecar_dir_for(&db_path);

    let mut state = WalpinSidecarState::new(
        Some(db_path.as_path()),
        true,
        "session",
        Duration::from_millis(500),
    )
    .expect("sidecar enabled for a file-backed path");
    let pid = std::process::id();
    let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
    let span = khive_storage::tx_registry::OldestSpan {
        id: khive_storage::tx_registry::TxId(1),
        age: Duration::from_secs(60),
        label: None,
        origin: khive_storage::tx_registry::TxOrigin::Unscoped,
    };

    state
        .observe(Some(span.clone()), Duration::from_secs(30))
        .await;
    let body_after_create = std::fs::read(&heartbeat_path).expect("heartbeat written");

    // Backdate the mtime so the touch is unambiguous: if `observe`
    // rewrote the body instead of touching it, the write would also
    // reset the mtime, making this assertion pass for the wrong reason —
    // the body-byte comparison below is what actually distinguishes
    // touch from rewrite.
    let backdated = std::time::SystemTime::now() - Duration::from_secs(120);
    // `set_modified` needs write access to the handle on Windows (a
    // read-only open succeeds but is refused by `set_modified` with
    // `PermissionDenied`); Unix accepts a read-only handle for this.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&heartbeat_path)
        .unwrap()
        .set_modified(backdated)
        .unwrap();

    state.observe(Some(span), Duration::from_secs(30)).await;

    let body_after_second_observe =
        std::fs::read(&heartbeat_path).expect("heartbeat still present");
    assert_eq!(
        body_after_create, body_after_second_observe,
        "unchanged oldest-span identity/label/attribution/cadence must touch mtime, \
             not rewrite the body"
    );
    let mtime_after = std::fs::metadata(&heartbeat_path)
        .unwrap()
        .modified()
        .unwrap();
    assert!(
        mtime_after > backdated,
        "the touch must advance mtime past the backdated value"
    );
}

#[tokio::test]
#[serial(khive_walpin_sidecar_env)]
async fn walpin_observe_recreates_heartbeat_after_it_is_deleted_while_span_still_live() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("observe_recreate.db");
    let sidecar_dir = crate::walpin::sidecar_dir_for(&db_path);

    let mut state = WalpinSidecarState::new(
        Some(db_path.as_path()),
        true,
        "session",
        Duration::from_millis(500),
    )
    .expect("sidecar enabled for a file-backed path");
    let pid = std::process::id();
    let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
    let span = khive_storage::tx_registry::OldestSpan {
        id: khive_storage::tx_registry::TxId(1),
        age: Duration::from_secs(60),
        label: None,
        origin: khive_storage::tx_registry::TxOrigin::Unscoped,
    };

    state
        .observe(Some(span.clone()), Duration::from_secs(30))
        .await;
    assert!(heartbeat_path.exists(), "heartbeat written on first tick");

    // Simulate enumeration deleting a slow writer's heartbeat while its
    // span is still live: the next tick still sees unchanged content
    // (same span, same label, same attribution, same cadence) so it
    // takes the touch path — which must detect the missing target and
    // fall through to a full recreate rather than silently no-op.
    std::fs::remove_file(&heartbeat_path).unwrap();
    assert!(!heartbeat_path.exists());

    state.observe(Some(span), Duration::from_secs(30)).await;

    assert!(
        heartbeat_path.exists(),
        "a touch failure against a deleted heartbeat must recreate it via a full write"
    );
    let recreated: crate::walpin::WalpinHeartbeat =
        serde_json::from_slice(&std::fs::read(&heartbeat_path).unwrap()).unwrap();
    assert_eq!(recreated.pid, pid);
    assert_eq!(recreated.oldest_tx_age_secs, 60.0);
}

#[tokio::test]
#[serial(tx_registry, khive_walpin_sidecar_env)]
async fn session_sweep_task_writes_and_clears_walpin_heartbeat() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("session_sweep.db");
    let pool = file_pool(&db_path);
    let sidecar_dir =
        crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed pool"));

    let cfg = SessionSweepConfig {
        interval: Duration::from_millis(10),
        tx_warn_secs: Duration::from_millis(20),
        tx_max_age_secs: Duration::from_millis(500),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_session_sweep_task(
        vec![SweepBackend {
            pool: Arc::clone(&pool),
            is_main: true,
        }],
        cfg,
        shutdown_rx,
    ));

    // No open span yet: a quiet process must write no *heartbeat*, but
    // it DOES register its one-time beacon at startup (ADR-091
    // Amendment 2 sidecar-health attribution) — the sidecar dir is not
    // empty, only heartbeat-free. Poll-wait rather than a fixed sleep:
    // the first tick fsyncs the beacon, and under parallel test load
    // that write can take longer than any small fixed window.
    let pid = std::process::id();
    let beacon = crate::walpin::beacon_path(&sidecar_dir, pid);
    let beacon_registered = wait_for(Duration::from_secs(2), || beacon.exists()).await;
    assert!(
        beacon_registered,
        "a quiet process must still register its one-time beacon"
    );
    assert!(
        !sidecar_dir.join(format!("{pid}.json")).exists(),
        "a quiet process must not write a walpin heartbeat"
    );

    let tx_handle =
        khive_storage::tx_registry::register(Some("session_sweep_walpin_test".to_string()));
    let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
    assert!(
        wait_for(Duration::from_secs(2), || heartbeat_path.exists()).await,
        "expected a walpin heartbeat once the span crossed tx_warn_secs"
    );
    let body = std::fs::read_to_string(&heartbeat_path).unwrap();
    let hb: crate::walpin::WalpinHeartbeat = serde_json::from_str(&body).unwrap();
    assert_eq!(hb.pid, pid);
    assert_eq!(hb.process_role, "session");
    assert_eq!(
        hb.oldest_tx_label.as_deref(),
        Some("session_sweep_walpin_test")
    );
    assert_eq!(
        hb.attribution_basis.as_deref(),
        Some("fallback"),
        "an Unscoped span observed only through the main view's fallback \
             must carry attribution_basis=\"fallback\", never \"origin\""
    );

    drop(tx_handle);
    assert!(
        wait_for(Duration::from_secs(2), || !heartbeat_path.exists()).await,
        "heartbeat must be removed once the stale span clears"
    );

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("session sweep task should exit within 1s")
        .expect("session sweep task panicked");
}

/// ADR-091 Amendment 3 fan-out: two file-backed pools in one process,
/// each its own `SweepBackend`. A span scoped to the SECONDARY pool's
/// own origin must produce a heartbeat only in the secondary's sidecar
/// — never the main backend's — and, because a `Secondary` filter never
/// falls back to `Unscoped`, its heartbeat carries the evidence-backed
/// `attribution_basis="origin"`. Uses the `graph_traverse_read` label
/// (`stores/graph.rs`'s `traverse`) — the design note's own example of
/// "the most WAL-pin-relevant span in the store" — as the registered
/// span's label, so this doubles as coverage that a traversal read span
/// surfaces correctly in a secondary backend's filtered view.
#[tokio::test]
#[serial(tx_registry, khive_walpin_sidecar_env)]
async fn session_sweep_fan_out_scopes_secondary_span_to_secondary_sidecar_only() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let main_dir = tempfile::tempdir().unwrap();
    let secondary_dir = tempfile::tempdir().unwrap();
    let main_pool = file_pool(&main_dir.path().join("main.db"));
    let secondary_pool = file_pool(&secondary_dir.path().join("secondary.db"));
    let main_sidecar =
        crate::walpin::sidecar_dir_for(main_pool.canonical_path().expect("file-backed"));
    let secondary_sidecar =
        crate::walpin::sidecar_dir_for(secondary_pool.canonical_path().expect("file-backed"));

    let cfg = SessionSweepConfig {
        interval: Duration::from_millis(10),
        tx_warn_secs: Duration::from_millis(20),
        tx_max_age_secs: Duration::from_millis(500),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_session_sweep_task(
        vec![
            SweepBackend {
                pool: Arc::clone(&main_pool),
                is_main: true,
            },
            SweepBackend {
                pool: Arc::clone(&secondary_pool),
                is_main: false,
            },
        ],
        cfg,
        shutdown_rx,
    ));

    let pid = std::process::id();
    let secondary_heartbeat = secondary_sidecar.join(format!("{pid}.json"));
    let main_heartbeat = main_sidecar.join(format!("{pid}.json"));

    let tx_handle = khive_storage::tx_registry::register_scoped(
        Some("graph_traverse_read".to_string()),
        secondary_pool.origin(),
    );
    assert!(
        wait_for(Duration::from_secs(2), || secondary_heartbeat.exists()).await,
        "expected a walpin heartbeat in the secondary backend's own sidecar"
    );
    assert!(
        !main_heartbeat.exists(),
        "a span scoped to the secondary backend's origin must never produce \
             a heartbeat in the main backend's sidecar"
    );

    let body = std::fs::read_to_string(&secondary_heartbeat).unwrap();
    let hb: crate::walpin::WalpinHeartbeat = serde_json::from_str(&body).unwrap();
    assert_eq!(hb.oldest_tx_label.as_deref(), Some("graph_traverse_read"));
    assert_eq!(
        hb.attribution_basis.as_deref(),
        Some("origin"),
        "a Secondary-view winner is always Database-origin-backed — never fallback"
    );

    drop(tx_handle);
    assert!(
        wait_for(Duration::from_secs(2), || !secondary_heartbeat.exists()).await,
        "secondary heartbeat must be removed once its span clears"
    );
    assert!(
        !main_heartbeat.exists(),
        "the main sidecar must have stayed untouched for the whole tick sequence"
    );

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("session sweep task should exit within 1s")
        .expect("session sweep task panicked");
}

/// ADR-091 Amendment 3: a `run_checkpoint_task` instance for backend A
/// (`is_main: false`, a `Secondary` filter scoped to A's own identity)
/// must never observe a span registered against a DIFFERENT backend's
/// `Database` origin, nor an `Unscoped` span — a `Secondary` filter never
/// falls back to `Unscoped` (that fallback is the main view's alone).
/// Drives the real task for several ticks and asserts neither the
/// captured `tracing` emissions nor backend A's own sidecar ever name
/// either span.
#[tokio::test]
#[serial(tx_registry, checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn checkpoint_task_ignores_span_registered_against_other_backend_origin_and_unscoped() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let pool_a = file_pool(&dir_a.path().join("backend_a.db"));
    // Only used to mint a real, distinct `DbIdentity` for backend B — no
    // checkpoint task is spawned for it.
    let pool_b = file_pool(&dir_b.path().join("backend_b.db"));
    let sidecar_a = crate::walpin::sidecar_dir_for(pool_a.canonical_path().expect("file-backed"));

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _tracing_guard = tracing::subscriber::set_default(subscriber);

    let _b_origin_handle = khive_storage::tx_registry::register_scoped(
        Some("b_origin_span_ignored_by_a".to_string()),
        pool_b.origin(),
    );
    let _unscoped_handle = khive_storage::tx_registry::register(Some(
        "unscoped_span_ignored_by_secondary".to_string(),
    ));

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        tx_warn_secs: Duration::from_millis(1),
        tx_max_age_secs: Duration::from_millis(1),
        ..CheckpointConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        pool_a,
        cfg,
        None,
        shutdown_rx,
        false, // is_main: backend A is a secondary backend here
    ));

    // No positive condition to poll for — this asserts an absence over a
    // bounded run of several ticks, mirroring
    // `checkpoint_task_emits_no_age_alert_for_an_empty_registry`'s same
    // fixed-window shape (there is nothing to wait-until for a negative).
    tokio::time::sleep(Duration::from_millis(60)).await;
    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    let events = buffer.lock().unwrap();
    assert!(
        events.iter().all(|e| {
            e.tx_label.as_deref() != Some("b_origin_span_ignored_by_a")
                && e.tx_label.as_deref() != Some("unscoped_span_ignored_by_secondary")
        }),
        "backend A's Secondary filter must never emit an age alert naming a span \
             registered against a different backend's origin or an Unscoped span, got: \
             {events:?}"
    );
    assert!(
        !sidecar_a
            .join(format!("{}.json", std::process::id()))
            .exists(),
        "backend A's own sidecar must never gain a heartbeat from a span it does not own"
    );
}

/// ADR-091 Amendment 3: a secondary backend's own `run_checkpoint_task`
/// must detect a stall on its OWN backend (never main-only ownership) —
/// both the Plank 1 age-sweep emission and the sidecar heartbeat, with
/// `attribution_basis="origin"` (a `Secondary` filter winner is always
/// `Database`-origin-backed, never the `Unscoped` fallback) and a
/// nonzero reflected age.
#[tokio::test]
#[serial(tx_registry, checkpoint_skip_metrics, khive_walpin_sidecar_env)]
async fn checkpoint_task_detects_and_enumerates_secondary_backend_stall() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "1");
    }) {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("secondary_stall.db"));
    let sidecar_dir = crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed"));

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };
    let _tracing_guard = tracing::subscriber::set_default(subscriber);

    let tx_handle = khive_storage::tx_registry::register_scoped(
        Some("secondary_stall_test".to_string()),
        pool.origin(),
    );

    let cfg = CheckpointConfig {
        interval: Duration::from_millis(10),
        tx_warn_secs: Duration::from_millis(5),
        tx_max_age_secs: Duration::from_millis(500),
        ..CheckpointConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let pid = std::process::id();
    let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
    let handle = tokio::spawn(run_checkpoint_task(
        pool,
        cfg,
        None,
        shutdown_rx,
        false, // is_main: this is a secondary backend's own checkpoint task
    ));

    assert!(
        wait_for(Duration::from_secs(2), || heartbeat_path.exists()).await,
        "expected a walpin heartbeat once the secondary backend's own span crossed \
             tx_warn_secs"
    );
    let body = std::fs::read_to_string(&heartbeat_path).unwrap();
    let hb: crate::walpin::WalpinHeartbeat = serde_json::from_str(&body).unwrap();
    assert_eq!(hb.oldest_tx_label.as_deref(), Some("secondary_stall_test"));
    assert_eq!(
        hb.attribution_basis.as_deref(),
        Some("origin"),
        "a Secondary-view winner is always Database-origin-backed — never fallback"
    );
    assert!(
        hb.oldest_tx_age_secs > 0.0,
        "the heartbeat must reflect a nonzero stale age for the secondary backend's own \
             span, got {hb:?}"
    );

    shutdown_tx.send(()).expect("send shutdown signal");
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");

    drop(tx_handle);

    let events = buffer.lock().unwrap();
    assert!(
        events.iter().any(|e| {
            e.tx_label.as_deref() == Some("secondary_stall_test")
                && e.message
                    .as_deref()
                    .is_some_and(|m| m.contains("ADR-091 Plank 1"))
        }),
        "expected the secondary backend's own checkpoint task to emit a Plank 1 age alert \
             for its own stalled span, got: {events:?}"
    );
}

#[test]
fn backfill_gap_arithmetic_against_real_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backfill_gap.db");
    let pool = file_pool(&path);
    let writer = pool.try_writer().expect("acquire writer");
    let conn = writer.conn();

    conn.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
    conn.execute_batch("INSERT INTO t (v) VALUES (1)").unwrap();

    let observation =
        query_backfill_gap(conn).expect("PRAGMA wal_checkpoint(PASSIVE) must succeed");
    // Nothing pins the WAL open in this test (no concurrent reader), so a
    // PASSIVE checkpoint fully drains what it just wrote: the one-row
    // backfill gap (log - checkpointed) is zero.
    assert!(
        observation.log_frames >= observation.checkpointed_frames,
        "checkpointed frames cannot exceed log frames"
    );
    assert_eq!(
        observation.log_frames - observation.checkpointed_frames,
        0,
        "an unpinned WAL must fully checkpoint under PASSIVE"
    );
}

#[test]
fn truncate_no_progress_probe_logs_backfill_gap_without_a_pin_depth_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backfill_gap_log.db");
    let pool = file_pool(&path);
    let writer = pool.try_writer().expect("acquire writer");
    writer
        .conn()
        .execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1)")
        .unwrap();

    let events = capture(|| log_backfill_gap(&pool, writer.conn()));
    let event = events
        .iter()
        .find(|event| {
            event
                .message
                .as_deref()
                .is_some_and(|message| message.contains("WAL backfill gap"))
        })
        .expect("a successful PASSIVE probe logs its backfill gap");
    assert!(event.backfill_gap_frames.is_some());
    assert_eq!(event.legacy_wal_pin_depth, None);
}

#[test]
fn backfill_gap_arithmetic_on_in_memory_pool_errors_cleanly() {
    // In-memory databases report `log = -1` (no WAL); the pragma read
    // itself does not panic and the caller (`log_backfill_gap`) treats
    // any error as a logged warning, never a crash.
    let cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).expect("in-memory pool");
    let writer = pool.try_writer().expect("acquire writer");
    // Either an explicit error or a nonsensical negative `log` value is
    // acceptable here — the requirement is just "does not panic".
    let _ = query_backfill_gap(writer.conn());
}

/// #1849: a canonical filesystem identity is an OS path, not a display
/// label. Distinct non-UTF-8 Unix paths can render to the same lossy
/// string and must still occupy distinct backend telemetry slots.
#[cfg(unix)]
#[test]
fn routine_wal_backend_key_preserves_non_utf8_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let path_a = PathBuf::from(OsString::from_vec(b"/tmp/khive-wal-\x80.db".to_vec()));
    let path_b = PathBuf::from(OsString::from_vec(b"/tmp/khive-wal-\x81.db".to_vec()));
    assert_eq!(
        path_a.display().to_string(),
        path_b.display().to_string(),
        "fixture must reproduce the lossy display-label collision"
    );
    assert_ne!(
        checkpoint_db_key_from_path(Some(&path_a)),
        checkpoint_db_key_from_path(Some(&path_b)),
        "backend keys must retain the canonical path's exact OS bytes"
    );
}

/// #1849: the periodic checkpoint's own PASSIVE row is the monitoring
/// sample. One tick must not issue the old no-arg probe followed by a
/// second PASSIVE, and the stored sample must distinguish logical
/// backlog from the physical sidecar high-water mark.
#[test]
#[serial(checkpoint_skip_metrics)]
fn routine_checkpoint_records_one_pass_logical_and_physical_wal_sample() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("routine_wal_sample.db");
    let pool = file_pool(&path);

    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch(
                "PRAGMA wal_autocheckpoint=0; \
                     CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT); \
                     INSERT INTO t VALUES (0, 'seed');",
            )
            .unwrap();
    }

    let reader =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();

    {
        let writer = pool.try_writer().expect("writer");
        writer.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
        for id in 1..=256_i64 {
            writer
                .conn()
                .execute("INSERT INTO t VALUES (?1, printf('%.*c', 2048, 'x'))", [id])
                .unwrap();
        }
        writer.conn().execute_batch("COMMIT").unwrap();
    }

    let checkpoint_conn = pool.open_standalone_writer().unwrap();
    let pragma_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pragma_calls_from_hook = Arc::clone(&pragma_calls);
    checkpoint_conn
        .authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Pragma { pragma_name, .. }
                    if pragma_name.eq_ignore_ascii_case("wal_checkpoint")
            ) {
                pragma_calls_from_hook.fetch_add(1, Ordering::SeqCst);
            }
            Authorization::Allow
        }))
        .unwrap();

    checkpoint_once(
        &pool,
        &checkpoint_conn,
        &CheckpointConfig::default(),
        &mut TruncateState::default(),
    )
    .unwrap();
    checkpoint_conn
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();

    assert_eq!(
        pragma_calls.load(Ordering::SeqCst),
        1,
        "one routine tick must issue exactly one PASSIVE checkpoint"
    );
    let pinned = routine_wal_observation(&pool).expect("routine sample");
    assert_eq!(
        pinned.busy, 0,
        "a pinned reader is not checkpoint-lock contention"
    );
    let first_timing = checkpoint_timing(&pool);
    assert_eq!(first_timing.ticks, 1);
    assert_eq!(
        first_timing.busy_ticks, 0,
        "pending frames must not count as busy"
    );
    assert!(pinned.log_frames > 0, "the test must create WAL frames");
    assert!(
        pinned.pending_frames > 0,
        "the old reader must leave a logical backlog: {pinned:?}"
    );
    assert_eq!(
        pinned.pending_frames,
        pinned.log_frames.saturating_sub(pinned.checkpointed_frames)
    );
    assert!(
        pinned.physical_wal_bytes.is_some_and(|bytes| bytes > 0),
        "the physical sidecar high-water must be reported separately: {pinned:?}"
    );

    reader.execute_batch("COMMIT").unwrap();
    checkpoint_once(
        &pool,
        &checkpoint_conn,
        &CheckpointConfig::default(),
        &mut TruncateState::default(),
    )
    .unwrap();
    let drained = routine_wal_observation(&pool).expect("drained routine sample");
    let drained_timing = checkpoint_timing(&pool);
    assert_eq!(drained_timing.ticks, first_timing.ticks + 1);
    assert!(drained_timing.elapsed_us_sum >= first_timing.elapsed_us_sum);
    assert!(drained_timing.elapsed_us_max >= first_timing.elapsed_us_max);
    assert_eq!(drained_timing.busy_ticks, 0);
    assert_eq!(drained.pending_frames, 0, "unpinned PASSIVE must drain");
    assert!(
        drained.physical_wal_bytes.is_some_and(|bytes| bytes > 0),
        "PASSIVE may reuse rather than shrink the physical WAL; the two gauges must remain \
             independently visible: {drained:?}"
    );
}

#[test]
#[serial(checkpoint_skip_metrics)]
fn routine_checkpoint_timing_records_real_call_and_debug_fields() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("timed_tick.db"));
    let conn = checkpoint_conn(&pool);
    conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
        .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let release_rx = Mutex::new(release_rx);
    conn.authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
        if matches!(context.action, AuthAction::Pragma { pragma_name, .. }
            if pragma_name.eq_ignore_ascii_case("wal_checkpoint"))
        {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        Authorization::Allow
    }))
    .unwrap();
    let tick_pool = Arc::clone(&pool);
    let tick = std::thread::spawn(move || {
        let mut result = None;
        let events = capture(|| {
            result = Some(checkpoint_once(
                &tick_pool,
                &conn,
                &CheckpointConfig {
                    truncate_high_water_pages: u64::MAX,
                    ..CheckpointConfig::default()
                },
                &mut TruncateState::default(),
            ));
        });
        (result.unwrap(), events)
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let during = checkpoint_timing(&pool);
    release_tx.send(()).unwrap();
    let (result, events) = tick.join().unwrap();
    result.unwrap();
    assert_eq!(
        during.ticks, 0,
        "in-flight call must not publish partial counters"
    );
    let timing = checkpoint_timing(&pool);
    assert_eq!(
        timing.ticks, 1,
        "one actual PASSIVE call must advance the count"
    );
    assert!(
        timing.elapsed_us_sum > 0,
        "channel-held checkpoint call must record elapsed time"
    );
    assert_eq!(timing.elapsed_us_max, timing.elapsed_us_sum);
    assert_eq!(timing.busy_ticks, 0);
    assert_eq!(timing.error_ticks, 0);
    let issued: Vec<_> = events
        .iter()
        .filter(|event| event.message.as_deref() == Some("WAL checkpoint issued"))
        .collect();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].elapsed_us, Some(timing.elapsed_us_sum));
    assert_eq!(issued[0].busy, Some(0));
}

#[test]
#[serial(checkpoint_skip_metrics)]
fn routine_checkpoint_timing_counts_sqlite_busy_from_competing_checkpoint() {
    struct BusyGate {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    static BUSY_GATE: Mutex<Option<BusyGate>> = Mutex::new(None);
    fn hold_checkpoint_lock(_attempt: i32) -> bool {
        let gate = BUSY_GATE
            .lock()
            .unwrap()
            .take()
            .expect("armed busy handler");
        gate.entered.send(()).unwrap();
        gate.release.recv_timeout(Duration::from_secs(5)).unwrap();
        false
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy_checkpoint.db");
    let pool = file_pool(&path);
    let conn = checkpoint_conn(&pool);
    conn.execute_batch(
        "PRAGMA wal_autocheckpoint=0; CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);",
    )
    .unwrap();
    let reader =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    conn.execute_batch("INSERT INTO t VALUES (2);").unwrap();
    let config = CheckpointConfig {
        truncate_high_water_pages: u64::MAX,
        ..CheckpointConfig::default()
    };
    let first_pages = checkpoint_once(&pool, &conn, &config, &mut TruncateState::default())
        .expect("initial PASSIVE observation");
    assert!(first_pages > 0);
    let first_sample = routine_wal_observation(&pool).expect("initial routine sample");

    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    *BUSY_GATE.lock().unwrap() = Some(BusyGate {
        entered: entered_tx,
        release: release_rx,
    });
    let competing = std::thread::spawn(move || {
        let checkpoint = rusqlite::Connection::open(path).unwrap();
        checkpoint.busy_handler(Some(hold_checkpoint_lock)).unwrap();
        checkpoint.query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
            row.get::<_, i64>(0)
        })
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("FULL checkpoint holds CKPT lock while waiting on reader");
    let mut result = None;
    let events = capture(|| {
        result = Some(checkpoint_once(
            &pool,
            &conn,
            &config,
            &mut TruncateState::default(),
        ));
        log_backfill_gap(&pool, &conn);
    });
    release_tx.send(()).unwrap();
    let competing_busy = competing.join().unwrap().unwrap();
    reader.execute_batch("COMMIT").unwrap();
    assert!(matches!(
        result.unwrap(),
        Err(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DatabaseBusy
    ));
    assert_eq!(competing_busy, 1);
    assert_eq!(
        routine_wal_observation(&pool),
        Some(first_sample),
        "a real busy row must preserve the last valid routine sample"
    );
    assert_eq!(last_observed_wal_pages(), Some(first_pages));
    let timing = checkpoint_timing(&pool);
    assert_eq!(timing.ticks, 2);
    assert_eq!(
        timing.busy_ticks, 1,
        "SQLite busy result must increment busy ticks"
    );
    assert_eq!(timing.error_ticks, 0);
    let issued = events
        .iter()
        .find(|event| {
            event.message.as_deref()
                == Some("WAL PASSIVE checkpoint returned a busy row; frame observation unavailable")
        })
        .expect("busy tick record");
    assert_eq!(issued.busy, Some(1), "tick record must retain SQLite busy");
    assert!(issued.elapsed_us.is_some());
    let gap = events
        .iter()
        .find(|event| {
            event.message.as_deref()
                == Some("ADR-091 Plank C: WAL backfill gap unavailable after TRUNCATE")
        })
        .expect("busy probe must report an unavailable backfill gap");
    assert_eq!(gap.backfill_gap_frames, None);
}

#[test]
#[serial(checkpoint_skip_metrics)]
fn routine_checkpoint_timing_counts_errors_and_excludes_post_truncate_probes() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("failed_tick.db"));
    let conn = checkpoint_conn(&pool);
    conn.authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
        if matches!(context.action, AuthAction::Pragma { pragma_name, .. }
            if pragma_name.eq_ignore_ascii_case("wal_checkpoint"))
        {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))
    .unwrap();
    let result = checkpoint_once(
        &pool,
        &conn,
        &CheckpointConfig::default(),
        &mut TruncateState::default(),
    );
    conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(result.is_err());
    let timing = checkpoint_timing(&pool);
    assert_eq!(timing.ticks, 1);
    assert_eq!(timing.error_ticks, 1);
    assert_eq!(timing.busy_ticks, 0);
    query_wal_pages(&pool, &conn);
    assert_eq!(
        checkpoint_timing(&pool),
        timing,
        "post-TRUNCATE observation is not a routine tick"
    );
}

#[test]
#[serial(checkpoint_skip_metrics)]
fn failed_post_truncate_measurement_preserves_last_sample_and_failure_streak() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("unmeasured_truncate.db"));
    let conn = checkpoint_conn(&pool);
    let previous_pages = LAST_WAL_PAGES.swap(73, Ordering::Relaxed);
    let previous_attempts = TRUNCATE_ATTEMPTS.load(Ordering::Relaxed);
    let previous_failures = TRUNCATE_CONSECUTIVE_FAILURES.load(Ordering::Relaxed);
    let checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let authorizer_calls = Arc::clone(&checkpoint_calls);
    conn.authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
        if matches!(context.action, AuthAction::Pragma { pragma_name, .. }
            if pragma_name.eq_ignore_ascii_case("wal_checkpoint"))
        {
            // Allow TRUNCATE, then fail exactly its post-attempt PASSIVE
            // re-measurement. The later backfill-gap probe may still run.
            if authorizer_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        } else {
            Authorization::Allow
        }
    }))
    .unwrap();
    let config = CheckpointConfig {
        warn_pages: 10,
        truncate_high_water_pages: 0,
        truncate_min_interval: Duration::ZERO,
        ..CheckpointConfig::default()
    };
    let mut state = TruncateState {
        consecutive_failures: 2,
        ..TruncateState::default()
    };
    let events = capture(|| {
        let _ = maybe_truncate(&pool, &conn, &config, 73, &mut state);
    });
    conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(checkpoint_calls.load(Ordering::SeqCst) >= 2);
    assert!(state.last_attempt.is_some());
    assert_eq!(truncate_attempts(), previous_attempts + 1);
    assert_eq!(last_observed_wal_pages(), Some(73));
    assert_eq!(state.consecutive_failures, 2);
    assert_eq!(truncate_consecutive_failures(), 2);
    assert!(events.iter().any(|event| event.message.as_deref()
        == Some("WAL TRUNCATE progress unmeasured; checking possible holders in this process and others")));
    assert!(truncate_needs_attribution(73, None));
    assert!(truncate_needs_attribution(73, Some(73)));
    assert!(!truncate_needs_attribution(73, Some(0)));

    let events = capture(|| note_truncate_outcome(&config, Some(20), &mut state));
    assert_eq!(state.consecutive_failures, 3);
    assert_eq!(truncate_consecutive_failures(), 3);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.message.as_deref()
                == Some("WAL TRUNCATE has failed to clear WAL pressure for 3 consecutive attempts"))
            .count(),
        1,
        "unmeasured attempt must preserve the streak for the next measured failure"
    );
    LAST_WAL_PAGES.store(previous_pages, Ordering::Relaxed);
    TRUNCATE_ATTEMPTS.store(previous_attempts, Ordering::Relaxed);
    TRUNCATE_CONSECUTIVE_FAILURES.store(previous_failures, Ordering::Relaxed);
}

#[test]
fn checkpoint_timing_accumulates_per_store_and_saturates() {
    let dir = tempfile::tempdir().unwrap();
    let a = file_pool(&dir.path().join("a.db"));
    let b = file_pool(&dir.path().join("b.db"));
    record_checkpoint_timing(&a, 17, Some(0));
    record_checkpoint_timing(&a, 31, Some(1));
    record_checkpoint_timing(&a, 7, None);
    record_checkpoint_timing(&b, 3, Some(0));
    assert_eq!(
        checkpoint_timing(&a),
        CheckpointTiming {
            ticks: 3,
            elapsed_us_sum: 55,
            elapsed_us_max: 31,
            busy_ticks: 1,
            error_ticks: 1,
        }
    );
    assert_eq!(
        checkpoint_timing(&b),
        CheckpointTiming {
            ticks: 1,
            elapsed_us_sum: 3,
            elapsed_us_max: 3,
            busy_ticks: 0,
            error_ticks: 0,
        }
    );
    checkpoint_timings().lock().unwrap().insert(
        checkpoint_db_key(&a),
        CheckpointTiming {
            ticks: u64::MAX,
            elapsed_us_sum: u64::MAX,
            elapsed_us_max: 31,
            busy_ticks: u64::MAX,
            error_ticks: u64::MAX,
        },
    );
    record_checkpoint_timing(&a, 1, Some(1));
    assert_eq!(
        checkpoint_timing(&a).elapsed_us_sum,
        u64::MAX,
        "elapsed sum must saturate on the first overflowing addition"
    );
    record_checkpoint_timing(&a, u64::MAX, None);
    assert_eq!(
        checkpoint_timing(&a),
        CheckpointTiming {
            ticks: u64::MAX,
            elapsed_us_sum: u64::MAX,
            elapsed_us_max: u64::MAX,
            busy_ticks: u64::MAX,
            error_ticks: u64::MAX,
        }
    );
}
