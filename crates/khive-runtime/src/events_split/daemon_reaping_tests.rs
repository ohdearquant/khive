use super::poll_events_daemon_child;
use std::io;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

struct ReapOnDrop(libc::pid_t);

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        loop {
            let mut status = 0;
            // SAFETY: this fixture owns the exact child PID; status is writable.
            let result = unsafe { libc::waitpid(self.0, &mut status, libc::WNOHANG) };
            if result == self.0 {
                return;
            }
            if result == -1 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            // No other thread reaps this fixture, so an unreaped child still owns its PID.
            // SAFETY: waitpid just confirmed this exact PID is our running child.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
            loop {
                // SAFETY: the exact child PID and writable status remain valid.
                let result = unsafe { libc::waitpid(self.0, &mut status, 0) };
                if result != -1 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return;
                }
            }
        }
    }
}

fn held_child() -> (Option<Child>, ChildStdin, ReapOnDrop) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read -r line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn held child");
    let cleanup = ReapOnDrop(child.id() as libc::pid_t);
    // A regression that drops Child must not also release the child from its read.
    let release = child.stdin.take().expect("piped stdin");
    (Some(child), release, cleanup)
}

fn poll_until_reaped(child: &mut Option<Child>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.is_some() {
        poll_events_daemon_child(child, Child::try_wait);
        assert!(Instant::now() < deadline, "released child must exit");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn assert_reaped(pid: libc::pid_t) {
    let mut status = 0;
    // SAFETY: inspect only this fixture's PID, with a writable status.
    let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    assert_eq!(result, -1, "supervisor must already have reaped the child");
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn poll_error_retains_child_until_a_successful_reap() {
    // Exact-PID cleanup must not race other tests spawning or reaping children.
    if crate::test_process::run_in_child() {
        return;
    }
    for kind in [io::ErrorKind::Interrupted, io::ErrorKind::Other] {
        let (mut child, release, cleanup) = held_child();
        let pid = child.as_ref().unwrap().id();
        poll_events_daemon_child(&mut child, Child::try_wait);
        assert_eq!(child.as_ref().map(Child::id), Some(pid));

        poll_events_daemon_child(&mut child, |_| Err(io::Error::from(kind)));
        assert_eq!(
            child.as_ref().map(Child::id),
            Some(pid),
            "a poll error must retain ownership and keep the spawn slot occupied"
        );

        drop(release);
        poll_until_reaped(&mut child);
        assert_reaped(cleanup.0);
    }
}

#[test]
fn externally_reaped_child_clears_slot_on_echild() {
    if crate::test_process::run_in_child() {
        return;
    }
    let (mut child, release, cleanup) = held_child();
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut status = 0;
        // SAFETY: this fixture owns the exact child PID and writable status.
        let result = unsafe { libc::waitpid(cleanup.0, &mut status, libc::WNOHANG) };
        if result == cleanup.0 {
            break;
        }
        assert_eq!(result, 0, "external waiter must own the child");
        assert!(Instant::now() < deadline, "released child must exit");
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut polled = false;
    poll_events_daemon_child(&mut child, |child| {
        polled = true;
        let result = child.try_wait();
        assert_eq!(
            result.as_ref().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        result
    });
    assert!(polled);
    assert!(child.is_none(), "ECHILD must not retain an unusable handle");
    assert_reaped(cleanup.0);
}

#[test]
fn empty_child_slot_is_not_polled() {
    poll_events_daemon_child(&mut None, |_| panic!("empty slot must not be polled"));
}
