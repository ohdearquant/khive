//! Process-level resource reads (ADR-103 Stage 1).
//!
//! Cumulative CPU time and peak RSS via `getrusage(2)`, plus current RSS
//! from platform APIs, used by the daemon's phase-span logging (background-task start/completion/cancellation) and
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
    /// Lifetime peak resident set size in bytes.
    pub max_rss_bytes: i64,
}

#[cfg(target_os = "linux")]
fn linux_rss_from_statm(statm: &str, page_size: i64) -> Option<i64> {
    let pages: i64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    pages.checked_mul(page_size)
}

#[cfg(target_os = "linux")]
fn platform_current_rss_bytes() -> Option<i64> {
    // SAFETY: sysconf has no pointer arguments and returns -1 on failure.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    linux_rss_from_statm(&statm, page_size as i64)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn platform_current_rss_bytes() -> Option<i64> {
    // SAFETY: task_info receives a writable, correctly sized Darwin struct and
    // its count in integer_t units. The self task port needs no ownership transfer.
    let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            (&mut info as *mut libc::mach_task_basic_info).cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS || count < libc::MACH_TASK_BASIC_INFO_COUNT {
        return None;
    }
    let rss = info.resident_size;
    i64::try_from(rss).ok().filter(|value| *value > 0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_current_rss_bytes() -> Option<i64> {
    None
}

/// Current resident bytes for this process, or None if the platform cannot
/// supply the reading. This independent read is not on the CPU snapshot path.
pub fn current_rss_bytes() -> Option<i64> {
    platform_current_rss_bytes()
}

/// Read cumulative CPU and peak RSS with one getrusage call.
/// Returns None when getrusage is unavailable or fails.
#[cfg(unix)]
pub fn process_resource_usage() -> Option<ProcessResourceUsage> {
    // SAFETY: getrusage writes only the initialized rusage out-parameter.
    let usage: libc::rusage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };
    Some(resource_usage_from_rusage(&usage))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // Unix field widths and signedness vary.
fn resource_usage_from_rusage(usage: &libc::rusage) -> ProcessResourceUsage {
    let user_us = (usage.ru_utime.tv_sec as i64)
        .saturating_mul(1_000_000)
        .saturating_add(i64::from(usage.ru_utime.tv_usec as i32));
    let sys_us = (usage.ru_stime.tv_sec as i64)
        .saturating_mul(1_000_000)
        .saturating_add(i64::from(usage.ru_stime.tv_usec as i32));
    let max_rss_bytes = if cfg!(target_os = "macos") {
        usage.ru_maxrss as i64
    } else {
        (usage.ru_maxrss as i64).saturating_mul(1024)
    };
    ProcessResourceUsage {
        cpu_us: user_us.saturating_add(sys_us),
        max_rss_bytes,
    }
}

/// Non-Unix fallback: no portable `getrusage` equivalent is wired up.
#[cfg(not(unix))]
pub fn process_resource_usage() -> Option<ProcessResourceUsage> {
    None
}

/// CPU time consumed strictly between two [`ProcessResourceUsage`] snapshots,
/// in microseconds.
///
/// `cpu_us` on a snapshot is cumulative process CPU time since process
/// start, not CPU consumed by any one phase, so per-phase attribution needs
/// the delta between an entry and exit snapshot. The result is floored at
/// zero so a spurious (should-never-happen) decrease in cumulative CPU
/// between two reads never surfaces as a negative delta.
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        let current = current_rss_bytes().expect("current RSS must be available");
        let peak = usage.max_rss_bytes;
        assert!(current > 0 && peak > 0, "RSS readings must be positive");
    }

    // `cpu_delta_us` must report the difference between two snapshots, not
    // either snapshot's raw cumulative value. Synthetic snapshots (rather
    // than real `getrusage` reads) make this fail deterministically if the
    // caller regresses to reporting `end.cpu_us` directly.
    #[test]
    fn cpu_delta_us_reports_the_difference_not_the_cumulative_end_value() {
        let start = ProcessResourceUsage {
            cpu_us: 500 * 60 * 1_000_000, // 500 CPU-minutes already burned
            max_rss_bytes: 1_000_000,
        };
        let end = ProcessResourceUsage {
            cpu_us: 500 * 60 * 1_000_000 + 1_500, // +1.5ms of CPU during the phase
            max_rss_bytes: 1_100_000,
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
            max_rss_bytes: 0,
        };
        let end = ProcessResourceUsage {
            cpu_us: 500,
            max_rss_bytes: 0,
        };
        assert_eq!(cpu_delta_us(Some(start), Some(end)), Some(0));
    }

    #[test]
    fn cpu_delta_us_is_none_when_either_snapshot_is_unavailable() {
        let snap = ProcessResourceUsage {
            cpu_us: 10,
            max_rss_bytes: 0,
        };
        assert_eq!(cpu_delta_us(None, Some(snap)), None);
        assert_eq!(cpu_delta_us(Some(snap), None), None);
        assert_eq!(cpu_delta_us(None, None), None);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn statm_parser_validates_pages_units_and_overflow() {
        assert_eq!(linux_rss_from_statm("100 7 3 2 0 4 0", 4096), Some(28_672));
        assert_eq!(linux_rss_from_statm("100", 4096), None);
        assert_eq!(linux_rss_from_statm("100 invalid", 4096), None);
        assert_eq!(linux_rss_from_statm("100 0", 4096), None);
        assert_eq!(linux_rss_from_statm("100 1", 0), None);
        assert_eq!(linux_rss_from_statm("100 9223372036854775807", 4096), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rss_mapping_child() {
        if std::env::var_os("KHIVE_TEST_RSS_MAPPING_CHILD").is_none() {
            return;
        }
        const BYTES: usize = 96 * 1024 * 1024;
        // SAFETY: sysconf has no pointer arguments.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page > 0, "fixture: page size unavailable");
        let baseline_rss = platform_current_rss_bytes().expect("fixture: baseline OS current RSS");
        let baseline_reported = current_rss_bytes();
        let before = process_resource_usage().expect("fixture: baseline getrusage");
        assert!(baseline_rss > 0, "fixture: baseline current RSS");
        assert!(before.max_rss_bytes > 0, "fixture: baseline peak RSS");
        // SAFETY: anonymous private mapping is exclusively owned by this child.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED, "fixture: mmap failed");
        for offset in (0..BYTES).step_by(page as usize) {
            // SAFETY: offsets are within the fresh writable mapping.
            unsafe {
                (address as *mut u8).add(offset).write_volatile(1);
            }
        }
        let high_rss =
            platform_current_rss_bytes().expect("fixture: OS current RSS after touching pages");
        let high = process_resource_usage().expect("fixture: populated mapping sample");
        let high_peak = high.max_rss_bytes;
        let high_reported = current_rss_bytes();
        assert!(
            high_rss > baseline_rss + 32 * 1024 * 1024,
            "fixture: touched mapping did not raise observed current RSS enough"
        );
        // SAFETY: address and size are the matching live mmap allocation.
        assert_eq!(
            unsafe { libc::munmap(address, BYTES) },
            0,
            "fixture: munmap failed"
        );
        let mut observed_drop = false;
        for _ in 0..100 {
            let current =
                platform_current_rss_bytes().expect("fixture: post-unmap OS current sample");
            if current < high_rss - 32 * 1024 * 1024 {
                observed_drop = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(observed_drop, "fixture: no observed RSS drop within bound");
        let after = process_resource_usage().expect("fixture: post-unmap resource sample");
        let baseline_reported = baseline_reported.expect("reported baseline current RSS");
        let high_reported = high_reported.expect("reported current RSS with mapping");
        let current = current_rss_bytes().expect("reported current RSS after unmap");
        let peak = after.max_rss_bytes;
        assert!(
            high_reported > baseline_reported.saturating_add(32 * 1024 * 1024),
            "reported current RSS must rise with the populated mapping"
        );
        assert!(
            current < high_reported.saturating_sub(32 * 1024 * 1024),
            "reported current RSS must fall after the observed OS RSS drop"
        );
        assert!(peak >= high_peak, "lifetime peak must not fall");
        assert!(
            current.saturating_add(32 * 1024 * 1024) <= peak,
            "current RSS plus the qualified drop must not exceed the peak"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn current_rss_drops_in_isolated_mmap_child() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let executable = std::env::current_exe().expect("fixture: test executable");
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("resource::tests::rss_mapping_child")
            .arg("--nocapture")
            .env("KHIVE_TEST_RSS_MAPPING_CHILD", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fixture: spawn child");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child.try_wait().expect("fixture: wait child").is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixture: child timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().expect("fixture: collect child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("running 1 test"),
            "fixture: expected one test: {stdout} {stderr}"
        );
        assert!(output.status.success(), "child failed: {stdout} {stderr}");
    }
}
