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
