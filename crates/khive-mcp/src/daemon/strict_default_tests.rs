//! `KHIVE_DAEMON_STRICT` as a tri-state whose unset value follows the daemon
//! rendezvous (ADR-049 Amendment 14 §2).

use super::test_harness::clear_daemon_env;
use super::*;
use serial_test::serial;

const CFG: &str = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

/// A private socket, PID and lock path in an empty directory, with
/// `KHIVE_DAEMON_STRICT` set as given. Dropping it clears all of them.
struct Rendezvous {
    dir: tempfile::TempDir,
}

impl Rendezvous {
    fn new(strict: Option<&str>) -> Self {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        set_strict(strict);
        Self { dir }
    }

    fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("khived.sock")
    }

    fn pid_file(&self) -> std::path::PathBuf {
        self.dir.path().join("khived.pid")
    }

    /// The socket file a daemon leaves when it dies without cleanup: present
    /// on disk, refusing every connection.
    fn leave_refusing_socket(&self) {
        drop(std::os::unix::net::UnixListener::bind(self.socket()).expect("bind socket fixture"));
        assert_eq!(
            std::os::unix::net::UnixStream::connect(self.socket())
                .expect_err("the fixture socket must refuse")
                .kind(),
            std::io::ErrorKind::ConnectionRefused
        );
    }

    /// A PID file naming a process that has exited and been reaped.
    fn leave_stale_pid_file(&self) {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived process");
        let pid = child.id();
        child.wait().expect("reap the short-lived process");
        // SAFETY: signal 0 only checks that the pid exists; it sends nothing.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!alive, "pid {pid} must be dead for the stale-PID arm");
        std::fs::write(self.pid_file(), format!("{pid}\n")).expect("write PID file fixture");
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        clear_daemon_env();
        std::env::remove_var("KHIVE_DAEMON_STRICT");
    }
}

fn set_strict(value: Option<&str>) {
    match value {
        Some(value) => std::env::set_var("KHIVE_DAEMON_STRICT", value),
        None => std::env::remove_var("KHIVE_DAEMON_STRICT"),
    }
}

#[test]
#[serial]
fn unset_strict_follows_the_rendezvous_on_disk() {
    let rendezvous = Rendezvous::new(None);
    assert!(!is_daemon_strict_mode(), "no socket and no PID file");

    rendezvous.leave_refusing_socket();
    assert!(
        is_daemon_strict_mode(),
        "a refusing socket file is a rendezvous"
    );
    std::fs::remove_file(rendezvous.socket()).unwrap();

    rendezvous.leave_stale_pid_file();
    assert!(
        is_daemon_strict_mode(),
        "a PID file naming a dead process is a rendezvous"
    );

    set_strict(Some(""));
    assert!(is_daemon_strict_mode(), "an empty value reads as unset");
    std::fs::remove_file(rendezvous.pid_file()).unwrap();
    assert!(
        !is_daemon_strict_mode(),
        "an empty value with nothing on disk"
    );
}

#[test]
#[serial]
fn explicit_false_spellings_override_a_present_rendezvous() {
    let rendezvous = Rendezvous::new(None);
    rendezvous.leave_refusing_socket();
    rendezvous.leave_stale_pid_file();
    for value in ["0", "false", "FALSE", "No", "off", " off "] {
        set_strict(Some(value));
        assert!(!is_daemon_strict_mode(), "KHIVE_DAEMON_STRICT={value:?}");
    }
}

#[test]
#[serial]
fn any_other_value_is_strict_without_a_rendezvous() {
    let _rendezvous = Rendezvous::new(None);
    for value in ["1", "true", "yes", "on", "strict", "2"] {
        set_strict(Some(value));
        assert!(is_daemon_strict_mode(), "KHIVE_DAEMON_STRICT={value:?}");
    }
}

#[tokio::test]
#[serial]
async fn a_rendezvous_seen_at_admission_holds_after_recovery_removes_it() {
    let rendezvous = Rendezvous::new(None);
    rendezvous.leave_stale_pid_file();
    let strict_inside = with_rendezvous_admission_snapshot(async {
        // What recovery does before it respawns.
        std::fs::remove_file(rendezvous.pid_file()).unwrap();
        is_daemon_strict_mode()
    })
    .await;
    assert!(
        strict_inside,
        "the request admitted with a stale PID file must stay strict"
    );
    assert!(
        !is_daemon_strict_mode(),
        "outside a forward the reading is taken from disk"
    );
}

// Acceptance 3, through the real forward path. The injected executable stays
// alive without binding, so the forward reaches its no-socket fallback
// decision after the readiness wait, exactly as a host whose daemon never
// comes up does. Recovery removes the stale socket or PID file before it
// spawns, so these arms also prove the admission-time reading.

const NEVER_BINDS: &str = "#!/bin/sh\nsleep 10\n";

fn never_binding_exe(rendezvous: &Rendezvous) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = rendezvous.dir.path().join("never-binds");
    std::fs::write(&path, NEVER_BINDS).expect("write executable fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make executable fixture executable");
    path
}

fn frame() -> DaemonRequestFrame {
    DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
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

fn assert_rejected_with_no_socket(out: Option<Result<String, McpError>>) {
    match out {
        Some(Err(error)) => {
            let data = error.data.expect("strict rejection carries data");
            assert_eq!(data["reason"], "no_socket", "{data}");
            assert_eq!(data[STRICT_FALLBACK_MARKER], true, "{data}");
        }
        other => panic!("expected a strict no_socket rejection, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn refusing_socket_with_strict_unset_rejects_with_no_socket() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    reset_fallback_counters();
    let rendezvous = Rendezvous::new(None);
    rendezvous.leave_refusing_socket();
    let exe = never_binding_exe(&rendezvous);

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    assert_rejected_with_no_socket(out);
    assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
}

#[tokio::test]
#[serial]
async fn refusing_socket_with_strict_zero_falls_back() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    reset_fallback_counters();
    let rendezvous = Rendezvous::new(Some("0"));
    rendezvous.leave_refusing_socket();
    let exe = never_binding_exe(&rendezvous);

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    assert!(
        out.is_none(),
        "KHIVE_DAEMON_STRICT=0 keeps local dispatch: {out:?}"
    );
    assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
}

#[tokio::test]
#[serial]
async fn no_rendezvous_with_strict_unset_falls_back_quietly() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    reset_fallback_counters();
    let rendezvous = Rendezvous::new(None);
    let exe = never_binding_exe(&rendezvous);

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    assert!(
        out.is_none(),
        "no socket and no PID file keeps Amendment 2's fallback: {out:?}"
    );
    assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
    assert_eq!(fallback_strict_violations(), 0);
}

#[tokio::test]
#[serial]
async fn stale_pid_file_with_strict_unset_rejects_with_no_socket() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    reset_fallback_counters();
    let rendezvous = Rendezvous::new(None);
    rendezvous.leave_stale_pid_file();
    let exe = never_binding_exe(&rendezvous);

    let out = forward_or_spawn_with_exe(&frame(), &exe).await;

    assert_rejected_with_no_socket(out);
}
