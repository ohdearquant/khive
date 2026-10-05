    #[test]
    fn exec_timeouts_validate_effective_values_and_deadlines() {
        let mut config = KhiveConfig::default();
        config.exec.timeout_default_s = Some(900.0);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidExecConfig { key, .. }) if key == "timeout_default_s"
        ));

        config.exec.timeout_default_s = None;
        config.exec.timeout_max_s = Some(1.0);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidExecConfig { key, .. }) if key == "timeout_default_s"
        ));

        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            config.exec.timeout_max_s = None;
            config.exec.timeout_default_s = Some(invalid);
            assert!(matches!(
                config.validate(),
                Err(ConfigError::InvalidExecConfig { key, .. }) if key == "timeout_default_s"
            ));
        }

        config.exec.timeout_default_s = None;
        config.exec.timeout_max_s = Some(600.0);
        config.validate().expect("valid resolved exec bounds");
    }
