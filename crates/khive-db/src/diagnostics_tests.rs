use serial_test::serial;

use super::*;
use crate::pool::{ConnectionPool, PoolConfig, WalCeilingPolicy, WalCeilingSource};

include!("diagnostics/wal_ceiling_tests.rs");
include!("diagnostics/environment_tests.rs");
include!("diagnostics_census_evidence_tests.rs");

fn seeded_pool(dir: &tempfile::TempDir) -> (ConnectionPool, PathBuf) {
    let path = dir.path().join("diag.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    {
        let writer = pool.try_writer().expect("writer");
        writer
                .conn()
                .execute_batch(
                    "CREATE TABLE t (x INTEGER); \
                     CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER, merged_into TEXT); \
                     CREATE TABLE graph_edges (
                         namespace TEXT NOT NULL,
                         id TEXT NOT NULL,
                         PRIMARY KEY (namespace, id)
                     ); \
                     CREATE TABLE graph_edges_seq (
                         seq INTEGER PRIMARY KEY AUTOINCREMENT,
                         edge_id TEXT NOT NULL UNIQUE
                     ); \
                     CREATE VIRTUAL TABLE fts_entities USING fts5(
                         namespace UNINDEXED, subject_id UNINDEXED, title, body,
                         tokenize='trigram'
                     ); \
                     CREATE VIRTUAL TABLE fts_notes USING fts5(
                         namespace UNINDEXED, subject_id UNINDEXED, title, body,
                         tokenize='trigram'
                     ); \
                     INSERT INTO t VALUES (1), (2), (3); \
                     INSERT INTO fts_entities(namespace, subject_id, title, body)
                         VALUES('local', 'entity-1', 'entity title', 'entity diagnostic body'); \
                     INSERT INTO fts_notes(namespace, subject_id, title, body)
                         VALUES('local', 'note-1', 'note title', 'note diagnostic body');",
                )
                .expect("seed writes");
    }
    (pool, path)
}

#[test]
fn dropping_census_future_guard_requests_cooperative_stop() {
    let stopped = Arc::new(AtomicBool::new(false));
    let guard = StopCensusOnDrop {
        stopped: Arc::clone(&stopped),
        armed: true,
    };

    drop(guard);

    assert!(
        stopped.load(Ordering::SeqCst),
        "dropping diagnostics while its census worker is live must stop the PID/fd walk"
    );
}

#[tokio::test]
async fn runtime_audit_batch_fields_are_additive_and_unavailable_without_a_control() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _path) = seeded_pool(&dir);
    let pool = Arc::new(pool);

    let without_control = collect_with_audit_append_failures_interruptibly(
        Arc::clone(&pool),
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
        0,
    )
    .await
    .expect("diagnostics succeed");
    assert!(without_control
        .writer_contention
        .audit_batch_flush_failures
        .is_none());
    assert!(
        without_control
            .writer_contention
            .audit_batch_flush_failures_unavailable_reason
            .is_some(),
        "no audit-batch control was supplied, so the field must carry a reason, not a \
             fabricated zero"
    );
    assert!(without_control
        .writer_contention
        .audit_degraded_rows
        .is_none());
    assert!(without_control.writer_contention.audit_degraded.is_none());

    let with_control = collect_with_runtime_audit_metrics_interruptibly(
        Arc::clone(&pool),
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
        0,
        Some(RuntimeAuditBatchMetrics {
            flush_failures: 3,
            degraded_rows: 7,
            degraded: true,
            admission_refused_obligations: 5,
            admission_refused_obligations_last_at_ms: Some(1_700_000_000_123),
            retry_exhausted_obligations: 4,
            retry_exhausted_obligations_last_at_ms: Some(1_700_000_000_789),
            admission_unresolved_obligations: 2,
            admission_unresolved_obligations_last_at_ms: Some(1_700_000_000_456),
        }),
    )
    .await
    .expect("diagnostics succeed");
    assert_eq!(
        with_control.writer_contention.audit_batch_flush_failures,
        Some(3)
    );
    assert!(with_control
        .writer_contention
        .audit_batch_flush_failures_unavailable_reason
        .is_none());
    assert_eq!(with_control.writer_contention.audit_degraded_rows, Some(7));
    assert_eq!(with_control.writer_contention.audit_degraded, Some(true));
    assert_eq!(
        with_control
            .writer_contention
            .audit_admission_refused_obligations,
        Some(5),
        "an operator must be able to read the admission-refused obligation count from \
             db_diagnostics without a test-only feature gate (ADR-103 Amendment 3)"
    );
    assert!(with_control
        .writer_contention
        .audit_admission_refused_obligations_unavailable_reason
        .is_none());
    assert_eq!(
        with_control
            .writer_contention
            .audit_admission_unresolved_obligations,
        Some(2),
        "an operator must be able to distinguish enqueued-but-unresolved rows from \
             confirmed-refused rows (ADR-103 Amendment 3)"
    );
    assert!(with_control
        .writer_contention
        .audit_admission_unresolved_obligations_unavailable_reason
        .is_none());
    assert!(without_control
        .writer_contention
        .audit_admission_refused_obligations
        .is_none());
    assert!(without_control
        .writer_contention
        .audit_admission_refused_obligations_unavailable_reason
        .is_some());
    assert!(without_control
        .writer_contention
        .audit_admission_unresolved_obligations
        .is_none());
    assert!(without_control
        .writer_contention
        .audit_admission_unresolved_obligations_unavailable_reason
        .is_some());

    let wire = serde_json::to_value(&with_control.writer_contention).unwrap();
    assert_eq!(wire["audit_retry_exhausted_obligations"], 4);
    assert_eq!(
        wire["audit_retry_exhausted_obligations_last_at_ms"],
        1_700_000_000_789_u64
    );
    assert!(with_control
        .writer_contention
        .audit_retry_exhausted_obligations_unavailable_reason
        .is_none());
    assert!(without_control
        .writer_contention
        .audit_retry_exhausted_obligations
        .is_none());
    assert!(without_control
        .writer_contention
        .audit_retry_exhausted_obligations_unavailable_reason
        .is_some());

    // Existing fields must be unaffected by the new ones — additive, not
    // a reshuffle.
    assert_eq!(
        with_control.writer_contention.writer_acquisitions,
        without_control.writer_contention.writer_acquisitions
    );
}

#[test]
fn writer_task_pool_sourced_counters_are_always_populated_directly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _path) = seeded_pool(&dir);

    let report = collect(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
    );

    // Unlike the runtime-supplied audit-batch fields, these two come
    // straight from the pool's own counters and are never `Option`.
    assert_eq!(report.writer_contention.writer_task_request_failures, 0);
    assert_eq!(report.writer_contention.writer_task_side_effects_unknown, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn request_cancellation_after_passive_stops_before_graph_and_census() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _) = seeded_pool(&dir);
    let pool = Arc::new(pool);
    TEST_REACHED_AFTER_PASSIVE.store(false, Ordering::SeqCst);
    TEST_PAUSE_AFTER_PASSIVE.store(true, Ordering::SeqCst);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let diagnostic_pool = Arc::clone(&pool);
    let task = tokio::spawn(crate::scope_request_read_cancellation(
        cancel_rx,
        async move {
            collect_with_audit_append_failures_interruptibly(
                diagnostic_pool,
                BuildIdentity::from_env("test", None),
                Duration::from_secs(30),
                0,
            )
            .await
        },
    ));

    tokio::time::timeout(Duration::from_secs(1), async {
        while !TEST_REACHED_AFTER_PASSIVE.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("diagnostics never completed its admitted PASSIVE phase");
    cancel_tx.send(true).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancelled diagnostics did not stop promptly")
        .expect("diagnostics task panicked");
    TEST_PAUSE_AFTER_PASSIVE.store(false, Ordering::SeqCst);
    assert!(matches!(result, Err(StorageError::Timeout { .. })));

    let one: i64 = pool
        .reader()
        .expect("diagnostics returned its connection")
        .conn()
        .query_row("SELECT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(one, 1);
}

#[test]
fn checkpoint_probe_returns_a_well_formed_triple_on_a_file_backed_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _path) = seeded_pool(&dir);
    let probe = pool
        .inspect_diagnostic_connection(false)
        .checkpoint_probe
        .expect("probe must succeed on a WAL database");

    assert!(
        probe.busy == 0 || probe.busy == 1,
        "busy is a 0/1 flag, got {}",
        probe.busy
    );
    assert!(
        probe.log_frames >= 0,
        "a WAL database must report a non-negative frame count, got {}",
        probe.log_frames
    );
    assert!(
        probe.checkpointed_frames >= 0,
        "checkpointed frames must be non-negative, got {}",
        probe.checkpointed_frames
    );
    assert!(
        probe.checkpointed_frames <= probe.log_frames,
        "a PASSIVE pass cannot checkpoint more frames than the WAL holds: {probe:?}"
    );
    assert!(
        probe.backfill_gap_frames() >= 0,
        "the one-row backfill gap clamps at 0: {probe:?}"
    );
}

#[cfg(unix)]
#[test]
fn checkpoint_probe_reports_replaced_pool_file_instead_of_probing_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, path) = seeded_pool(&dir);
    let replacement = dir.path().join("replacement.db");
    let replacement_conn = Connection::open(&replacement).expect("replacement database");
    replacement_conn
        .execute_batch("CREATE TABLE replacement_marker (value INTEGER)")
        .expect("initialize replacement database");
    drop(replacement_conn);
    std::fs::rename(&replacement, &path).expect("replace the pool's path");

    let inspection = inspect_pool(&pool);
    assert!(inspection.checkpoint_probe.is_none());
    assert!(
        inspection
            .checkpoint_probe_error
            .as_deref()
            .is_some_and(|error| error.contains("file identity changed")),
        "replacement must be reported as a probe failure"
    );
}

#[test]
fn checkpoint_probe_backfill_gap_is_a_row_difference_not_a_pin_claim() {
    let probe = CheckpointProbe {
        busy: 0,
        log_frames: 12,
        checkpointed_frames: 7,
    };
    assert_eq!(probe.backfill_gap_frames(), 5);
    let no_wal = CheckpointProbe {
        busy: 0,
        log_frames: -1,
        checkpointed_frames: -1,
    };
    assert_eq!(no_wal.backfill_gap_frames(), 0);
    let busy = CheckpointProbe { busy: 1, ..probe };
    assert_eq!(busy.backfill_gap_frames(), 5);
}

#[test]
fn pin_age_does_not_depend_on_the_wall_clock_epoch() {
    let probe = CheckpointProbe {
        busy: 0,
        log_frames: 12,
        checkpointed_frames: 7,
    };
    let future_epoch = checkpoint::CheckpointRunStatus::Observed(checkpoint::CheckpointRun {
        frame: 7,
        first_observed_at_unix_ms: 10_000,
    });
    let past_epoch = checkpoint::CheckpointRunStatus::Observed(checkpoint::CheckpointRun {
        frame: 7,
        first_observed_at_unix_ms: 1,
    });
    let young = checkpoint_pin_diagnostics_for_run(
        Some(&probe),
        None,
        future_epoch,
        Some(Duration::from_millis(10)),
    );
    let forward = checkpoint_pin_diagnostics_for_run(
        Some(&probe),
        None,
        future_epoch,
        Some(Duration::from_millis(1010)),
    );
    let backward = checkpoint_pin_diagnostics_for_run(
        Some(&probe),
        None,
        past_epoch,
        Some(Duration::from_millis(1010)),
    );
    assert_eq!(young.oldest_pinned_frame, None);
    assert_eq!(forward.oldest_pinned_frame, Some(7));
    assert_eq!(forward.pin_depth, Some(5));
    assert_eq!(backward.oldest_pinned_frame, Some(7));
    let busy = CheckpointProbe { busy: 1, ..probe };
    let unavailable = checkpoint_pin_diagnostics_for_run(
        Some(&busy),
        None,
        future_epoch,
        Some(Duration::from_millis(1010)),
    );
    assert_eq!(unavailable.oldest_pinned_frame, None);
    assert_eq!(unavailable.pin_depth, None);
}

#[test]
fn checkpoint_pin_report_waits_one_second_for_a_matching_run() {
    let probe = CheckpointProbe {
        busy: 0,
        log_frames: 12,
        checkpointed_frames: 7,
    };
    let run = checkpoint::CheckpointRunStatus::Observed(checkpoint::CheckpointRun {
        frame: 7,
        first_observed_at_unix_ms: 1_000,
    });

    let young = checkpoint_pin_diagnostics_for_run(
        Some(&probe),
        None,
        run,
        Some(Duration::from_millis(999)),
    );
    assert_eq!(young.backfill_ceiling, Some(7));
    assert_eq!(young.oldest_pinned_frame, None);
    assert!(young
        .oldest_pinned_frame_unavailable_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("less than one second")));
    assert_eq!(young.oldest_pinned_frame_run, None);
    assert_eq!(young.pin_depth, None);
    assert!(young.pin_depth_unavailable_reason.is_some());

    let aged =
        checkpoint_pin_diagnostics_for_run(Some(&probe), None, run, Some(Duration::from_secs(1)));
    assert_eq!(aged.backfill_ceiling, Some(7));
    assert_eq!(aged.oldest_pinned_frame, Some(7));
    assert_eq!(aged.pin_depth, Some(5));
    assert_eq!(aged.pin_depth_unavailable_reason, None);
    assert_eq!(
        aged.oldest_pinned_frame_run,
        Some(match run {
            checkpoint::CheckpointRunStatus::Observed(value) => value,
            _ => unreachable!(),
        })
    );
}

#[test]
fn checkpoint_pin_report_keeps_busy_probe_fields_null_even_with_an_aged_run() {
    let run = checkpoint::CheckpointRunStatus::Observed(checkpoint::CheckpointRun {
        frame: 7,
        first_observed_at_unix_ms: 1,
    });
    for probe in [
        Some(CheckpointProbe {
            busy: 1,
            log_frames: 12,
            checkpointed_frames: 7,
        }),
        Some(CheckpointProbe {
            busy: 0,
            log_frames: -1,
            checkpointed_frames: -1,
        }),
        Some(CheckpointProbe {
            busy: 0,
            log_frames: 12,
            checkpointed_frames: 12,
        }),
    ] {
        let result = checkpoint_pin_diagnostics_for_run(
            probe.as_ref(),
            None,
            run,
            Some(Duration::from_secs(1)),
        );
        assert_eq!(result.backfill_ceiling, None);
        assert!(result.backfill_ceiling_unavailable_reason.is_some());
        assert_eq!(result.oldest_pinned_frame, None);
        assert!(result.oldest_pinned_frame_unavailable_reason.is_some());
        assert_eq!(result.oldest_pinned_frame_run, None);
        assert!(result.oldest_pinned_frame_run_unavailable_reason.is_some());
        assert_eq!(result.pin_depth, None);
        assert!(result.pin_depth_unavailable_reason.is_some());
    }

    let error = checkpoint_pin_diagnostics_for_run(
        None,
        Some("probe failed"),
        run,
        Some(Duration::from_secs(1)),
    );
    assert_eq!(error.backfill_ceiling, None);
    assert_eq!(
        error.backfill_ceiling_unavailable_reason.as_deref(),
        Some("probe failed")
    );
    assert_eq!(error.oldest_pinned_frame, None);
    assert!(error.oldest_pinned_frame_run_unavailable_reason.is_some());
    assert_eq!(error.pin_depth, None);
    assert!(error.pin_depth_unavailable_reason.is_some());
}

#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
#[test]
fn db_diagnostics_reports_start_times_for_all_holders_and_identifies_reporter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _) = seeded_pool(&dir);

    let report = collect(
        &pool,
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let reporter = crate::walpin::reporting_pid();
    let process = report
        .wal_pin
        .census_process_start_times
        .iter()
        .find(|process| process.pid == reporter)
        .expect("the reporting process must remain in the census list");

    assert_eq!(report.wal_pin.reporting_pid, reporter);
    assert_eq!(report.wal_pin.reporting_process_is_holder, Some(true));
    assert_eq!(
        report.wal_pin.census_process_start_times.len(),
        report.wal_pin.census_holder_pids.len(),
        "every confirmed holder gets raw start-time data"
    );
    assert_eq!(
        process.process_start_time_secs,
        crate::walpin::process_start_time_secs(reporter)
    );
    assert_eq!(process.process_start_time_unavailable_reason, None);
    #[cfg(target_os = "linux")]
    assert_eq!(report.wal_pin.start_time_resolution_secs, Some(2));
    #[cfg(target_os = "macos")]
    assert_eq!(report.wal_pin.start_time_resolution_secs, Some(1));
    assert_eq!(
        serde_json::to_value(&report).unwrap()["wal_pin"]["census_process_start_times"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["pid"].as_u64())
            .collect::<Vec<_>>(),
        report
            .wal_pin
            .census_holder_pids
            .iter()
            .map(|pid| u64::from(*pid))
            .collect::<Vec<_>>(),
        "start-time reporting does not filter census holders"
    );
}

/// The verb must not perturb the state it reports: the probe touches none
/// of the ADR-091 counters.
#[test]
#[serial(checkpoint_skip_metrics)]
fn checkpoint_probe_does_not_perturb_the_adr091_counters() {
    crate::checkpoint::reset_checkpoint_metrics_for_tests();
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _path) = seeded_pool(&dir);
    let before = checkpoint_counters();
    for _ in 0..3 {
        pool.inspect_diagnostic_connection(false)
            .checkpoint_probe
            .expect("probe must succeed");
    }
    let after = checkpoint_counters();

    assert_eq!(
        before, after,
        "checkpoint_probe must leave every ADR-091 counter untouched"
    );
}

#[test]
fn wal_file_state_reports_the_sidecar_size_for_a_live_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_pool, path) = seeded_pool(&dir);

    let state = wal_file_state(&path);
    assert!(
        state.wal_path.ends_with("diag.db-wal"),
        "WAL path is the db path plus a -wal suffix, got {}",
        state.wal_path
    );
    assert!(
        state.wal_size_bytes.is_some(),
        "a seeded WAL database must have a stat-able -wal file: {state:?}"
    );
    assert!(state.unavailable_reason.is_none(), "{state:?}");
}

#[test]
fn wal_file_state_degrades_with_a_reason_when_the_sidecar_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = wal_file_state(&dir.path().join("never-created.db"));
    assert!(state.wal_size_bytes.is_none());
    assert!(
        state.unavailable_reason.is_some(),
        "an absent WAL file must carry a reason, not a silent zero: {state:?}"
    );
}

#[test]
fn collect_on_a_file_backed_db_carries_build_identity_and_every_counter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _path) = seeded_pool(&dir);
    let reader_admission_capacity = pool.max_readers().max(1);

    let report = collect(
        &pool,
        BuildIdentity::from_env("9.9.9", Some("deadbeef")),
        Duration::from_secs(30),
    );

    assert_eq!(report.build.version, "9.9.9");
    assert_eq!(report.build.build_hash.as_deref(), Some("deadbeef"));
    assert!(report.db_path.is_some());
    assert!(
        report.checkpoint_probe.is_some(),
        "file-backed collect must land a probe; error was {:?}",
        report.checkpoint_probe_error
    );
    assert!(
        report.wal_file.as_ref().and_then(|w| w.wal_size_bytes) >= Some(0),
        "wal_size_bytes must be a non-negative byte count when present"
    );

    let json = serde_json::to_value(&report).expect("report serializes");
    let counters = json
        .get("checkpoint_counters")
        .expect("counters section present");
    for key in [
        "last_observed_wal_pages",
        "truncate_attempts",
        "truncate_consecutive_failures",
        "checkpoint_skipped_ticks",
        "checkpoint_consecutive_skips",
        "checkpoint_last_skip_wal_pages",
        "checkpoint_pressure_elevated_ticks",
        "checkpoint_pressure_episodes_started",
        "checkpoint_pressure_episodes_recovered",
        "checkpoint_lifecycle_append_attempts",
        "checkpoint_lifecycle_append_failures",
        "checkpoint_lifecycle_enqueue_drops",
        "read_tx_max_age_evictions",
    ] {
        assert!(counters.get(key).is_some(), "counter {key} must be present");
    }
    assert_eq!(
        report.writer_contention.writer_acquisitions, 1,
        "the seed write checked the finite-wait pooled writer out once"
    );
    assert_eq!(report.writer_contention.pooled_writer_acquisitions, 1);
    assert_eq!(report.writer_contention.standalone_writer_acquisitions, 0);
    assert_eq!(report.writer_contention.writer_task_acquisitions, 0);
    assert_eq!(report.writer_contention.writer_acquisition_timeouts, 0);
    assert_eq!(
        report.reader_contention,
        ReaderContentionDiagnostics {
            configured_reader_cap: pool.config().max_readers,
            configured_checkout_timeout_ms: u64::try_from(
                pool.config().checkout_timeout.as_millis(),
            )
            .unwrap_or(u64::MAX),
            configured_busy_timeout_ms: u64::try_from(pool.config().busy_timeout.as_millis())
                .unwrap_or(u64::MAX),
            reader_admission_capacity,
            available_reader_admission_slots: reader_admission_capacity,
            reader_acquisitions: 0,
            pooled_reader_checkouts: 0,
            standalone_reader_opens: 0,
            infrastructure_standalone_reader_opens: 0,
            reader_checkout_timeouts: 0,
            reader_busy_timeouts: 0,
            active_pooled_reader_checkouts: 0,
            peak_active_pooled_reader_checkouts: 0,
            completed_pooled_reader_checkouts: 0,
            max_completed_reader_hold_micros: 0,
            max_completed_reader_hold_operation: None,
            reader_discards: 0,
            reader_replacement_open_failures: 0,
        },
        "the diagnostics probe itself must not masquerade as request reader traffic"
    );
    assert_eq!(
        report.graph_edge_integrity,
        Some(GraphEdgeIntegrity {
            duplicate_edge_id_groups: 0,
            graph_edges_rows: 0,
            graph_edges_seq_rows: 0,
            pre_v14_duplicate_edge_state_detected: false,
            live_entities_carrying_merged_into: 0,
        })
    );
    assert!(report.graph_edge_integrity_error.is_none());
    let fts_segments = report
        .fts_segments
        .as_ref()
        .expect("file-backed diagnostics must decode both FTS structure rows");
    assert_eq!(fts_segments.entities.segment_count, 1);
    assert_eq!(fts_segments.notes.segment_count, 1);
    assert_eq!(fts_segments.total_segments, 2);
    assert!(report.fts_segments_error.is_none());
    let fts_json = json
        .get("fts_segments")
        .expect("FTS segment diagnostics serialize");
    assert_eq!(fts_json["entities"]["segment_count"], 1);
    assert!(json.get("fts_maintenance").is_some());
    assert!(
        report.size_composition.is_some(),
        "file-backed diagnostics must include page composition; error was {:?}",
        report.size_composition_error
    );
    assert!(report.size_composition_error.is_none());
    assert!(report.writer_contention.audit_append_failures.is_none());
    assert!(report
        .writer_contention
        .audit_obligation_append_failures
        .is_none());
    assert!(report
        .writer_contention
        .audit_obligation_append_failures_unavailable_reason
        .is_some());
    assert!(json["writer_contention"]["audit_obligation_append_failures"].is_null());
    assert!(
        report
            .writer_contention
            .audit_append_failures_unavailable_reason
            .is_some(),
        "a direct khive-db snapshot must not fabricate a runtime audit count"
    );
}

include!("diagnostics/reader_contention_tests.rs");

#[test]
fn diagnostics_composes_file_backed_standalone_acquisitions_without_counting_its_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, _path) = seeded_pool(&dir);

    pool.standalone_autocommit(|_| Ok(()))
        .expect("write-traffic standalone operation");

    let report = collect_with_audit_append_failures(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
        0,
    );
    assert_eq!(report.writer_contention.writer_acquisitions, 2);
    assert_eq!(report.writer_contention.pooled_writer_acquisitions, 1);
    assert_eq!(report.writer_contention.standalone_writer_acquisitions, 1);
    assert_eq!(report.writer_contention.writer_task_acquisitions, 0);
    assert_eq!(report.writer_contention.writer_acquisition_timeouts, 0);

    let second = collect_with_audit_append_failures(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
        0,
    );
    assert_eq!(
        second.writer_contention, report.writer_contention,
        "the diagnostics PASSIVE probe must not inflate write-traffic counters"
    );
}

#[test]
fn runtime_aware_collect_exposes_the_supplied_audit_failure_counter() {
    let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool");

    let report = collect_with_audit_append_failures(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
        17,
    );

    assert_eq!(report.writer_contention.audit_append_failures, Some(17));
    assert!(
        report
            .writer_contention
            .audit_append_failures_unavailable_reason
            .is_none(),
        "a supplied runtime counter must not carry an unavailable reason"
    );
}

#[test]
fn diagnostics_exposes_an_induced_writer_checkout_timeout() {
    let pool = ConnectionPool::new(PoolConfig {
        checkout_timeout: Duration::from_millis(1),
        ..PoolConfig::default()
    })
    .expect("in-memory pool");

    let held = pool.writer().expect("first writer checkout succeeds");
    assert!(
        matches!(
            pool.writer(),
            Err(crate::SqliteError::WriterPoolCheckoutTimeout { .. })
        ),
        "holding the sole writer must exercise the typed timeout path"
    );
    drop(held);

    let report = collect_with_audit_append_failures(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
        0,
    );
    assert_eq!(report.writer_contention.writer_acquisitions, 1);
    assert_eq!(report.writer_contention.pooled_writer_acquisitions, 1);
    assert_eq!(report.writer_contention.standalone_writer_acquisitions, 0);
    assert_eq!(report.writer_contention.writer_task_acquisitions, 0);
    assert_eq!(report.writer_contention.writer_acquisition_timeouts, 1);
}

/// The `u64::MAX` never-observed sentinel must serialize as `null`, never
/// as a huge number an operator would read as a real page count.
#[test]
fn never_observed_sentinels_serialize_as_null() {
    let counters = CheckpointCounters {
        last_observed_wal_pages: None,
        truncate_attempts: 0,
        truncate_consecutive_failures: 0,
        checkpoint_skipped_ticks: 0,
        checkpoint_consecutive_skips: 0,
        checkpoint_last_skip_wal_pages: None,
        checkpoint_pressure_elevated_ticks: 0,
        checkpoint_pressure_episodes_started: 0,
        checkpoint_pressure_episodes_recovered: 0,
        checkpoint_lifecycle_append_attempts: 0,
        checkpoint_lifecycle_append_failures: 0,
        checkpoint_lifecycle_enqueue_drops: 0,
        read_tx_max_age_evictions: 0,
    };
    let json = serde_json::to_value(counters).expect("serializes");
    assert!(json["last_observed_wal_pages"].is_null());
    assert!(json["checkpoint_last_skip_wal_pages"].is_null());
}

#[test]
fn graph_edge_integrity_detects_the_pre_v14_duplicate_state() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    conn.execute_batch(
        "CREATE TABLE graph_edges (
                 namespace TEXT NOT NULL,
                 id TEXT NOT NULL,
                 PRIMARY KEY (namespace, id)
             );
             CREATE TABLE graph_edges_seq (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 edge_id TEXT NOT NULL UNIQUE
             );
             CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER, merged_into TEXT);
             INSERT INTO graph_edges(namespace, id)
             VALUES ('alpha', 'shared-edge'), ('beta', 'shared-edge');
             INSERT INTO graph_edges_seq(edge_id) VALUES ('shared-edge');",
    )
    .expect("seed the state possible before the V14 uniqueness guard");

    let integrity = graph_edge_integrity(&conn).expect("integrity query succeeds");

    assert_eq!(integrity.duplicate_edge_id_groups, 1);
    assert_eq!(integrity.graph_edges_rows, 2);
    assert_eq!(integrity.graph_edges_seq_rows, 1);
    assert!(integrity.pre_v14_duplicate_edge_state_detected);
}

#[test]
fn graph_edge_integrity_counts_only_live_rows_that_still_carry_merge_provenance() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    conn.execute_batch(
        "CREATE TABLE graph_edges (
                 namespace TEXT NOT NULL,
                 id TEXT NOT NULL,
                 PRIMARY KEY (namespace, id)
             );
             CREATE TABLE graph_edges_seq (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 edge_id TEXT NOT NULL UNIQUE
             );
             CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER, merged_into TEXT);
             INSERT INTO entities(id, deleted_at, merged_into) VALUES
                 ('kept', NULL, NULL),
                 ('tombstoned-source', 1, 'kept'),
                 ('left-live-by-an-old-restore', NULL, 'kept'),
                 ('plain-soft-delete', 1, NULL);",
    )
    .expect("seed one row of each shape");

    let integrity = graph_edge_integrity(&conn).expect("integrity query succeeds");

    assert_eq!(
        integrity.live_entities_carrying_merged_into, 1,
        "a merge tombstone and a plain live row are both in order; only the live row \
             carrying merged_into is the invariant violation"
    );
    assert_eq!(integrity.duplicate_edge_id_groups, 0);
}

#[test]
fn graph_edge_integrity_does_not_mislabel_retained_delete_history_as_a_duplicate() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    conn.execute_batch(
        "CREATE TABLE graph_edges (
                 namespace TEXT NOT NULL,
                 id TEXT NOT NULL,
                 PRIMARY KEY (namespace, id)
             );
             CREATE TABLE graph_edges_seq (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 edge_id TEXT NOT NULL UNIQUE
             );
             CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER, merged_into TEXT);
             INSERT INTO graph_edges(namespace, id) VALUES ('local', 'live-edge');
             INSERT INTO graph_edges_seq(edge_id)
             VALUES ('deleted-edge'), ('live-edge');",
    )
    .expect("seed a retained sequence row for a hard-deleted edge");

    let integrity = graph_edge_integrity(&conn).expect("integrity query succeeds");

    assert_eq!(integrity.duplicate_edge_id_groups, 0);
    assert_eq!(integrity.graph_edges_rows, 1);
    assert_eq!(integrity.graph_edges_seq_rows, 2);
    assert!(
        !integrity.pre_v14_duplicate_edge_state_detected,
        "ledger rows intentionally survive hard deletion; count mismatch alone is not the \
             pre-V14 duplicate state"
    );
}

#[cfg(unix)]
#[test]
fn unmeasured_sidecar_cleanup_fields_are_absent_from_the_wire_payload() {
    let pin = wal_pin_attribution_from_census(crate::walpin::CensusResult {
        holders: std::collections::HashSet::new(),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    });

    assert_eq!(pin.sidecar_listing_truncated, None);
    assert_eq!(pin.sidecar_entries_cleanup_would_reap, None);
    let json = serde_json::to_value(pin).expect("attribution serializes");
    assert!(
        json.get("sidecar_listing_truncated").is_none(),
        "a skipped enumeration must omit sidecar_listing_truncated, not fabricate false"
    );
    assert!(
        json.get("sidecar_entries_cleanup_would_reap").is_none(),
        "a skipped enumeration must omit sidecar_entries_cleanup_would_reap, not fabricate 0"
    );
}

#[cfg(unix)]
#[test]
fn wal_pin_census_serializes_only_under_the_nested_carrier() {
    let pin = wal_pin_attribution_from_census(crate::walpin::CensusResult {
        holders: std::collections::HashSet::from([41, 7]),
        uninspectable_pids: vec![99],
        truncated: true,
        budget_exhausted: false,
    });

    let json = serde_json::to_value(pin).expect("attribution serializes");
    assert_eq!(json["census"]["holder_pids"], serde_json::json!([7, 41]));
    assert_eq!(
        json["census"]["uninspectable_pids"],
        serde_json::json!([99])
    );
    assert_eq!(json["census"]["truncated"], true);

    for duplicate in [
        "census_holder_pids",
        "census_uninspectable_pids",
        "census_truncated",
        "census_is_complete",
    ] {
        assert!(
            json.get(duplicate).is_none(),
            "wal_pin.{duplicate} must not duplicate wal_pin.census: {json}"
        );
    }
}

/// A missing configured path must never be created by a diagnostic
/// request. The untracked standalone open omits `SQLITE_OPEN_CREATE`, so
/// the probe degrades to an error and the file stays absent.
#[test]
fn probe_refuses_a_missing_configured_path_without_creating_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, path) = seeded_pool(&dir);

    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
    assert!(!path.exists(), "precondition: the database file is gone");

    let report = collect(
        &pool,
        BuildIdentity::from_env("0.0.0", None),
        Duration::from_secs(30),
    );

    assert!(
        report.checkpoint_probe.is_none(),
        "a missing database must not yield a probe result: {report:?}"
    );
    assert!(
        report.checkpoint_probe_error.is_some(),
        "a missing database must say why there is no probe: {report:?}"
    );
    assert!(
        !path.exists(),
        "a diagnostics request must never create the database it was asked about"
    );
}

/// In-memory backends have no WAL file and no census target: the report
/// still returns, with explicit reasons rather than missing sections.
#[test]
fn collect_degrades_gracefully_for_an_in_memory_backend() {
    let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool");
    let report = collect(
        &pool,
        BuildIdentity::from_env("0.0.0", None),
        Duration::from_secs(30),
    );

    assert!(report.db_path.is_none());
    assert!(report.wal_file.is_none());
    assert!(report.checkpoint_probe.is_none());
    assert!(report.fts_segments.is_none());
    assert!(report.fts_segments_error.is_some());
    assert!(
        report.checkpoint_probe_error.is_some(),
        "an in-memory report must say WHY there is no probe"
    );
    assert!(!report.wal_pin.available);
    assert!(report.wal_pin.unavailable_reason.is_some());
    assert_eq!(report.wal_pin.status, WalPinAttributionStatus::Unavailable);
    assert!(matches!(
        report.wal_pin.census,
        WalPinCensus::Unavailable { .. }
    ));
    assert!(report.size_composition.is_none());
    assert!(report
        .size_composition_error
        .as_deref()
        .is_some_and(|reason| reason.contains("no file-backed page composition")));
}

#[cfg(unix)]
#[test]
fn incomplete_holder_census_is_a_tagged_degraded_result() {
    let census = crate::walpin::CensusResult {
        holders: std::collections::HashSet::from([41, 7]),
        uninspectable_pids: vec![99, 99],
        truncated: true,
        budget_exhausted: false,
    };

    let pin = wal_pin_attribution_from_census(census);

    assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
    assert!(!pin.available);
    assert!(!pin.census_is_complete);
    assert_eq!(pin.census_holder_pids, vec![7, 41]);
    assert_eq!(pin.census_uninspectable_pids, vec![99]);
    assert!(
        pin.unavailable_reason.as_deref().is_some_and(
            |reason| reason.contains("additional database holders cannot be ruled out")
        ),
        "the legacy reason must also fail loud for old consumers: {pin:?}"
    );
    match &pin.census {
        WalPinCensus::Incomplete {
            holder_pids,
            uninspectable_pids,
            truncated,
            reason,
        } => {
            assert_eq!(holder_pids, &vec![7, 41]);
            assert_eq!(uninspectable_pids, &vec![99]);
            assert!(*truncated);
            assert!(reason.contains("additional database holders cannot be ruled out"));
        }
        other => panic!("incomplete scan must serialize as incomplete, got {other:?}"),
    }

    let json = serde_json::to_value(&pin).expect("serializes");
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["census"]["status"], "incomplete");
}

#[cfg(unix)]
#[test]
fn complete_holder_census_stays_explicit_while_attribution_is_degraded() {
    let census = crate::walpin::CensusResult {
        holders: std::collections::HashSet::from([7]),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    };

    let pin = wal_pin_attribution_from_census(census);

    assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
    assert!(pin.census_is_complete);
    assert!(matches!(
        pin.census,
        WalPinCensus::Complete { ref holder_pids } if holder_pids == &vec![7]
    ));
    assert_eq!(
        pin.status_reasons.len(),
        1,
        "only missing sidecar reconciliation degrades a complete OS census"
    );
}

#[cfg(unix)]
#[test]
fn complete_holder_and_read_only_sidecar_evidence_reconcile_to_complete() {
    let census = crate::walpin::CensusResult {
        holders: std::collections::HashSet::from([7]),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    };
    let sidecar = crate::walpin::WalpinReport {
        entries: vec![crate::walpin::WalpinPidHealth::RegisteredSilent { pid: 7 }],
        sidecar_listing_truncated: false,
        cleanup_would_reap: 0,
        orphan_temps_reaped: 0,
    };

    let pin = wal_pin_attribution_from_evidence(census, sidecar);

    assert_eq!(pin.status, WalPinAttributionStatus::Complete);
    assert!(pin.available);
    assert!(pin.fully_attributed);
    assert!(pin.status_reasons.is_empty());
    assert_eq!(pin.registered_silent_pids, vec![7]);
    assert!(pin.census_pids_without_attribution.is_empty());
    assert_eq!(pin.sidecar_listing_truncated, Some(false));
    assert_eq!(pin.sidecar_entries_cleanup_would_reap, Some(0));
}

#[cfg(unix)]
#[test]
fn complete_census_with_an_unregistered_holder_is_degraded_not_exonerated() {
    let census = crate::walpin::CensusResult {
        holders: std::collections::HashSet::from([7, 41]),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    };
    let sidecar = crate::walpin::WalpinReport {
        entries: vec![crate::walpin::WalpinPidHealth::RegisteredSilent { pid: 7 }],
        sidecar_listing_truncated: false,
        cleanup_would_reap: 0,
        orphan_temps_reaped: 0,
    };

    let pin = wal_pin_attribution_from_evidence(census, sidecar);

    assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
    assert!(!pin.available);
    assert!(!pin.fully_attributed);
    assert_eq!(pin.census_pids_without_attribution, vec![41]);
    assert!(pin
        .status_reasons
        .iter()
        .any(|reason| reason.contains("holder(s) have no sidecar attribution")));
}

#[test]
fn database_size_composition_reports_tables_indexes_fts_and_vectors_separately() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    conn.execute_batch(
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
             CREATE INDEX idx_docs_body ON docs(body); \
             CREATE TABLE fts_demo_data(id INTEGER PRIMARY KEY, block BLOB); \
             CREATE TABLE vec_demo_chunks(id INTEGER PRIMARY KEY, vectors BLOB); \
             CREATE TABLE knowledge_sections(id INTEGER PRIMARY KEY, embedding BLOB); \
             INSERT INTO docs(body) VALUES (zeroblob(8192)); \
             INSERT INTO fts_demo_data(block) VALUES (zeroblob(8192)); \
             INSERT INTO vec_demo_chunks(vectors) VALUES (zeroblob(8192)); \
             INSERT INTO knowledge_sections(embedding) VALUES (zeroblob(8192));",
    )
    .expect("seed size classes");

    let composition = size_composition::collect(&conn, true)
        .size_composition
        .expect("dbstat composition");
    let class_for = |name: &str| {
        composition
            .objects
            .iter()
            .find(|object| object.name == name)
            .map(|object| object.storage_class)
    };

    assert_eq!(class_for("docs"), Some(DatabaseStorageClass::RowTable));
    assert_eq!(
        class_for("idx_docs_body"),
        Some(DatabaseStorageClass::Index)
    );
    assert_eq!(
        class_for("fts_demo_data"),
        Some(DatabaseStorageClass::FullText)
    );
    assert_eq!(
        class_for("vec_demo_chunks"),
        Some(DatabaseStorageClass::Vector)
    );
    assert_eq!(
        class_for("knowledge_sections"),
        Some(DatabaseStorageClass::MixedRowAndEmbedding)
    );
    assert!(composition.vector_bytes > 0);
    assert!(composition.full_text_bytes > 0);
    assert!(composition.mixed_embedding_bytes > 0);
    assert_eq!(
        composition
            .accounted_bytes
            .saturating_add(composition.freelist_bytes)
            .saturating_add(composition.unaccounted_bytes),
        composition.database_bytes
    );
}

/// A holder with no sidecar registration remains degraded even though the
/// read-only sidecar pass itself completed successfully.
#[cfg(unix)]
#[test]
#[serial(khive_walpin_sidecar_env)]
fn wal_pin_attribution_degrades_when_a_holder_has_no_sidecar_registration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, path) = seeded_pool(&dir);
    let _ = &pool;

    let pin = wal_pin_attribution(&path, Duration::from_secs(30));

    assert!(
        !pin.fully_attributed,
        "the pool's OS holder has no test sidecar registration"
    );
    assert!(
        pin.unavailable_reason.is_some(),
        "the missing holder attribution must be explained: {pin:?}"
    );
    assert!(pin.sidecar_entries.is_empty());
    assert!(pin.reporting.is_empty());
    assert_eq!(pin.sidecar_listing_truncated, Some(false));
    assert_eq!(pin.sidecar_entries_cleanup_would_reap, Some(0));
    assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
    assert!(matches!(
        pin.census,
        WalPinCensus::Complete { .. } | WalPinCensus::Incomplete { .. }
    ));
}

/// `sidecar_dir_for` is a purely lexical derivation from whatever path it
/// is handed (`pool.rs`'s `sidecar_dir_for_alias_convergence` proves
/// this at the primitive level). Diagnostics must feed it
/// `pool.canonical_path()` — the same value the checkpoint sidecar
/// writers use — never the pool's raw configured path, or a symlinked
/// database misses its own sidecar evidence entirely.
#[cfg(unix)]
#[test]
#[serial(khive_walpin_sidecar_env)]
fn diagnostics_finds_sidecar_evidence_through_an_aliased_database_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real_dir = dir.path().join("real");
    std::fs::create_dir(&real_dir).expect("mkdir real dir");
    let real_path = real_dir.join("diag.db");
    std::fs::write(&real_path, b"").expect("create real file");
    let alias_path = dir.path().join("alias.db");
    std::os::unix::fs::symlink(&real_path, &alias_path).expect("symlink alias");

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(alias_path.clone()),
        ..PoolConfig::for_test()
    })
    .expect("pool open through symlinked path");
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .expect("seed a write so the WAL file exists");
    }

    let canonical = pool
        .canonical_path()
        .expect("file-backed pool")
        .to_path_buf();
    assert_ne!(
        canonical, alias_path,
        "the alias must actually differ from the canonical path for this test to mean \
             anything"
    );

    let pid = std::process::id();
    let sidecar_dir = crate::walpin::sidecar_dir_for(&canonical);
    let beacon = crate::walpin::WalpinBeacon {
        pid,
        process_role: "session".to_string(),
        started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
        sweep_interval_ms: 5_000,
    };
    crate::walpin::write_beacon(&sidecar_dir, &beacon).expect("seed this process's beacon");

    let report = collect(
        &pool,
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );

    // The wider OS holder census can be legitimately incomplete in a
    // sandboxed test environment (other, unrelated processes this test
    // has no permission to inspect) — that variability is orthogonal to
    // what this test checks. What must hold regardless is that the
    // sidecar enumeration itself, which is keyed off the canonical
    // path, actually ran, completed untruncated, and found this
    // process's own beacon rather than missing it beside the wrong
    // (aliased) directory.
    assert_eq!(
        report.wal_pin.sidecar_listing_truncated,
        Some(false),
        "the sidecar enumeration must run to completion: {:?}",
        report.wal_pin
    );
    assert!(
        report.wal_pin.registered_silent_pids.contains(&pid),
        "the beacon written beside the canonical path must be found: {:?}",
        report.wal_pin
    );
    assert!(
        report.wal_pin.census_holder_pids.contains(&pid),
        "the OS census must find this process holding its own database open: {:?}",
        report.wal_pin
    );
    assert!(
        !report
            .wal_pin
            .census_pids_without_attribution
            .contains(&pid),
        "this process's own holder entry must be attributed by its own sidecar evidence, \
             not left unexplained: {:?}",
        report.wal_pin
    );
}

/// The async collector (`collect_with_audit_append_failures_interruptibly`)
/// resolves its operational path independently of the sync collector
/// (`collect`) — see `operational_db_path`. A regression that reintroduced
/// the raw configured path on only the async side would leave the sync
/// alias test above green while the async path silently missed its own
/// sidecar evidence; this exercises the same aliasing scenario through
/// the async entry point.
#[cfg(unix)]
#[tokio::test]
#[serial(khive_walpin_sidecar_env)]
async fn diagnostics_finds_sidecar_evidence_through_an_aliased_database_path_async() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real_dir = dir.path().join("real");
    std::fs::create_dir(&real_dir).expect("mkdir real dir");
    let real_path = real_dir.join("diag.db");
    std::fs::write(&real_path, b"").expect("create real file");
    let alias_path = dir.path().join("alias.db");
    std::os::unix::fs::symlink(&real_path, &alias_path).expect("symlink alias");

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(alias_path.clone()),
        ..PoolConfig::for_test()
    })
    .expect("pool open through symlinked path");
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .expect("seed a write so the WAL file exists");
    }

    let canonical = pool
        .canonical_path()
        .expect("file-backed pool")
        .to_path_buf();
    assert_ne!(
        canonical, alias_path,
        "the alias must actually differ from the canonical path for this test to mean \
             anything"
    );

    let pid = std::process::id();
    let sidecar_dir = crate::walpin::sidecar_dir_for(&canonical);
    let beacon = crate::walpin::WalpinBeacon {
        pid,
        process_role: "session".to_string(),
        started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
        sweep_interval_ms: 5_000,
    };
    crate::walpin::write_beacon(&sidecar_dir, &beacon).expect("seed this process's beacon");

    let pool = Arc::new(pool);
    let report = collect_with_audit_append_failures_interruptibly(
        Arc::clone(&pool),
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
        0,
    )
    .await
    .expect("diagnostics succeed");

    // Same rationale as the sync test: the wider OS holder census can be
    // legitimately incomplete in a sandboxed environment, but the sidecar
    // enumeration itself — keyed off the canonical path — must run to
    // completion and find this process's own beacon.
    assert_eq!(
        report.wal_pin.sidecar_listing_truncated,
        Some(false),
        "the sidecar enumeration must run to completion: {:?}",
        report.wal_pin
    );
    assert!(
        report.wal_pin.registered_silent_pids.contains(&pid),
        "the beacon written beside the canonical path must be found: {:?}",
        report.wal_pin
    );
    assert!(
        report.wal_pin.census_holder_pids.contains(&pid),
        "the OS census must find this process holding its own database open: {:?}",
        report.wal_pin
    );
    assert!(
        !report
            .wal_pin
            .census_pids_without_attribution
            .contains(&pid),
        "this process's own holder entry must be attributed by its own sidecar evidence, \
             not left unexplained: {:?}",
        report.wal_pin
    );
}

#[tokio::test]
async fn size_composition_can_be_skipped_while_page_accounting_is_still_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pages.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE ballast (body BLOB);")
            .expect("create ballast");
        for _ in 0..40 {
            writer
                .conn()
                .execute("INSERT INTO ballast (body) VALUES (zeroblob(4096))", [])
                .expect("insert ballast row");
        }
        writer
            .conn()
            .execute("DELETE FROM ballast", [])
            .expect("free the ballast pages");
    }
    let pool = Arc::new(pool);
    let collect = |collect_size_composition: bool| {
        collect_with_runtime_audit_metrics_for_process_interruptibly(
            Arc::clone(&pool),
            BuildIdentity::from_env("test", None),
            ProcessIdentity::current(&pool),
            Duration::from_secs(30),
            0,
            None,
            collect_size_composition,
        )
    };

    let full = collect(true).await.expect("full report");
    let composition = full
        .size_composition
        .expect("the dbstat composition is collected by default");
    let pages = full.page_accounting.expect("page accounting on a file");
    assert_eq!(
        (
            pages.page_size_bytes,
            pages.page_count,
            pages.freelist_pages
        ),
        (
            composition.page_size_bytes,
            composition.page_count,
            composition.freelist_pages
        ),
        "the pragma reads and the composition must agree on one file"
    );
    assert!(pages.freelist_pages >= 40, "{pages:?}");

    let skipped = collect(false).await.expect("report without the walk");
    assert!(skipped.size_composition.is_none(), "{skipped:?}");
    assert_eq!(
        skipped.size_composition_error.as_deref(),
        Some(SIZE_COMPOSITION_NOT_REQUESTED)
    );
    assert_eq!(skipped.page_accounting, Some(pages));
    assert!(skipped.page_accounting_error.is_none());
}
