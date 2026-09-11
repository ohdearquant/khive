//! The warm-index-host marker in its own test binary: it is a process-global that
//! is never cleared, so a test that flips it cannot share a binary with one that
//! asserts the default.

/// Order matters and is why this is one test rather than two: the default must be
/// observed before anything marks the process. A client that inherits the host role
/// by accident is exactly the failure this flag exists to prevent.
#[test]
fn the_role_defaults_to_client_and_is_set_only_by_an_explicit_mark() {
    assert!(
        !khive_runtime::daemon::is_warm_index_host(),
        "a process is a client until its boot path says otherwise"
    );
    khive_runtime::daemon::mark_warm_index_host();
    assert!(khive_runtime::daemon::is_warm_index_host());
}
