//! Deliberately failing test. It checks that a pull request whose dependents-closure
//! run fails is refused by the CI gate (ADR-205 D9). This file is never merged: the
//! pull request that adds it is closed once the gate's verdict is read.

#[test]
fn closure_test_failure_is_refused_by_the_gate() {
    let marker_set = std::env::var_os("KHIVE_SCOPE_REFUSAL_CHECK_PASS").is_some();
    assert!(
        marker_set,
        "deliberate failure: the CI gate must refuse a scoped run whose closure test fails"
    );
}
