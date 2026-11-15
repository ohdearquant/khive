// Included by daemon::tests::exit_records (ADR-185 Amendment 5).

use super::*;
use super::super::exit_record::{
    exit_record_path, read_previous, ExitReason, ExitRecord, PreviousExit, SENDER_CAPTURE_LATE,
};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[test]
fn the_record_lives_beside_the_pid_file() {
    assert_eq!(
        exit_record_path(std::path::Path::new("/run/khive/khived.pid")),
        PathBuf::from("/run/khive/khived.pid.exit")
    );
}

#[test]
fn reasons_are_spelled_as_the_record_format_says() {
    assert_eq!(ExitReason::Signal("SIGTERM").to_string(), "signal SIGTERM");
    assert_eq!(ExitReason::Signal("SIGINT").to_string(), "signal SIGINT");
    assert_eq!(ExitReason::Idle.to_string(), "idle");
    assert_eq!(ExitReason::Error.to_string(), "error");
}

#[test]
fn the_previous_record_is_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("khived.pid.exit");
    assert_eq!(read_previous(&record), PreviousExit::Absent);

    let exited = "exited pid=7 reason=signal SIGTERM sender_pid=9 at=2026-10-13T00:00:00.000Z";
    std::fs::write(&record, format!("{exited}\n")).unwrap();
    assert_eq!(read_previous(&record), PreviousExit::Exited(exited.to_owned()));

    std::fs::write(&record, "running pid=42 started=2026-10-13T00:00:00.000Z\n").unwrap();
    assert_eq!(
        read_previous(&record),
        PreviousExit::NoExit {
            pid: "42".to_owned()
        }
    );

    std::fs::write(&record, "half-writ").unwrap();
    assert!(matches!(
        read_previous(&record),
        PreviousExit::Unreadable(_)
    ));

    std::fs::remove_file(&record).unwrap();
    std::fs::create_dir(&record).unwrap();
    assert!(matches!(
        read_previous(&record),
        PreviousExit::Unreadable(_)
    ));
}

fn directory_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn only_the_pid_file_owner_writes_the_record_and_each_write_is_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let record_path = exit_record_path(&pid_file);
    let me = std::process::id();
    std::fs::write(&pid_file, me.to_string()).unwrap();

    let record = ExitRecord::claim(&pid_file);
    let running = std::fs::read_to_string(&record_path).unwrap();
    assert!(
        running.starts_with(&format!("running pid={me} started=")) && running.ends_with('\n'),
        "{running:?}"
    );
    assert_eq!(running.lines().count(), 1);

    record.exiting(ExitReason::Idle);
    let exited = std::fs::read_to_string(&record_path).unwrap();
    assert!(
        exited.starts_with(&format!("exited pid={me} reason=idle sender_pid=unknown at=")),
        "{exited:?}"
    );
    assert_eq!(
        directory_names(dir.path()),
        ["khived.pid", "khived.pid.exit"],
        "no temporary file is left behind"
    );

    // A replacement daemon now owns the PID file: a late write must not land.
    std::fs::write(&pid_file, (me + 1).to_string()).unwrap();
    record.exiting(ExitReason::Signal("SIGTERM"));
    assert_eq!(std::fs::read_to_string(&record_path).unwrap(), exited);
    assert_eq!(
        directory_names(dir.path()),
        ["khived.pid", "khived.pid.exit"]
    );
}

// The record across real processes: a daemon child stopped by a known
// sender, the next boot reading it, a SIGKILL that leaves no exit line, and
// the boot after that naming the missing exit.

const DEADLINE: Duration = Duration::from_secs(10);

struct DaemonChild {
    child: Child,
    stderr: PathBuf,
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl DaemonChild {
    fn spawn(dir: &std::path::Path, boot: usize, nofile_soft: Option<libc::rlim_t>) -> Self {
        use std::os::unix::process::CommandExt as _;
        let stderr = dir.join(format!("boot-{boot}.stderr"));
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "daemon::tests::exit_records::exit_record_daemon_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .envs(
                std::env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("KHIVE_")),
            )
            .env("KHIVE_SOCKET", dir.join("s"))
            .env("KHIVE_PID", dir.join("p"))
            .env("KHIVE_LOCK", dir.join("l"))
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).expect("stderr capture"));
        if let Some(soft) = nofile_soft {
            // SAFETY: getrlimit/setrlimit are async-signal-safe system calls on
            // a stack value; nothing else runs between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    limit.rlim_cur = soft.min(limit.rlim_max);
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().expect("spawn daemon child");
        Self { child, stderr }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait_until_listening(&mut self, sock: &std::path::Path) {
        let deadline = Instant::now() + DEADLINE;
        while std::os::unix::net::UnixStream::connect(sock).is_err() {
            assert!(
                self.child.try_wait().expect("poll child").is_none(),
                "daemon child exited before listening:\n{}",
                self.log()
            );
            assert!(Instant::now() < deadline, "daemon child never listened");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

#[test]
fn the_exit_record_names_the_signal_and_its_sender_and_the_next_boot_reports_it() {
    let dir = crate::test_process::private_tempdir_builder()
        .prefix("kh-exit-")
        .tempdir_in("/tmp")
        .expect("isolated short socket directory");
    let sock = dir.path().join("s");
    let record = exit_record_path(&dir.path().join("p"));

    // Boot 1, under a 256-descriptor soft limit.
    let mut first = DaemonChild::spawn(dir.path(), 1, Some(256));
    first.wait_until_listening(&sock);
    let first_pid = first.pid();
    let running = std::fs::read_to_string(&record).expect("running record");
    assert!(
        running.starts_with(&format!("running pid={first_pid} started=")),
        "{running:?}"
    );
    let log = first.log();
    assert!(
        log.contains("descriptor limit") && log.contains("nofile_soft=256"),
        "boot must log the effective descriptor limit:\n{log}"
    );
    // The recorder must run before tokio's wakeup, or the exit line below can
    // name no sender; the registry runs actions in registration order.
    assert!(
        !log.contains(SENDER_CAPTURE_LATE),
        "the sender recorder must be registered before the shutdown streams:\n{log}"
    );

    // A known process sends SIGTERM.
    let mut sender = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {first_pid}")])
        .spawn()
        .expect("spawn sender");
    let sender_pid = sender.id();
    assert!(sender.wait().expect("sender exit").success());
    let status = first.wait_for_exit();
    assert!(status.success(), "graceful exit: {status}\n{}", first.log());
    let exited = std::fs::read_to_string(&record).expect("exit record");
    assert!(
        exited.starts_with(&format!(
            "exited pid={first_pid} reason=signal SIGTERM sender_pid={sender_pid} at="
        )),
        "{exited:?}"
    );
    let log = first.log();
    assert!(
        log.contains("khived exiting") && log.contains(&format!("sender_pid={sender_pid}")),
        "the shutdown line must name the sender:\n{log}"
    );

    // Boot 2 reports it, then dies by SIGKILL without writing an exit line.
    let mut second = DaemonChild::spawn(dir.path(), 2, None);
    second.wait_until_listening(&sock);
    let second_pid = second.pid();
    let log = second.log();
    assert!(
        log.contains(&format!(
            "previous exit: exited pid={first_pid} reason=signal SIGTERM sender_pid={sender_pid}"
        )),
        "{log}"
    );
    second.child.kill().expect("SIGKILL boot 2");
    second.child.wait().expect("reap boot 2");
    let running = std::fs::read_to_string(&record).unwrap();
    assert!(
        running.starts_with(&format!("running pid={second_pid} ")),
        "{running:?}"
    );

    // Boot 3 names the missing exit; this test process stops it.
    let mut third = DaemonChild::spawn(dir.path(), 3, None);
    third.wait_until_listening(&sock);
    let log = third.log();
    assert!(
        log.contains(&format!(
            "previous incarnation pid={second_pid} recorded no exit (killed or crashed)"
        )),
        "{log}"
    );
    let third_pid = third.pid();
    // SAFETY: the still-owned child pid and SIGTERM are valid.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(third_pid).unwrap(), libc::SIGTERM) },
        0
    );
    assert!(third.wait_for_exit().success(), "{}", third.log());
    let exited = std::fs::read_to_string(&record).unwrap();
    assert!(
        exited.starts_with(&format!(
            "exited pid={third_pid} reason=signal SIGTERM sender_pid={} at=",
            std::process::id()
        )),
        "{exited:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "subprocess helper, invoked by the_exit_record_names_the_signal_and_its_sender_and_the_next_boot_reports_it"]
async fn exit_record_daemon_child() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("warn,khive.boot=info")
        .with_ansi(false)
        .init();
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "exit-record-test".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let boot_guard = Some(acquire_daemon_boot_guard().expect("boot guard"));
    run_daemon_with_boot_guard_and_start(dispatcher, boot_guard, |_| {})
        .await
        .expect("daemon shutdown");
}
