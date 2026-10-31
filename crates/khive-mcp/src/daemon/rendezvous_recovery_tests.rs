//! Client lifecycle recovery acts on one rendezvous and never signals a
//! supervised daemon (ADR-185 Amendment 5).

use super::test_harness::{clear_daemon_env, reset_counters, RecoveryTestGuard};
use super::*;
use serial_test::serial;
use std::sync::atomic::{AtomicUsize, Ordering};

const CFG: &str = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
const NS: &str = "test";

fn frame() -> DaemonRequestFrame {
    DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: NS.to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: CFG.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    }
}

/// A live process standing in for a daemon. With `FORCE_PID_IS_DAEMON` set,
/// recovery classifies it as a khive daemon and would SIGTERM it.
fn live_incumbent(cleanup: &mut RecoveryTestGuard) -> u32 {
    let child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn stand-in daemon");
    let pid = cleanup.track_child(child);
    FORCE_PID_IS_DAEMON.store(true, Ordering::SeqCst);
    pid
}

/// An executable that records it was launched, then never binds.
fn recording_exe(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let launched = dir.join("launched");
    let exe = dir.join("recording-exe");
    std::fs::write(
        &exe,
        format!("#!/bin/sh\n: > '{}'\nsleep 10\n", launched.display()),
    )
    .expect("write executable fixture");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
        .expect("make executable fixture executable");
    (exe, launched)
}

/// The default rendezvous directory under the private HOME, created the way
/// daemon startup creates it, so its PID file is trusted.
fn default_rendezvous_dir() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("private HOME"));
    let dir = home.join(".khive");
    std::fs::create_dir_all(&dir).expect("create default rendezvous dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("owner-only rendezvous dir");
    dir
}

fn refusal_data(out: Option<Result<String, McpError>>, reason: &str) -> serde_json::Value {
    match out {
        Some(Err(error)) => {
            let data = error.data.expect("refusal carries data");
            assert_eq!(data["reason"], reason, "{data}");
            assert_eq!(data["domain_disposition"], "not_committed", "{data}");
            data
        }
        other => panic!("expected a {reason} refusal, got {other:?}"),
    }
}

/// The incident shape: a private socket, the PID file left at its default,
/// and the default PID file naming a live daemon.
#[tokio::test]
#[serial]
async fn socket_override_alone_refuses_recovery_and_leaves_the_default_daemon_alive() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let private_socket = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &private_socket);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    let default_pid_file = default_rendezvous_dir().join("khived.pid");
    assert_eq!(
        pid_path(),
        default_pid_file,
        "KHIVE_PID unset reads the default"
    );
    let incumbent = live_incumbent(&mut cleanup);
    std::fs::write(&default_pid_file, incumbent.to_string()).expect("write default PID file");
    let (exe, launched) = recording_exe(dir.path());

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    let data = refusal_data(out, "rendezvous_split");
    assert_eq!(data["set"], "KHIVE_SOCKET", "{data}");
    assert_eq!(data["unset"], "KHIVE_PID", "{data}");
    assert_eq!(
        data["socket"],
        private_socket.display().to_string(),
        "{data}"
    );
    assert_eq!(
        data["pid_file"],
        default_pid_file.display().to_string(),
        "{data}"
    );
    assert!(
        process_is_alive(incumbent),
        "the default daemon must be alive"
    );
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0, "no recovery entered");
    assert!(!launched.exists(), "nothing spawned");
    assert_eq!(
        std::fs::read_to_string(&default_pid_file).unwrap(),
        incumbent.to_string()
    );
}

/// The mirror shape: a private PID file naming a live daemon, the socket left
/// at its default.
#[tokio::test]
#[serial]
async fn pid_override_alone_refuses_recovery() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let private_pid_file = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_PID", &private_pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    let default_socket = default_rendezvous_dir().join("khived.sock");
    assert_eq!(
        socket_path(),
        default_socket,
        "KHIVE_SOCKET unset reads the default"
    );
    let incumbent = live_incumbent(&mut cleanup);
    std::fs::write(&private_pid_file, incumbent.to_string()).expect("write PID file");
    let (exe, launched) = recording_exe(dir.path());

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    let data = refusal_data(out, "rendezvous_split");
    assert_eq!(data["set"], "KHIVE_PID", "{data}");
    assert_eq!(data["unset"], "KHIVE_SOCKET", "{data}");
    assert_eq!(
        data["socket"],
        default_socket.display().to_string(),
        "{data}"
    );
    assert_eq!(
        data["pid_file"],
        private_pid_file.display().to_string(),
        "{data}"
    );
    assert!(process_is_alive(incumbent));
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert!(!launched.exists(), "nothing spawned");
}

/// Control: the same live process behind a paired private rendezvous is
/// recovered as before (signalled, then a daemon is launched).
#[tokio::test]
#[serial]
async fn paired_private_rendezvous_still_recovers() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    let incumbent = live_incumbent(&mut cleanup);
    std::fs::write(&pid_file, incumbent.to_string()).expect("write PID file");
    let (exe, launched) = recording_exe(dir.path());

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    if let Some(Err(error)) = &out {
        assert_ne!(
            error.data.as_ref().map(|data| data["reason"].clone()),
            Some(serde_json::json!("rendezvous_split")),
            "a paired rendezvous is never split"
        );
    }
    assert_eq!(
        SIGTERM_COUNT.load(Ordering::SeqCst),
        1,
        "recovery signalled the incumbent"
    );
    assert!(!process_is_alive(incumbent));
    assert!(launched.exists(), "recovery launched a daemon");
}

/// A private rendezvous whose PID file names a live, unresponsive daemon.
fn claimed_rendezvous(cleanup: &mut RecoveryTestGuard, dir: &std::path::Path) -> u32 {
    clear_daemon_env();
    reset_counters();
    let pid_file = dir.join("khived.pid");
    let socket = dir.join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &socket);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.join("khived.recovery.lock"));
    std::env::set_var("KHIVE_RECOVERER_LOCK", dir.join("khived.recoverer.lock"));
    std::env::set_var("KHIVE_SUPERVISOR_MARKER", dir.join("khived.supervisor"));
    // A socket file that refuses every connection: the daemon is not serving.
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind socket fixture"));
    let incumbent = live_incumbent(cleanup);
    std::fs::write(&pid_file, incumbent.to_string()).expect("write PID file");
    incumbent
}

fn write_marker(job: &str, pid: u32, interval_secs: u64) {
    let path = khive_runtime::daemon::supervisor_marker_path();
    std::fs::write(path, format!("{job}\n{pid}\n{interval_secs}\n")).expect("write marker");
}

async fn recover(spawn_calls: &AtomicUsize) -> Result<RecoveryOutcome, RecoveryError> {
    let spawn = || {
        spawn_calls.fetch_add(1, Ordering::SeqCst);
        std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
    };
    kill_and_respawn_with_exit_timeout(CFG, NS, &spawn, std::time::Duration::from_secs(2)).await
}

#[tokio::test]
#[serial]
async fn a_claimed_rendezvous_daemon_is_never_signalled() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let incumbent = claimed_rendezvous(&mut cleanup, dir.path());
    write_marker("example.job", std::process::id(), 1);
    let spawn_calls = AtomicUsize::new(0);

    let outcome = recover(&spawn_calls).await;

    match outcome {
        Err(RecoveryError::SupervisedDaemonUnresponsive { pid, marker }) => {
            assert_eq!(pid, incumbent);
            assert_eq!(marker.job, "example.job");
            assert_eq!(marker.pid, std::process::id());
        }
        other => panic!("expected SupervisedDaemonUnresponsive, got {other:?}"),
    }
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert!(process_is_alive(incumbent));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(pid_path()).unwrap(),
        incumbent.to_string()
    );
}

#[tokio::test]
#[serial]
async fn an_unreadable_marker_still_protects_the_daemon() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let incumbent = claimed_rendezvous(&mut cleanup, dir.path());
    // A directory at the marker path is unreadable as marker text.
    std::fs::create_dir(khive_runtime::daemon::supervisor_marker_path()).unwrap();
    let spawn_calls = AtomicUsize::new(0);

    let outcome = recover(&spawn_calls).await;

    assert!(
        matches!(
            &outcome,
            Err(RecoveryError::SupervisedDaemonUnresponsive { pid, marker })
                if *pid == incumbent && marker.job == "<unreadable>"
        ),
        "{outcome:?}"
    );
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert!(process_is_alive(incumbent));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
}

/// Control: with no marker the same daemon is signalled and replaced, as
/// before the amendment.
#[tokio::test]
#[serial]
async fn an_unclaimed_rendezvous_daemon_is_still_signalled() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let incumbent = claimed_rendezvous(&mut cleanup, dir.path());
    let spawn_calls = AtomicUsize::new(0);

    let outcome = recover(&spawn_calls).await;

    match outcome {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
        }
        other => panic!("expected a respawn after the signal, got {other:?}"),
    }
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 1);
    assert!(!process_is_alive(incumbent));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
}

/// Through the forward path: the supervisor's bounded wait expires (degraded
/// bootstrap), and the request is refused with the marker and both pids named.
#[tokio::test]
#[serial]
async fn degraded_bootstrap_refuses_to_replace_a_live_supervised_daemon() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let mut cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let incumbent = claimed_rendezvous(&mut cleanup, dir.path());
    write_marker("example.job", std::process::id(), 1);
    let (exe, launched) = recording_exe(dir.path());

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    let data = refusal_data(out, "supervised_daemon_unresponsive");
    assert_eq!(data["pid"], incumbent, "{data}");
    assert_eq!(data["job"], "example.job", "{data}");
    assert_eq!(data["marker_pid"], std::process::id(), "{data}");
    assert_eq!(
        data["marker"],
        khive_runtime::daemon::supervisor_marker_path()
            .display()
            .to_string(),
        "{data}"
    );
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert!(process_is_alive(incumbent));
    assert!(!launched.exists(), "nothing spawned");
}

#[tokio::test]
#[serial]
async fn recovery_repairs_default_directory_but_probes_do_not() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    let pid_file = directory.join("khived.pid");

    let refusal = pid_file_directory_is_trusted_if_present(&pid_file).unwrap_err();
    assert!(refusal.contains("0775"), "{refusal}");
    assert!(!refusal.contains("KHIVE_PID"), "{refusal}");
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o775
    );

    let spawn_calls = AtomicUsize::new(0);
    match recover(&spawn_calls).await {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
        }
        other => panic!("expected default recovery to spawn, got {other:?}"),
    }
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[tokio::test]
#[serial]
async fn recovery_from_fresh_home_under_umask_002() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("private HOME"));
    let directory = home.join(".khive");
    assert!(!directory.exists(), "fixture must start with a fresh HOME");
    let spawn_calls = AtomicUsize::new(0);

    // SAFETY: the private-home test runs in a separate process, so its umask
    // is not shared with the parent test runner.
    let previous_umask = unsafe { libc::umask(0o002) };
    let outcome = recover(&spawn_calls).await;
    // SAFETY: restore the value returned by umask in this isolated process.
    unsafe { libc::umask(previous_umask) };

    match outcome {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
        }
        other => panic!("expected fresh-home recovery to spawn, got {other:?}"),
    }
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[tokio::test]
#[serial]
async fn fresh_home_paired_overrides_still_prepare_default_lock_parent() {
    use std::os::unix::fs::PermissionsExt;
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    assert!(!home.join(".khive").exists());
    let override_dir = tempfile::tempdir_in(&home).unwrap();
    std::fs::set_permissions(override_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("KHIVE_PID", override_dir.path().join("daemon.pid"));
    std::env::set_var("KHIVE_SOCKET", override_dir.path().join("daemon.sock"));
    let spawn_calls = AtomicUsize::new(0);
    match recover(&spawn_calls).await {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
        }
        other => panic!("expected recovery using the default lock parent: {other:?}"),
    }
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::metadata(home.join(".khive"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test]
#[serial]
async fn failure_of_any_recovery_boot_fence_never_authorizes_later_actions() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    default_rendezvous_dir();
    let spawn_calls = AtomicUsize::new(0);
    for stage in 0..=2 {
        reset_counters();
        std::fs::write(pid_path(), b"stale").unwrap();
        super::recovery::FAIL_RECOVERY_BOOT_LOCK_STAGE.store(stage, Ordering::SeqCst);
        let result = recover(&spawn_calls).await;
        super::recovery::FAIL_RECOVERY_BOOT_LOCK_STAGE.store(usize::MAX, Ordering::SeqCst);
        assert!(
            matches!(result, Ok(RecoveryOutcome::Uncertain)),
            "stage {stage}: {result:?}"
        );
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
        assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), usize::from(stage == 2));
        assert_eq!(std::fs::read(pid_path()).unwrap(), b"stale");
    }
    reset_counters();
    match recover(&spawn_calls).await {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
        }
        other => panic!("baseline recovery must reach the launcher: {other:?}"),
    }
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 1);
    assert!(!pid_path().exists());
}

#[tokio::test]
#[serial]
async fn recovery_refuses_insecure_explicit_override_without_chmod() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    default_rendezvous_dir();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    let spawn_calls = AtomicUsize::new(0);

    let outcome = recover(&spawn_calls).await;

    assert!(matches!(
        outcome,
        Err(RecoveryError::PidFileDirectoryUntrusted(_))
    ));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o775
    );
}

#[test]
#[serial]
fn explicitly_selected_default_directory_is_not_repaired_by_recovery() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    std::env::set_var("KHIVE_PID", directory.join("khived.pid"));
    std::env::set_var("KHIVE_SOCKET", directory.join("khived.sock"));

    daemon::prepare_default_rendezvous_for_recovery().unwrap();

    assert!(pid_file_directory_is_trusted_if_present(&pid_path()).is_err());
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o775
    );
}

#[test]
#[serial]
fn empty_overrides_follow_the_default_repair_path() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    std::env::set_var("KHIVE_PID", "");
    std::env::set_var("KHIVE_SOCKET", "");

    daemon::prepare_default_rendezvous_for_recovery().unwrap();

    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
#[serial]
fn read_only_pid_probe_does_not_create_an_absent_default_directory() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("private HOME"));
    let directory = home.join(".khive");
    assert!(!directory.exists(), "fixture must start with a fresh HOME");

    assert!(!pid_file_directory_is_trusted_if_present(&directory.join("khived.pid")).unwrap());

    assert!(
        !directory.exists(),
        "a read-only probe must not create a directory"
    );
}

#[test]
#[serial]
fn default_recovery_allows_a_trusted_existing_override_parent() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    let outside_lock = directory.parent().unwrap().join("outside.lock");
    std::env::set_var("KHIVE_LOCK", &outside_lock);

    daemon::prepare_default_rendezvous_for_recovery().unwrap();
    assert!(
        !outside_lock.exists(),
        "default preparation does not create the override lock"
    );
    let lock = daemon::open_mutating_daemon_lock(&outside_lock, false)
        .expect("trusted existing override parent");
    assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
#[serial]
async fn recovery_refuses_preplaced_lock_symlinks_before_opening_them() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    let target = directory.parent().unwrap().join("not-a-recovery-lock");
    let marker_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    for lock in [
        daemon::lock_path(),
        daemon::recoverer_lock_path(),
        marker_lock.clone(),
    ] {
        symlink(&target, &lock).unwrap();
        let spawn_calls = AtomicUsize::new(0);

        let outcome = recover(&spawn_calls).await;

        match outcome {
            Err(RecoveryError::PidFileDirectoryUntrusted(message)) => {
                assert!(message.contains(&lock.display().to_string()), "{message}");
            }
            other => panic!("expected a lock refusal before startup, got {other:?}"),
        }
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
        assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
        assert!(!target.exists(), "a symlink target must not be created");
        std::fs::remove_file(lock).unwrap();
    }
}

#[tokio::test]
#[serial]
#[ignore = "requires exclusive flock on a read-only descriptor and a qualified local filesystem"]
async fn recovery_waits_for_preopened_group_readable_locks() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    let marker_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    let boot_lock = daemon::lock_path();

    for lock in [
        boot_lock.clone(),
        daemon::recoverer_lock_path(),
        marker_lock.clone(),
    ] {
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let held = std::fs::File::open(&lock).expect("read-only lock fixture");
        // SAFETY: `held` is an open file descriptor; LOCK_NB bounds the setup.
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        if lock == marker_lock {
            daemon::prepare_default_rendezvous_for_recovery().unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();
            *super::forward::SUPERVISOR_LOCK_CONTENDED_HOOK
                .lock()
                .unwrap() = Some(sender);
            let mut task = tokio::spawn(super::forward::acquire_supervisor_marker_lock());
            let event = tokio::task::spawn_blocking(move || {
                receiver.recv_timeout(std::time::Duration::from_secs(3))
            });
            tokio::select! {
                result = &mut task => panic!("marker contender completed while holder retained lock: {result:?}"),
                received = event => received.unwrap().expect("fixture did not observe contention"),
            }
            drop(held);
            let guard = tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .expect("marker lock must acquire after release")
                .unwrap()
                .unwrap();
            drop(guard);
        } else {
            let attempt = if lock == boot_lock {
                daemon::try_acquire_daemon_boot_guard_until(std::time::Instant::now())
            } else {
                daemon::try_acquire_recoverer_lock_until(std::time::Instant::now())
            }
            .expect("qualified lock attempt");
            assert!(
                attempt.is_none(),
                "the preopened holder must be observed before release"
            );
            drop(held);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let guard = if lock == boot_lock {
                daemon::try_acquire_daemon_boot_guard_until(deadline)
            } else {
                daemon::try_acquire_recoverer_lock_until(deadline)
            }
            .expect("qualified lock attempt");
            assert!(guard.is_some(), "the lock must be available after release");
            drop(guard);
        }
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
        std::fs::remove_file(lock).unwrap();
    }
}

#[tokio::test]
#[serial]
async fn recovery_tightens_existing_owned_group_accessible_locks() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    default_rendezvous_dir();
    let marker_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    for mode in [0o644, 0o664] {
        for lock in [
            daemon::lock_path(),
            daemon::recoverer_lock_path(),
            marker_lock.clone(),
        ] {
            std::fs::write(&lock, b"").unwrap();
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(mode)).unwrap();
            let spawn_calls = AtomicUsize::new(0);

            let outcome = recover(&spawn_calls).await;

            match outcome {
                Ok(RecoveryOutcome::Spawned(mut child)) => {
                    let _ = child.wait();
                }
                other => panic!("expected recovery with tightened lock, got {other:?}"),
            }
            assert_eq!(
                std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
            assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
            std::fs::remove_file(lock).unwrap();
        }
    }
}

#[tokio::test]
#[serial]
async fn recovery_accepts_existing_owner_only_unheld_locks() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    default_rendezvous_dir();
    let marker_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    for lock in [
        daemon::lock_path(),
        daemon::recoverer_lock_path(),
        marker_lock,
    ] {
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let spawn_calls = AtomicUsize::new(0);

    match recover(&spawn_calls).await {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
        }
        other => panic!("expected owner-only locks to permit recovery, got {other:?}"),
    }
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
#[serial]
fn existing_restrictive_lock_does_not_gain_owner_permissions() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    default_rendezvous_dir();
    let lock = daemon::lock_path();
    std::fs::write(&lock, b"").unwrap();
    for mode in [0o400, 0o200] {
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(mode)).unwrap();
        daemon::prepare_default_rendezvous_for_recovery().unwrap();
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
}

#[tokio::test]
#[serial]
async fn new_supervisor_lock_is_owner_only_under_restrictive_umask() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    default_rendezvous_dir();
    let marker_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    assert!(!marker_lock.exists());
    // SAFETY: this test runs in an isolated private-home child process.
    let old_umask = unsafe { libc::umask(0o777) };
    let opened = super::forward::acquire_supervisor_marker_lock().await;
    // SAFETY: restore the umask returned in this isolated process.
    unsafe { libc::umask(old_umask) };

    let lock = opened.expect("create supervisor marker lock");
    assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
#[serial]
async fn supervisor_lock_opener_refuses_a_symlink() {
    use std::os::unix::fs::symlink;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    let target = directory.parent().unwrap().join("not-a-supervisor-lock");
    let lock = super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    symlink(&target, &lock).unwrap();

    let error = super::forward::acquire_supervisor_marker_lock()
        .await
        .unwrap_err();

    assert!(error.to_string().contains(&lock.display().to_string()));
    assert!(
        !target.exists(),
        "opening a lock must not create a symlink target"
    );
}

#[tokio::test]
#[serial]
async fn explicit_supervisor_lock_parent_must_exist_and_be_safe() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let root = tempfile::tempdir().expect("private synthetic root");
    let missing = root.path().join("missing");
    std::env::set_var("KHIVE_PID", missing.join("khived.pid"));
    std::env::set_var("KHIVE_SOCKET", missing.join("khived.sock"));
    let absent_error = super::forward::acquire_supervisor_marker_lock()
        .await
        .unwrap_err();
    assert!(absent_error
        .to_string()
        .contains(&missing.display().to_string()));
    assert!(
        !missing.exists(),
        "an explicit absent parent must not be created"
    );

    let unsafe_parent = root.path().join("unsafe");
    std::fs::create_dir(&unsafe_parent).unwrap();
    std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o775)).unwrap();
    std::env::set_var("KHIVE_PID", unsafe_parent.join("khived.pid"));
    std::env::set_var("KHIVE_SOCKET", unsafe_parent.join("khived.sock"));
    let unsafe_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    let unsafe_error = super::forward::acquire_supervisor_marker_lock()
        .await
        .unwrap_err();
    assert!(unsafe_error
        .to_string()
        .contains(&unsafe_parent.display().to_string()));
    assert!(
        !unsafe_lock.exists(),
        "an unsafe override parent must not gain a lock"
    );
    assert_eq!(
        std::fs::metadata(&unsafe_parent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o775
    );

    let target = root.path().join("target");
    let alias = root.path().join("alias");
    std::fs::create_dir(&target).unwrap();
    symlink(&target, &alias).unwrap();
    std::env::set_var("KHIVE_PID", alias.join("khived.pid"));
    std::env::set_var("KHIVE_SOCKET", alias.join("khived.sock"));
    assert!(super::forward::acquire_supervisor_marker_lock()
        .await
        .is_err());
    let target_lock = target.join("khived.sock.supervisor-marker.lock");
    assert!(
        !target_lock.exists(),
        "a symlink override parent must not be followed"
    );

    std::env::set_var("KHIVE_PID", target.join("khived.pid"));
    std::env::set_var("KHIVE_SOCKET", target.join("khived.sock"));
    let guard = super::forward::acquire_supervisor_marker_lock()
        .await
        .expect("trusted explicit parent positive control");
    assert_eq!(
        guard.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
#[serial]
async fn forward_refuses_preplaced_marker_lock_before_open() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    let target = directory.parent().unwrap().join("not-a-supervisor-lock");
    let marker_lock =
        super::forward::supervisor_marker_lock_path(&daemon::supervisor_marker_path());
    symlink(&target, &marker_lock).unwrap();
    let (exe, launched) = recording_exe(directory.parent().unwrap());

    let outcome = forward_or_spawn_with_exe(&frame(), &exe).await;

    let data = refusal_data(outcome, "untrusted_pid_file_directory");
    assert!(data["error"]
        .as_str()
        .unwrap()
        .contains(&marker_lock.display().to_string()));
    assert!(
        !target.exists(),
        "supervisor-lock open must not follow the symlink"
    );
    assert!(!launched.exists());
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
#[serial]
fn recovery_rejects_default_directory_symlink_without_chmodding_target() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    std::fs::remove_dir(&directory).unwrap();
    let target = directory.parent().unwrap().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o775)).unwrap();
    symlink(&target, &directory).unwrap();

    assert!(daemon::prepare_default_rendezvous_for_recovery().is_err());
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o775
    );
}

#[test]
#[serial]
fn recovery_rejects_unsafe_ancestor_before_chmod() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    let home = directory.parent().unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o775)).unwrap();

    let outcome = daemon::prepare_default_rendezvous_for_recovery();
    let mode = std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(outcome.is_err());
    assert_eq!(mode, 0o775);
}

#[test]
#[serial]
fn default_directory_repair_never_adds_owner_permissions() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o550)).unwrap();

    daemon::prepare_default_rendezvous_for_recovery().unwrap();
    let mode = std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode, 0o500);
}

#[test]
#[serial]
fn inaccessible_default_directory_never_gains_owner_permissions() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = default_rendezvous_dir();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000)).unwrap();

    let _outcome = daemon::prepare_default_rendezvous_for_recovery();

    let mode = std::fs::symlink_metadata(&directory)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode, 0o000);
}
