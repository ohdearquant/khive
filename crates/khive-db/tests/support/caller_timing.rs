use std::ffi::OsStr;
use std::time::Duration;

pub fn assert_caller_latency(elapsed: Duration, checkout_timeout: Duration) {
    let profile_file = std::env::var_os("LLVM_PROFILE_FILE");
    if let Some(bound) = caller_latency_bound(checkout_timeout, profile_file.as_deref()) {
        assert!(
            elapsed < bound,
            "writer checkout took {elapsed:?} with a {checkout_timeout:?} admission timeout; \
             timeout diagnostics must return within the {bound:?} caller bound"
        );
    }
}

fn caller_latency_bound(
    checkout_timeout: Duration,
    profile_file: Option<&OsStr>,
) -> Option<Duration> {
    // Instrumentation may amplify scheduling delays; each caller retains a
    // separate hang watchdog and still requires a real admission timeout.
    if profile_file.is_some() {
        None
    } else {
        Some(checkout_timeout * 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstrumented_caller_bound_is_ten_checkout_timeouts() {
        for timeout_ms in [50, 75] {
            assert_eq!(
                caller_latency_bound(Duration::from_millis(timeout_ms), None),
                Some(Duration::from_millis(timeout_ms * 10))
            );
        }
    }

    #[test]
    fn coverage_presence_skips_only_the_numeric_caller_bound() {
        for profile_file in [OsStr::new(""), OsStr::new("profile-%p.profraw")] {
            assert_eq!(
                caller_latency_bound(Duration::from_millis(50), Some(profile_file)),
                None
            );
        }
    }
}
