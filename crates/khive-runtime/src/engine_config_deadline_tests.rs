#[test]
fn exec_binary_digest_timeout_is_bounded_at_load() {
    let mut config = KhiveConfig::default();
    assert_eq!(
        config
            .exec
            .binary_digest_timeout_s
            .unwrap_or(DEFAULT_EXEC_BINARY_DIGEST_TIMEOUT_S),
        10
    );
    for invalid in [0, MAX_EXEC_BINARY_DIGEST_TIMEOUT_S + 1] {
        config.exec.binary_digest_timeout_s = Some(invalid);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidExecConfig { key, .. }) if key == "binary_digest_timeout_s"
        ));
    }
    config.exec.binary_digest_timeout_s = Some(20);
    config.validate().expect("bounded digest timeout override");
}

#[test]
fn web_partial_ceiling_config_rejects_incoherent_effective_bounds_at_load() {
    let dir = tempfile::tempdir().unwrap();
    for (default_key, max_key, default, maximum) in [
        ("timeout_default_s", "timeout_max_s", 30_u64, 120_u64),
        (
            "max_bytes_default",
            "max_bytes_max",
            5 * 1024 * 1024,
            50 * 1024 * 1024,
        ),
        ("search_limit_default", "search_limit_max", 10, 50),
    ] {
        for invalid in [
            format!("{max_key}=1"),
            format!("{max_key}=0"),
            format!("{default_key}=0"),
            format!("{default_key}={}", maximum + 1),
            format!("{default_key}=2\n{max_key}=1"),
        ] {
            let path = write_toml(&dir, &format!("[web]\n{invalid}\n"));
            let error = KhiveConfig::load(Some(&path)).unwrap_err();
            assert!(
                error.to_string().contains(default_key),
                "{invalid}: {error}"
            );
        }
        for valid in [
            String::new(),
            format!("{max_key}={default}"),
            format!("{default_key}={maximum}"),
            format!("{default_key}=1\n{max_key}=1"),
        ] {
            let path = write_toml(&dir, &format!("[web]\n{valid}\n"));
            let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
            config.web.validate().unwrap();
        }
    }
}

#[test]
fn web_ceiling_programmatic_validation_rejects_unrepresentable_deadlines() {
    let config = WebSectionConfig {
        timeout_max_s: Some(u64::MAX),
        ..Default::default()
    };
    assert!(
        matches!(config.resolved_ceilings(), Err(ConfigError::InvalidWebConfig { key, .. }) if key == "timeout_max_s")
    );
    assert!(config.validate().is_err());
    let config = WebSectionConfig {
        max_bytes_max: Some(1),
        ..Default::default()
    };
    assert!(config.resolved_ceilings().is_err());
}
