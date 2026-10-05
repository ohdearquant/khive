// Checkpoint environment parser fixtures run in private processes.

#[test]
#[serial]
fn checkpoint_config_env_override() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }

    crate::test_process::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "250");
    crate::test_process::set_var("KHIVE_WAL_WARN_PAGES", "1500");
    crate::test_process::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "8000");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", "12000");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS", "60");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_BUSY_MS", "500");
    crate::test_process::set_var("KHIVE_TX_WARN_SECS", "15");
    crate::test_process::set_var("KHIVE_TX_MAX_AGE_SECS", "90");

    let cfg = CheckpointConfig::from_env();

    crate::test_process::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
    crate::test_process::remove_var("KHIVE_WAL_WARN_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");
    crate::test_process::remove_var("KHIVE_TX_WARN_SECS");
    crate::test_process::remove_var("KHIVE_TX_MAX_AGE_SECS");

    assert_eq!(cfg.interval, Duration::from_millis(250));
    assert_eq!(cfg.warn_pages, 1500);
    assert_eq!(cfg.high_water_pages, 8000);
    assert_eq!(cfg.truncate_high_water_pages, 12000);
    assert_eq!(cfg.truncate_min_interval, Duration::from_secs(60));
    assert_eq!(cfg.truncate_busy_timeout, Duration::from_millis(500));
    assert_eq!(cfg.tx_warn_secs, Duration::from_secs(15));
    assert_eq!(cfg.tx_max_age_secs, Duration::from_secs(90));
}

#[test]
#[serial]
fn checkpoint_config_defaults_on_invalid_env() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }

    let default = CheckpointConfig::default();

    crate::test_process::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "not_a_number");
    crate::test_process::set_var("KHIVE_WAL_WARN_PAGES", "");
    crate::test_process::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "0");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", "not_a_number");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS", "");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_BUSY_MS", "0");
    crate::test_process::set_var("KHIVE_TX_WARN_SECS", "not_a_number");
    crate::test_process::set_var("KHIVE_TX_MAX_AGE_SECS", "0");

    let cfg = CheckpointConfig::from_env();

    crate::test_process::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
    crate::test_process::remove_var("KHIVE_WAL_WARN_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");
    crate::test_process::remove_var("KHIVE_TX_WARN_SECS");
    crate::test_process::remove_var("KHIVE_TX_MAX_AGE_SECS");

    assert_eq!(cfg.interval, default.interval);
    assert_eq!(cfg.warn_pages, default.warn_pages);
    assert_eq!(cfg.high_water_pages, default.high_water_pages);
    assert_eq!(
        cfg.truncate_high_water_pages,
        default.truncate_high_water_pages
    );
    assert_eq!(cfg.truncate_min_interval, default.truncate_min_interval);
    assert_eq!(cfg.truncate_busy_timeout, default.truncate_busy_timeout);
    assert_eq!(cfg.tx_warn_secs, default.tx_warn_secs);
    assert_eq!(cfg.tx_max_age_secs, default.tx_max_age_secs);
}

#[test]
#[serial]
fn checkpoint_config_rejects_zero_for_all_fields() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }

    let default = CheckpointConfig::default();
    crate::test_process::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "0");
    crate::test_process::set_var("KHIVE_WAL_WARN_PAGES", "0");
    crate::test_process::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "0");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", "0");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS", "0");
    crate::test_process::set_var("KHIVE_WAL_TRUNCATE_BUSY_MS", "0");
    crate::test_process::set_var("KHIVE_TX_WARN_SECS", "0");
    crate::test_process::set_var("KHIVE_TX_MAX_AGE_SECS", "0");

    let cfg = CheckpointConfig::from_env();

    crate::test_process::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
    crate::test_process::remove_var("KHIVE_WAL_WARN_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
    crate::test_process::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");
    crate::test_process::remove_var("KHIVE_TX_WARN_SECS");
    crate::test_process::remove_var("KHIVE_TX_MAX_AGE_SECS");

    assert_eq!(
        cfg.interval, default.interval,
        "zero interval must fall back to default"
    );
    assert_eq!(
        cfg.warn_pages, default.warn_pages,
        "zero warn_pages must fall back to default"
    );
    assert_eq!(
        cfg.high_water_pages, default.high_water_pages,
        "zero high_water_pages must fall back to default"
    );
    assert_eq!(
        cfg.truncate_high_water_pages, default.truncate_high_water_pages,
        "zero truncate_high_water_pages must fall back to default"
    );
    assert_eq!(
        cfg.truncate_min_interval, default.truncate_min_interval,
        "zero truncate_min_interval must fall back to default"
    );
    assert_eq!(
        cfg.truncate_busy_timeout, default.truncate_busy_timeout,
        "zero truncate_busy_timeout must fall back to default"
    );
    assert_eq!(
        cfg.tx_warn_secs, default.tx_warn_secs,
        "zero tx_warn_secs must fall back to default"
    );
    assert_eq!(
        cfg.tx_max_age_secs, default.tx_max_age_secs,
        "zero tx_max_age_secs must fall back to default"
    );
}

/// Fix: a reversed threshold pair must not be honored independently. See
/// crates/khive-db/docs/api/checkpoint.md#checkpoint_config_rejects_reversed_tx_thresholds
#[test]
#[serial]
fn checkpoint_config_rejects_reversed_tx_thresholds() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }

    let default = CheckpointConfig::default();
    crate::test_process::set_var("KHIVE_TX_WARN_SECS", "120");
    crate::test_process::set_var("KHIVE_TX_MAX_AGE_SECS", "30");

    let cfg = CheckpointConfig::from_env();

    crate::test_process::remove_var("KHIVE_TX_WARN_SECS");
    crate::test_process::remove_var("KHIVE_TX_MAX_AGE_SECS");

    assert_eq!(
        cfg.tx_warn_secs, default.tx_warn_secs,
        "a reversed pair must fall back tx_warn_secs to its default, got: {:?}",
        cfg.tx_warn_secs
    );
    assert_eq!(
        cfg.tx_max_age_secs, default.tx_max_age_secs,
        "a reversed pair must fall back tx_max_age_secs to its default, got: {:?}",
        cfg.tx_max_age_secs
    );
}

/// Degenerate equal-thresholds case; see
/// crates/khive-db/docs/api/checkpoint.md#checkpoint_config_rejects_equal_tx_thresholds
#[test]
#[serial]
fn checkpoint_config_rejects_equal_tx_thresholds() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }

    let default = CheckpointConfig::default();
    crate::test_process::set_var("KHIVE_TX_WARN_SECS", "60");
    crate::test_process::set_var("KHIVE_TX_MAX_AGE_SECS", "60");

    let cfg = CheckpointConfig::from_env();

    crate::test_process::remove_var("KHIVE_TX_WARN_SECS");
    crate::test_process::remove_var("KHIVE_TX_MAX_AGE_SECS");

    assert_eq!(
        cfg.tx_warn_secs, default.tx_warn_secs,
        "an equal pair must fall back tx_warn_secs to its default, got: {:?}",
        cfg.tx_warn_secs
    );
    assert_eq!(
        cfg.tx_max_age_secs, default.tx_max_age_secs,
        "an equal pair must fall back tx_max_age_secs to its default, got: {:?}",
        cfg.tx_max_age_secs
    );
}

/// `KHIVE_WAL_WARN_SUSTAINED_CYCLES` overrides the default and rejects 0.
#[test]
#[serial]
fn checkpoint_config_warn_sustained_cycles_env_override() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }

    let default = CheckpointConfig::default();
    assert_eq!(default.warn_sustained_cycles, DEFAULT_WARN_SUSTAINED_CYCLES);

    crate::test_process::set_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES", "5");
    let cfg = CheckpointConfig::from_env();
    crate::test_process::remove_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES");
    assert_eq!(cfg.warn_sustained_cycles, 5);

    crate::test_process::set_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES", "0");
    let cfg_zero = CheckpointConfig::from_env();
    crate::test_process::remove_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES");
    assert_eq!(
        cfg_zero.warn_sustained_cycles, DEFAULT_WARN_SUSTAINED_CYCLES,
        "zero must fall back to the default"
    );

    crate::test_process::set_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES", "not_a_number");
    let cfg_invalid = CheckpointConfig::from_env();
    crate::test_process::remove_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES");
    assert_eq!(
        cfg_invalid.warn_sustained_cycles,
        DEFAULT_WARN_SUSTAINED_CYCLES
    );
}
