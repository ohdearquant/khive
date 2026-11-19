#[test]
#[serial_test::serial(khive_walpin_sidecar_env)]
fn sidecar_enabled_defaults_to_file_backed() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove("KHIVE_WALPIN_SIDECAR");
    }) {
        return;
    }
    // Deterministic regardless of the ambient environment (minor,
    // ADR-091 Amendment 2: the prior version was vacuously true
    // whenever `KHIVE_WALPIN_SIDECAR` happened to be set already).
    assert!(sidecar_enabled(true), "file-backed must default on");
    assert!(!sidecar_enabled(false), "in-memory must default off");
}

#[test]
#[serial_test::serial(khive_walpin_sidecar_env)]
fn sidecar_enabled_env_override_wins_either_way() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
    crate::test_process::set_var("KHIVE_WALPIN_SIDECAR", "off");
    assert!(
        !sidecar_enabled(true),
        "explicit off must override file-backed default"
    );
    crate::test_process::set_var("KHIVE_WALPIN_SIDECAR", "on");
    assert!(
        sidecar_enabled(false),
        "explicit on must override in-memory default"
    );
}
