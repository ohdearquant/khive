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
}
