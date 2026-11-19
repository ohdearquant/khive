use std::time::Duration;

use super::{
    stdio_bridge_idle_timeout_from_env, stdio_bridge_max_outstanding_requests_from_env,
    stdio_bridge_request_obligation_ttl_from_env,
};

const CHILD_MARKER: &str = "KHIVE_BRIDGE_NUMERIC_ENV_TEST";

fn inputs() -> Vec<Option<String>> {
    vec![
        None,
        Some(String::new()),
        Some("not a number".into()),
        Some(" \t42\n".into()),
        Some("-1".into()),
        Some("0".into()),
        Some("4294967296".into()),
        Some(u64::MAX.to_string()),
        Some(usize::MAX.to_string()),
        Some("18446744073709551616".into()),
    ]
}

fn set(name: &str, value: Option<&str>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

fn duration(secs: u64) -> Option<Duration> {
    (secs != 0).then(|| Duration::from_secs(secs))
}

#[test]
fn bridge_idle_timeout_retains_legacy_numeric_policy() {
    if khive_storage::test_support::run_exact_test_in_child(CHILD_MARKER, false, |_| {}) {
        return;
    }
    const NAME: &str = "KHIVE_BRIDGE_IDLE_TIMEOUT_SECS";
    for input in inputs() {
        set(NAME, input.as_deref());
        let old = std::env::var(NAME)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        assert_eq!(
            stdio_bridge_idle_timeout_from_env(),
            duration(old),
            "{input:?}"
        );
    }
}

#[test]
fn bridge_request_ttl_retains_legacy_numeric_policy() {
    if khive_storage::test_support::run_exact_test_in_child(CHILD_MARKER, false, |_| {}) {
        return;
    }
    const NAME: &str = "KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS";
    for input in inputs() {
        set(NAME, input.as_deref());
        let old = std::env::var(NAME)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(3600);
        assert_eq!(
            stdio_bridge_request_obligation_ttl_from_env(),
            duration(old),
            "{input:?}"
        );
    }
}

#[test]
fn bridge_outstanding_limit_retains_legacy_numeric_policy() {
    if khive_storage::test_support::run_exact_test_in_child(CHILD_MARKER, false, |_| {}) {
        return;
    }
    const NAME: &str = "KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS";
    for input in inputs() {
        set(NAME, input.as_deref());
        let old = std::env::var(NAME)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&value| value > 0)
            .unwrap_or(1024);
        assert_eq!(
            stdio_bridge_max_outstanding_requests_from_env(),
            old,
            "{input:?}"
        );
    }
}
