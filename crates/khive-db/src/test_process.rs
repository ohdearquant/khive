//! Process-local environment for library fixtures; configure worker-owning tests before exec.

use std::ffi::OsStr;
use std::process::Command;

const CHILD_TEST: &str = "KHIVE_DB_ISOLATED_ENV_TEST";

/// Sibling isolation does not stop a fixture's own workers from reading the environment.
/// Give worker-owning fixtures their fixed values through `configure` before exec;
/// the mutation helpers are for thread-free parser/configuration cases only.
pub(crate) fn run_in_child(configure: impl FnOnce(&mut Command)) -> bool {
    khive_storage::test_support::run_exact_test_in_child(CHILD_TEST, false, configure)
}

fn assert_isolated() {
    let current = std::thread::current();
    let name = current.name().expect("libtest names its test threads");
    assert_eq!(
        std::env::var(CHILD_TEST).ok().as_deref(),
        Some(name),
        "environment writes require this exact test's isolated child"
    );
}

pub(crate) fn set_var(key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
    assert_isolated();
    std::env::set_var(key, value);
}

pub(crate) fn remove_var(key: impl AsRef<OsStr>) {
    assert_isolated();
    std::env::remove_var(key);
}

#[test]
fn environment_write_refuses_an_unisolated_test() {
    const KEY: &str = "KHIVE_DB_ENV_GUARD_CONTROL";
    let before = std::env::var_os(KEY);
    let refusal =
        std::panic::catch_unwind(|| crate::test_process::set_var(KEY, "must not be written"));
    assert!(refusal.is_err(), "an unisolated write must refuse");
    assert_eq!(
        std::env::var_os(KEY),
        before,
        "refusal must precede mutation"
    );
}

#[test]
fn child_environment_does_not_change_the_parent() {
    const KEY: &str = "KHIVE_DB_ENV_CHILD_CONTROL";
    let before = std::env::var_os(KEY);
    if run_in_child(|command| {
        command.env(KEY, "configured before exec");
    }) {
        assert_eq!(std::env::var_os(KEY), before);
        return;
    }
    assert_eq!(std::env::var(KEY).as_deref(), Ok("configured before exec"));
    crate::test_process::set_var(KEY, "changed only in child");
    assert_eq!(std::env::var(KEY).as_deref(), Ok("changed only in child"));
    crate::test_process::remove_var(KEY);
    assert!(std::env::var_os(KEY).is_none());
}
