    /// The budget knob is read per request, so a wrong read is a wrong bound
    /// on every call. `0` has to mean unbounded rather than "spend nothing",
    /// because a zero-millisecond budget would truncate every census on the
    /// first process and report a holder list of nothing at all.
    #[test]
    #[serial_test::serial(khive_walpin_census_budget_env)]
    fn census_budget_reads_zero_as_unbounded_and_survives_a_malformed_value() {
        let _guard = crate::walpin::EnvVarGuard::capture(CENSUS_BUDGET_ENV);

        std::env::remove_var(CENSUS_BUDGET_ENV);
        assert_eq!(
            request_census_budget(),
            Some(DEFAULT_CENSUS_BUDGET),
            "an unset variable takes the default bound"
        );

        std::env::set_var(CENSUS_BUDGET_ENV, "0");
        assert_eq!(
            request_census_budget(),
            None,
            "0 restores the unbounded full-machine walk"
        );

        std::env::set_var(CENSUS_BUDGET_ENV, " 750 ");
        assert_eq!(
            request_census_budget(),
            Some(Duration::from_millis(750)),
            "a surrounding-whitespace value is still a number"
        );

        std::env::set_var(CENSUS_BUDGET_ENV, "soon");
        assert_eq!(
            request_census_budget(),
            Some(DEFAULT_CENSUS_BUDGET),
            "a malformed budget must not fail the request; the report states \
             which budget was actually used"
        );
    }

    /// A budget stop and an enumeration failure both set `truncated`, and an
    /// operator reads one sentence. If that sentence is the same for both,
    /// the bound makes the report cry wolf on every busy box.
    #[test]
    fn a_budget_stop_and_an_enumeration_failure_do_not_share_a_reason() {
        let budget = census_truncation_cause(true);
        let failure = census_truncation_cause(false);
        assert_ne!(budget, failure);
        assert!(
            budget.contains("budget"),
            "the budget reason must name the budget: {budget}"
        );
        assert!(
            budget.contains("wal_pin_census_budget_ms"),
            "and must point at the field carrying the value: {budget}"
        );
        assert!(
            !failure.contains("budget"),
            "an enumeration failure must not be described as a budget stop: {failure}"
        );
    }

    /// `wal_pin_census_budget_ms` is an `Option` precisely so that a producer
    /// which failed to record the budget cannot write the value that means
    /// "unbounded". This pins the wire shape of both states.
    #[test]
    fn collection_cost_distinguishes_an_unbounded_census_from_a_zero_cost_one() {
        let unbounded = CollectionCost {
            total_ms: 9,
            sqlite_ms: 4,
            wal_file_stat_ms: 0,
            wal_pin_census_ms: 5,
            wal_pin_sidecar_ms: 0,
            wal_pin_census_budget_ms: None,
            wal_pin_census_budget_exhausted: false,
        };
        let bounded = CollectionCost {
            wal_pin_census_budget_ms: Some(2000),
            wal_pin_census_budget_exhausted: true,
            ..unbounded
        };

        let unbounded = serde_json::to_value(unbounded).unwrap();
        let bounded = serde_json::to_value(bounded).unwrap();
        assert_eq!(
            unbounded["wal_pin_census_budget_ms"],
            serde_json::Value::Null
        );
        assert_eq!(bounded["wal_pin_census_budget_ms"], 2000);
        assert_eq!(unbounded["wal_pin_census_budget_exhausted"], false);
        assert_eq!(bounded["wal_pin_census_budget_exhausted"], true);
    }

    #[test]
    fn process_identity_serializes_os_start_time_or_explicit_unavailability() {
        let known = ProcessIdentity::from_start_time(42, Some(1_000_000_000), 3);
        assert_eq!(
            serde_json::to_value(known).unwrap(),
            serde_json::json!({
                "pid": 42,
                "started_at": 1_000_000_000,
                "started_at_unavailable_reason": null,
                "pool_generation": 3
            }),
            "preserve the OS timestamp, not request time or a derived uptime"
        );

        let unknown = ProcessIdentity::from_start_time(42, None, 3);
        let json = serde_json::to_value(unknown).unwrap();
        assert_eq!(json["pid"], 42);
        assert!(json["started_at"].is_null(), "never invent a start time");
        let reason = json["started_at_unavailable_reason"].as_str().unwrap();
        assert!(!reason.is_empty());
        assert!(reason.contains(std::env::consts::OS));
        assert_eq!(json["pool_generation"], 3);
    }

    #[tokio::test]
    async fn process_identity_is_present_in_every_collector_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (file_pool, _) = seeded_pool(&dir);
        let memory_pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool");
        for pool in [file_pool, memory_pool] {
            let pool = Arc::new(pool);
            let expected = ProcessIdentity::current(&pool);
            let writer_before = pool.writer_acquisition_snapshot();
            let sync = collect(
                &pool,
                BuildIdentity::from_env("test", None),
                Duration::from_secs(30),
            );
            let asynchronous = collect_with_runtime_audit_metrics_interruptibly(
                Arc::clone(&pool),
                BuildIdentity::from_env("test", None),
                Duration::from_secs(30),
                0,
                None,
            )
            .await
            .expect("diagnostics");
            for report in [sync, asynchronous] {
                assert_eq!(report.process, expected);
                let json = serde_json::to_value(report).unwrap();
                assert_eq!(json["process"], serde_json::to_value(&expected).unwrap());
                assert_eq!(json["build"]["version"], "test");
            }
            assert_eq!(
                pool.writer_acquisition_snapshot(),
                writer_before,
                "diagnostics must not count its probes as write traffic"
            );
        }
    }

    #[test]
    fn process_identity_survives_pool_reconstruction_with_reset_counters() {
        let pool = ConnectionPool::new(PoolConfig::default()).expect("first pool");
        drop(pool.try_writer().expect("writer acquisition"));
        drop(pool.reader().expect("reader acquisition"));
        let before = collect(
            &pool,
            BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
        );
        assert!(before.writer_contention.writer_acquisitions > 0);
        assert!(before.reader_contention.reader_acquisitions > 0);
        drop(pool);

        let replacement = ConnectionPool::new(PoolConfig::default()).expect("replacement pool");
        let after = collect(
            &replacement,
            BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
        );
        assert_eq!(after.process.pid, before.process.pid);
        assert_eq!(after.process.started_at, before.process.started_at);
        assert_eq!(
            after.process.started_at_unavailable_reason,
            before.process.started_at_unavailable_reason
        );
        assert!(after.process.pool_generation > before.process.pool_generation);
        assert_eq!(after.writer_contention.writer_acquisitions, 0);
        assert_eq!(after.reader_contention.reader_acquisitions, 0);
    }
