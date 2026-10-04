//! One guarded signal to a whole process group.

/// Send `signal` to every member of the process group `pgid`.
///
/// A group id of 0 or 1, or any negative id, is refused with an
/// [`std::io::ErrorKind::InvalidInput`] error before the OS is called. Negated,
/// 0 names the caller's own group, 1 names every process the caller may signal,
/// and a negative id names a single process rather than a group. Any other
/// failure is the OS error, unchanged: `ESRCH` once the group has no member left.
///
/// Which group to signal, whether a missing group is an error, and what to do
/// after the signal all stay with the caller.
#[cfg(unix)]
pub fn signal_process_group(pgid: i32, signal: i32) -> std::io::Result<()> {
    if pgid <= 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to signal process group {pgid}"),
        ));
    }
    // SAFETY: `kill` borrows no Rust memory or descriptors, and `pgid > 1`
    // keeps `-pgid` clear of the "own group" (0) and "every process" (-1)
    // forms, so the signal reaches the one group the caller named.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;

    use super::signal_process_group;

    #[test]
    fn signal_process_group_kills_a_child_group_then_reports_esrch() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn a child into its own process group");
        let pgid = child.id() as i32;

        signal_process_group(pgid, libc::SIGKILL).expect("signal the live group");
        let status = child.wait().expect("reap the killed child");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "the child must die to the group signal"
        );

        // Probe with signal 0: nothing is delivered if the id has been reused.
        let error = signal_process_group(pgid, 0).expect_err("the group is gone");
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn signal_process_group_refuses_ids_that_do_not_name_one_group() {
        for pgid in [0, 1, -5] {
            // Signal 0 delivers nothing, so a missing guard cannot harm the host.
            match signal_process_group(pgid, 0) {
                Ok(()) => panic!("process group id {pgid} reached the OS"),
                Err(error) => {
                    assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::InvalidInput,
                        "process group id {pgid} must be refused as invalid input"
                    );
                    assert_eq!(
                        error.raw_os_error(),
                        None,
                        "process group id {pgid} must be refused without an OS call"
                    );
                }
            }
        }
    }
}
