    // --- ImapBackoff: escalation, cap, reset ---

    #[test]
    fn escalates_5_10_20_40_80_160_320_capped_at_600() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        let expected_steps = [5u64, 10, 20, 40, 80, 160, 320, 600, 600, 600];
        for (i, &secs) in expected_steps.iter().enumerate() {
            let tick = b.record_failure();
            assert_eq!(
                tick.step,
                Duration::from_secs(secs),
                "attempt {} (index {i}) expected step {secs}s, got {:?}",
                tick.attempt,
                tick.step
            );
            assert_eq!(tick.attempt, (i + 1) as u32);
        }
    }

    #[test]
    fn caps_at_max_and_never_exceeds_it() {
        let max = Duration::from_secs(600);
        let mut b = ImapBackoff::new(Duration::from_secs(5), max);
        for _ in 0..50 {
            let tick = b.record_failure();
            assert!(tick.step <= max);
            assert!(tick.delay >= tick.step.min(max));
            // The post-jitter delay must never exceed
            // `max`, even once `step` itself has saturated at `max` (an
            // unclamped 25% jitter window on a capped 600s step would reach
            // 750s otherwise).
            assert!(
                tick.delay <= max,
                "post-jitter delay {:?} exceeded the {max:?} cap",
                tick.delay
            );
        }
    }

    #[test]
    fn default_config_jittered_delay_never_exceeds_default_max() {
        // Explicitly drive the DEFAULT_BASE/DEFAULT_MAX config to the cap and
        // assert both the unjittered step and the jittered delay respect
        // DEFAULT_MAX -- the exact cap regression scenario (750s > 600s cap).
        let mut b = ImapBackoff::default();
        for _ in 0..20 {
            let tick = b.record_failure();
            assert!(tick.step <= DEFAULT_MAX);
            assert!(
                tick.delay <= DEFAULT_MAX,
                "post-jitter delay {:?} exceeded DEFAULT_MAX {:?}",
                tick.delay,
                DEFAULT_MAX
            );
        }
    }

    #[test]
    fn resets_to_base_on_success() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        for _ in 0..5 {
            b.record_failure();
        }
        assert!(b.attempt() > 0);
        b.record_success();
        assert_eq!(b.attempt(), 0);

        let tick = b.record_failure();
        assert_eq!(
            tick.step,
            Duration::from_secs(5),
            "first failure after a success must restart at base, not resume escalation"
        );
        assert_eq!(tick.attempt, 1);
    }

    #[test]
    fn should_warn_true_on_every_new_step_false_once_capped() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(20));
        // step sequence: 5 (new), 10 (new), 20 (new, = cap), 20 (repeat, capped), 20 (repeat)
        let expected_warn = [true, true, true, false, false];
        for &want_warn in &expected_warn {
            let tick = b.record_failure();
            assert_eq!(
                tick.should_warn, want_warn,
                "attempt {}: step={:?}",
                tick.attempt, tick.step
            );
        }
    }

    #[test]
    fn should_warn_resets_after_success_then_new_failure() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(20));
        assert!(b.record_failure().should_warn); // 5s, new
        assert!(b.record_failure().should_warn); // 10s, new
        assert!(b.record_failure().should_warn); // 20s, new (clamped to cap)
        assert!(!b.record_failure().should_warn); // 20s, repeat at cap -> no re-log
        b.record_success();
        let tick = b.record_failure();
        assert!(
            tick.should_warn,
            "escalation edge must re-arm after a success resets state"
        );
        assert_eq!(tick.step, Duration::from_secs(5));
    }

    #[test]
    fn attempt_zero_before_any_failure() {
        let b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        assert_eq!(b.attempt(), 0);
    }

    #[test]
    fn custom_base_and_max_respected() {
        let mut b = ImapBackoff::new(Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(b.record_failure().step, Duration::from_millis(100));
        assert_eq!(b.record_failure().step, Duration::from_millis(200));
        assert_eq!(b.record_failure().step, Duration::from_millis(300));
        assert_eq!(b.record_failure().step, Duration::from_millis(300));
    }

    // --- is_backoff_eligible classification ---

    #[test]
    fn auth_and_transport_are_backoff_eligible() {
        assert!(is_backoff_eligible(&ChannelError::Auth("x".into())));
        assert!(is_backoff_eligible(&ChannelError::Transport("x".into())));
    }

    #[test]
    fn config_unauthorized_invalid_envelope_are_not_backoff_eligible() {
        assert!(!is_backoff_eligible(&ChannelError::Config("x".into())));
        assert!(!is_backoff_eligible(&ChannelError::UnauthorizedSender(
            "x".into()
        )));
        assert!(!is_backoff_eligible(&ChannelError::InvalidEnvelope(
            "x".into()
        )));
    }
