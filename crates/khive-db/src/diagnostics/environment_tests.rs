/// The budget knob is read per request, so a wrong read is a wrong bound
/// on every call. `0` has to mean unbounded rather than "spend nothing",
/// because a zero-millisecond budget would truncate every census on the
/// first process and report a holder list of nothing at all.
#[test]
#[serial_test::serial(khive_walpin_census_budget_env)]
fn census_budget_reads_zero_as_unbounded_and_survives_a_malformed_value() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = crate::walpin::EnvVarGuard::capture(CENSUS_BUDGET_ENV);

    crate::test_process::remove_var(CENSUS_BUDGET_ENV);
    assert_eq!(
        request_census_budget(),
        Some(DEFAULT_CENSUS_BUDGET),
        "an unset variable takes the default bound"
    );

    crate::test_process::set_var(CENSUS_BUDGET_ENV, "0");
    assert_eq!(
        request_census_budget(),
        None,
        "0 restores the unbounded full-machine walk"
    );

    crate::test_process::set_var(CENSUS_BUDGET_ENV, " 750 ");
    assert_eq!(
        request_census_budget(),
        Some(Duration::from_millis(750)),
        "a surrounding-whitespace value is still a number"
    );

    crate::test_process::set_var(CENSUS_BUDGET_ENV, "soon");
    assert_eq!(
        request_census_budget(),
        Some(DEFAULT_CENSUS_BUDGET),
        "a malformed budget must not fail the request; the report states \
         which budget was actually used"
    );
}

/// ADR-091 Amendment 6: an operator who explicitly disables the sidecar
/// also disables its collection. Diagnostics must honor that rather than
/// running `inspect_live` regardless of the operator's setting.
#[cfg(unix)]
#[test]
#[serial(khive_walpin_sidecar_env)]
fn wal_pin_attribution_reports_disabled_when_the_sidecar_is_explicitly_off() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_WALPIN_SIDECAR", "0");
    }) {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (pool, path) = seeded_pool(&dir);
    let _ = &pool;

    let pin = wal_pin_attribution(&path, Duration::from_secs(30));

    assert!(
        !pin.available,
        "an explicitly disabled sidecar can never produce a reconciled answer"
    );
    assert!(
        pin.unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("disabled")),
        "the reason must name the disabled sidecar, not a generic enumeration failure: \
         {pin:?}"
    );
    assert!(pin.sidecar_entries.is_empty());
    assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
}

#[cfg(unix)]
#[tokio::test]
async fn passive_diagnostics_and_holder_census_bypass_an_active_floor() {
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
    let path = root.join("diagnostic-floor.db");
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
    pool.writer().unwrap().execute_batch(
        "CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER, merged_into TEXT);
         CREATE TABLE graph_edges (namespace TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(namespace, id));
         CREATE TABLE graph_edges_seq (seq INTEGER PRIMARY KEY AUTOINCREMENT, edge_id TEXT NOT NULL UNIQUE);
         CREATE VIRTUAL TABLE fts_entities USING fts5(namespace UNINDEXED, subject_id UNINDEXED, title, body, tokenize='trigram');
         CREATE VIRTUAL TABLE fts_notes USING fts5(namespace UNINDEXED, subject_id UNINDEXED, title, body, tokenize='trigram');
         INSERT INTO entities(id) VALUES ('diagnostic-fixture')"
    ).unwrap();
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
    let pid = std::process::id();
    let sidecar = crate::walpin::sidecar_dir_for(pool.canonical_path().unwrap());
    crate::walpin::write_beacon(
        &sidecar,
        &crate::walpin::WalpinBeacon {
            pid,
            process_role: "session".into(),
            started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
            sweep_interval_ms: 5_000,
        },
    )
    .unwrap();
    let pool = Arc::new(pool);
    let report = collect_with_audit_append_failures_interruptibly(
        Arc::clone(&pool),
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
        0,
    )
    .await
    .expect("actual diagnostics must remain available below floor");
    let passive = report
        .checkpoint_probe
        .expect("the real PASSIVE probe must execute");
    assert_eq!(passive.busy, 0);
    assert!(passive.log_frames > 0);
    assert_eq!(passive.checkpointed_frames, passive.log_frames);
    assert!(report.checkpoint_probe_error.is_none());
    assert_eq!(report.disk_guard.effective_reserve_bytes, Some(100));
    assert!(report.graph_edge_integrity.is_some());
    assert!(report.graph_edge_integrity_error.is_none());
    assert!(report.fts_segments.is_some());
    assert!(report.fts_segments_error.is_none());
    assert_eq!(report.wal_pin.sidecar_listing_truncated, Some(false));
    assert!(report.wal_pin.registered_silent_pids.contains(&pid));
    assert!(
        report.wal_pin.census_holder_pids.contains(&pid),
        "the census must find this pool's own process"
    );
    assert!(!report
        .wal_pin
        .census_pids_without_attribution
        .contains(&pid));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert!(!locks.exists());
    assert_eq!(pool.writer_acquisition_snapshot(), before);
    assert_eq!(
        pool.reader()
            .unwrap()
            .query_row("SELECT count(*) FROM entities", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
