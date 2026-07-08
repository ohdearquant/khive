//! Process-level resource reads (ADR-103 Stage 1).
//!
//! Cumulative CPU time and RSS via `getrusage(2)`, used by the daemon's
//! phase-span logging (background-task start/completion/cancellation) and
//! the `comm.health` resource self-report. `libc` is already a workspace
//! dependency (used elsewhere in this crate for `flock`/`kill`), so this
//! reuses it rather than adding a new one, per ADR-103 Stage 1's preference
//! for a small std/libc-based read over a dedicated sysinfo-style crate.

/// A point-in-time snapshot of this process's cumulative resource usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessResourceUsage {
    /// Cumulative user+system CPU time consumed by this process since start,
    /// in microseconds.
    pub cpu_us: i64,
    /// Resident set size, in bytes.
    pub rss_bytes: i64,
}

/// Read this process's cumulative CPU time and RSS via `getrusage(RUSAGE_SELF)`.
///
/// Returns `None` if the underlying syscall fails (never expected in
/// practice on a supported platform) or on a non-Unix target, where no
/// portable equivalent is wired up. Callers treat this as a best-effort
/// diagnostic read, never load-bearing.
#[cfg(unix)]
pub fn process_resource_usage() -> Option<ProcessResourceUsage> {
    // SAFETY: `getrusage` is a POSIX syscall with no memory side effects
    // beyond writing into the `rusage` out-param, which is a plain
    // `#[repr(C)]` struct zero-initialized immediately before the call.
    let usage: libc::rusage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };
    let user_us = usage.ru_utime.tv_sec * 1_000_000 + i64::from(usage.ru_utime.tv_usec as i32);
    let sys_us = usage.ru_stime.tv_sec * 1_000_000 + i64::from(usage.ru_stime.tv_usec as i32);
    // `ru_maxrss` is bytes on macOS/Darwin and KiB on Linux — the two
    // platforms this fleet actually ships on (see CLAUDE.md toolchain rules).
    let rss_bytes: i64 = if cfg!(target_os = "macos") {
        usage.ru_maxrss as i64
    } else {
        usage.ru_maxrss as i64 * 1024
    };
    Some(ProcessResourceUsage {
        cpu_us: user_us + sys_us,
        rss_bytes,
    })
}

/// Non-Unix fallback: no portable `getrusage` equivalent is wired up.
#[cfg(not(unix))]
pub fn process_resource_usage() -> Option<ProcessResourceUsage> {
    None
}

/// CPU time consumed strictly between two [`ProcessResourceUsage`] snapshots,
/// in microseconds.
///
/// Review finding (issue #723 fix-round): `cpu_us` on a snapshot is
/// cumulative process CPU time since process start, not CPU consumed by any
/// one phase. Callers that want a per-phase attribution must capture a
/// snapshot at phase entry and another at phase exit and take the delta —
/// this helper does that subtraction and floors at zero (via `saturating_sub`
/// followed by `.max(0)`, since a plain `i64::saturating_sub` only guards
/// against integer overflow/underflow, not against a negative *result* — a
/// spurious (should-never-happen) decrease in cumulative CPU between two
/// reads must never surface as a negative delta).
pub fn cpu_delta_us(
    start: Option<ProcessResourceUsage>,
    end: Option<ProcessResourceUsage>,
) -> Option<i64> {
    match (start, end) {
        (Some(start), Some(end)) => Some(end.cpu_us.saturating_sub(start.cpu_us).max(0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_resource_usage_returns_positive_values() {
        // Do a small amount of work first so ru_utime/ru_maxrss are
        // guaranteed non-zero on every platform this runs on.
        let mut acc: u64 = 0;
        for i in 0..1_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);

        let usage = process_resource_usage().expect("getrusage must succeed on unix CI runners");
        assert!(usage.cpu_us >= 0, "cpu_us must be non-negative");
        assert!(
            usage.rss_bytes > 0,
            "rss_bytes must be positive for a live process"
        );
    }

    // Review finding (issue #723 fix-round): `cpu_delta_us` must report the
    // difference between two snapshots, not either snapshot's raw
    // (cumulative-since-process-start) value. This test uses synthetic
    // snapshots rather than real `getrusage` reads so it fails deterministically
    // if someone reverts the caller to reporting `end.cpu_us` directly —
    // a huge cumulative value in `end` alone would otherwise pass a merely
    // "non-negative" check.
    #[test]
    fn cpu_delta_us_reports_the_difference_not_the_cumulative_end_value() {
        let start = ProcessResourceUsage {
            cpu_us: 500 * 60 * 1_000_000, // 500 CPU-minutes already burned
            rss_bytes: 1_000_000,
        };
        let end = ProcessResourceUsage {
            cpu_us: 500 * 60 * 1_000_000 + 1_500, // +1.5ms of CPU during the phase
            rss_bytes: 1_100_000,
        };

        let delta = cpu_delta_us(Some(start), Some(end)).expect("both snapshots present");
        assert_eq!(
            delta, 1_500,
            "delta must be end-minus-start, not the cumulative end value"
        );
        assert!(
            delta < start.cpu_us,
            "a correct delta must be far smaller than the pre-existing cumulative total"
        );
    }

    #[test]
    fn cpu_delta_us_saturates_instead_of_underflowing_on_a_spurious_decrease() {
        let start = ProcessResourceUsage {
            cpu_us: 1_000,
            rss_bytes: 0,
        };
        let end = ProcessResourceUsage {
            cpu_us: 500,
            rss_bytes: 0,
        };
        assert_eq!(cpu_delta_us(Some(start), Some(end)), Some(0));
    }

    #[test]
    fn cpu_delta_us_is_none_when_either_snapshot_is_unavailable() {
        let snap = ProcessResourceUsage {
            cpu_us: 10,
            rss_bytes: 0,
        };
        assert_eq!(cpu_delta_us(None, Some(snap)), None);
        assert_eq!(cpu_delta_us(Some(snap), None), None);
        assert_eq!(cpu_delta_us(None, None), None);
    }
}
