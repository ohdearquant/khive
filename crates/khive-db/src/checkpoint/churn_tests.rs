#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(checkpoint_skip_metrics, khive_walpin_census_budget_env)]
async fn db_diagnostics_commit_churn_without_readers_does_not_report_a_pin() {
    let _budget_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_CENSUS_BUDGET_MS");
    std::env::set_var("KHIVE_WALPIN_CENSUS_BUDGET_MS", "10");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("diagnostic_commit_churn.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER)")
            .unwrap();
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(run_checkpoint_task(
        Arc::clone(&pool),
        CheckpointConfig {
            interval: Duration::from_millis(10),
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

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_pool = Arc::clone(&pool);
    let writer_stop = Arc::clone(&stop);
    let writer_task = std::thread::spawn(move || {
        let writer = writer_pool.writer().expect("writer checkout");
        let mut value = 0_i64;
        while !writer_stop.load(Ordering::SeqCst) {
            writer
                .conn()
                .execute("INSERT INTO t VALUES (?1)", [value])
                .expect("commit loop row");
            value += 1;
            std::thread::yield_now();
        }
    });

    let deadline = Instant::now() + Duration::from_millis(1_200);
    let mut saw_oldest_pinned_frame = false;
    let mut saw_pin_depth = false;
    while Instant::now() < deadline {
        let report = crate::diagnostics::collect(
            &pool,
            crate::diagnostics::BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
        );
        saw_oldest_pinned_frame |= report.checkpoint_pin.oldest_pinned_frame.is_some();
        saw_pin_depth |= report.checkpoint_pin.pin_depth.is_some();
        tokio::task::yield_now().await;
    }

    stop.store(true, Ordering::SeqCst);
    writer_task.join().expect("commit loop must finish");
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("commit-churn checkpoint task shutdown exceeded 5 s")
        .expect("checkpoint task must not panic");
    assert!(
        !saw_oldest_pinned_frame,
        "short-lived backfill rows without a reader must not age into pins"
    );
    assert!(
        !saw_pin_depth,
        "writer-only backfill gaps are not pin depths"
    );
}

#[test]
#[serial(checkpoint_skip_metrics, khive_walpin_census_budget_env)]
fn live_multi_writer_churn_has_no_pin_at_tight_or_default_cadence() {
    let _budget_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_CENSUS_BUDGET_MS");
    std::env::set_var("KHIVE_WALPIN_CENSUS_BUDGET_MS", "10");

    for interval in [Duration::from_millis(10), Duration::from_millis(500)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_multi_writer_churn.db");
        let pool = file_pool(&path);
        {
            let writer = pool.writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (writer INTEGER)")
                .unwrap();
        }
        let _run_owner = CheckpointRunTaskGuard::start(&pool, interval);
        let stop = Arc::new(AtomicBool::new(false));
        let first_commits = Arc::new(AtomicUsize::new(0));
        let writer_connections: Vec<_> = (0..3)
            .map(|_| {
                let conn = rusqlite::Connection::open(&path).unwrap();
                conn.busy_timeout(Duration::from_millis(250)).unwrap();
                conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
                conn
            })
            .collect();
        let writers: Vec<_> = writer_connections
            .into_iter()
            .enumerate()
            .map(|(writer_id, conn)| {
                let writer_id = i64::try_from(writer_id).unwrap();
                let stop = Arc::clone(&stop);
                let first_commits = Arc::clone(&first_commits);
                std::thread::spawn(move || {
                    let mut commits = 0_u64;
                    while !stop.load(Ordering::SeqCst) {
                        match conn.execute("INSERT INTO t VALUES (?1)", [writer_id]) {
                            Ok(_) => {
                                commits += 1;
                                if commits == 1 {
                                    first_commits.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(rusqlite::Error::SqliteFailure(code, _))
                                if matches!(
                                    code.code,
                                    rusqlite::ErrorCode::DatabaseBusy
                                        | rusqlite::ErrorCode::DatabaseLocked
                                ) => {}
                            Err(error) => panic!("writer {writer_id} failed: {error}"),
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    commits
                })
            })
            .collect();

        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);
        let mut samples = 0;
        let mut bad_reports = Vec::new();
        // Hold the window open until every writer has committed once, up to the deadline.
        while started.elapsed() < Duration::from_millis(1_600)
            || (first_commits.load(Ordering::SeqCst) < 3 && Instant::now() < deadline)
        {
            let report = crate::diagnostics::collect(
                &pool,
                crate::diagnostics::BuildIdentity::from_env("test", None),
                Duration::from_secs(30),
            );
            samples += 1;
            let (run, age) = checkpoint_run_snapshot(&pool);
            if report.checkpoint_pin.oldest_pinned_frame.is_some()
                || report.checkpoint_pin.pin_depth.is_some()
                || age.is_some_and(|age| age >= Duration::from_secs(1))
            {
                bad_reports.push((
                    report.checkpoint_pin.oldest_pinned_frame,
                    report.checkpoint_pin.pin_depth,
                    run,
                    age,
                ));
            }
            std::thread::sleep(interval);
        }

        stop.store(true, Ordering::SeqCst);
        let commits: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().expect("writer must finish"))
            .collect();
        assert!(
            commits.iter().all(|commits| *commits > 0),
            "every writer must commit before the deadline at {interval:?}: {commits:?}"
        );
        assert!(samples >= 2, "checkpoint probes must run at {interval:?}");
        assert!(
            bad_reports.is_empty(),
            "writer-only churn must not age a run or report a pin at {interval:?}: {bad_reports:?}"
        );
    }
}
