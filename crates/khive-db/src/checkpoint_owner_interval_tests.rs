    #[test]
    fn pr3409_retiring_fast_owner_restores_live_interval() {
        let dir = tempfile::tempdir().unwrap();
        let pool = file_pool(&dir.path().join("pr3409_retired_fast.db"));
        let slow = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(1_000));
        let fast = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
        assert_eq!(pr3409_active_interval(&pool), 10);
        drop(fast);
        assert_eq!(
            pr3409_active_interval(&pool),
            1_000,
            "the remaining owner, not a departed owner, defines the budget"
        );
        drop(slow);
        assert_eq!(checkpoint_run_status(&pool), CheckpointRunStatus::NoTask);
    }

    #[test]
    fn checkpoint_run_interval_tracks_duplicate_fast_owners() {
        let dir = tempfile::tempdir().unwrap();
        let pool = file_pool(&dir.path().join("duplicate_intervals.db"));
        let slow = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(1_000));
        let fast_one = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
        let fast_two = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
        drop(fast_one);
        assert_eq!(pr3409_active_interval(&pool), 10);
        drop(fast_two);
        assert_eq!(pr3409_active_interval(&pool), 1_000);
        drop(slow);
        assert_eq!(checkpoint_run_status(&pool), CheckpointRunStatus::NoTask);
    }

    #[test]
    fn pr3409_busy_gap_uses_surviving_owner_budget() {
        let dir = tempfile::tempdir().unwrap();
        let pool = file_pool(&dir.path().join("pr3409_surviving_budget.db"));
        let _slow = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(1_000));
        let fast = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
        drop(fast);
        let interval = pr3409_active_interval(&pool);
        let mut entry = None;
        advance_checkpoint_run(&mut entry, Some((0, 20, 7)), 1_000, interval);
        advance_checkpoint_run(&mut entry, Some((1, -1, -1)), 1_050, interval);
        advance_checkpoint_run(&mut entry, Some((0, 21, 7)), 1_100, interval);
        assert_eq!(
            entry
                .expect("matching informative row")
                .run
                .first_observed_at_unix_ms,
            1_000,
            "a 100ms busy gap fits the surviving 1000ms owner's budget"
        );
    }

    #[test]
    fn pr3409_retiring_slow_owner_keeps_fast_budget_and_cleans_last_owner() {
        let dir = tempfile::tempdir().unwrap();
        let pool = file_pool(&dir.path().join("pr3409_retired_slow.db"));
        let fast = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
        let slow = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(1_000));
        drop(slow);
        assert_eq!(pr3409_active_interval(&pool), 10);
        drop(fast);
        assert_eq!(checkpoint_run_status(&pool), CheckpointRunStatus::NoTask);
    }

    #[test]
    fn pr3409_new_lifecycle_does_not_inherit_retired_interval() {
        let dir = tempfile::tempdir().unwrap();
        let pool = file_pool(&dir.path().join("pr3409_new_lifecycle.db"));
        let fast = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(10));
        drop(fast);
        let _slow = CheckpointRunTaskGuard::start(&pool, Duration::from_millis(1_000));
        assert_eq!(pr3409_active_interval(&pool), 1_000);
        assert_eq!(
            checkpoint_run_status(&pool),
            CheckpointRunStatus::NoObservation
        );
    }
