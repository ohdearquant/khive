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
