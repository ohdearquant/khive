//! Regression test for #2656 — the daemon's socket and PID file are two
//! halves of one rendezvous, so a boot that overrides exactly one of
//! `KHIVE_SOCKET` / `KHIVE_PID` must refuse rather than run with a private
//! socket and a shared PID file (or the reverse).
//!
//! Before the fix the two paths resolved independently and nothing coupled
//! them: a second daemon started with only `KHIVE_SOCKET` set bound its own
//! socket while claiming the default PID file, and the stale-daemon cleanup
//! then read an incumbent's pid out of a file belonging to a different daemon
//! — refusing over an unrelated pid when that process was alive, and deleting
//! a live daemon's PID file when it was not.
//!
//! Each case runs as the sole test in a child with private daemon paths.
//! Its changes to `HOME`, `KHIVE_SOCKET`, `KHIVE_PID`, and the process-wide
//! single-shot shutdown token cannot affect another case.

#![cfg(unix)]

#[path = "../src/test_process.rs"]
mod test_process;

use async_trait::async_trait;
use khive_runtime::daemon::{
    pid_path, read_frame, run_daemon_with_boot_guard, run_daemon_with_boot_guard_and_start,
    socket_path, write_frame, DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use khive_runtime::{DaemonDispatch, RequestIdentity};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;

/// The phrase only the pairing refusal emits. Asserted present in the two
/// unpaired cases and absent in the two paired ones, so the "boots" cases
/// cannot pass merely because some other error occurred first.
const PAIRING_REFUSAL_MARKER: &str = "two halves of one daemon rendezvous";
const PID_TRUST_CHILD: &str = "KHIVE_PID_TRUST_CHILD";
const PID_TRUST_ROOT: &str = "KHIVE_PID_TRUST_ROOT";

fn run_in_pid_trust_child() -> bool {
    let name = std::thread::current()
        .name()
        .expect("test thread has a name")
        .to_string();
    if std::env::var(PID_TRUST_CHILD).ok().as_deref() == Some(name.as_str()) {
        return false;
    }

    let fixture = tempfile::Builder::new()
        .prefix("kh-pid-trust-")
        .tempdir_in("/tmp")
        .expect("short isolated socket directory");
    let home = fixture.path().join("home");
    std::fs::create_dir(&home).expect("private HOME");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"));
    let mut output = output;
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            output.env_remove(key);
        }
    }
    let output = output
        .args(["--exact", name.as_str(), "--nocapture", "--test-threads=1"])
        .env(PID_TRUST_CHILD, &name)
        .env(PID_TRUST_ROOT, fixture.path())
        .env("HOME", &home)
        .env_remove("LATTICE_MODEL_CACHE")
        .output()
        .expect("run isolated daemon test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "isolated daemon test must pass:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn configure_pid_trust_paths(root: &std::path::Path, pid_mode: u32) -> std::path::PathBuf {
    let socket_dir = root.join("socket-dir");
    let pid_dir = root.join("pid-dir");
    std::fs::create_dir(&socket_dir).expect("socket parent");
    std::fs::create_dir(&pid_dir).expect("PID parent");
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))
        .expect("private socket parent");
    std::fs::set_permissions(&pid_dir, std::fs::Permissions::from_mode(pid_mode))
        .expect("set PID parent mode");

    std::env::set_var("KHIVE_SOCKET", socket_dir.join("khived.sock"));
    let pid_file = pid_dir.join("khived.pid");
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", root.join("boot.lock"));
    std::env::set_var("KHIVE_RECOVERER_LOCK", root.join("recoverer.lock"));
    pid_file
}

#[derive(Clone)]
struct NeverDispatch;

#[async_trait]
impl DaemonDispatch for NeverDispatch {
    fn plan(&self, _ops: &str) -> String {
        panic!("a refused or failed boot must not plan a request")
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<RequestIdentity>,
    ) -> Result<String, String> {
        Err("dispatch must not be reached: boot never serves in this test".to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        "test"
    }

    fn config_id(&self) -> &str {
        "test-config"
    }
}

/// Snapshot + restore guard for the ambient process state these tests mutate
/// (`HOME`, `KHIVE_SOCKET`, `KHIVE_PID`). Restoring via `Drop` keeps the host
/// process clean even if an assertion panics partway through.
struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_socket: Option<std::ffi::OsString>,
    prev_pid: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn capture() -> Self {
        Self {
            prev_home: std::env::var_os("HOME"),
            prev_socket: std::env::var_os("KHIVE_SOCKET"),
            prev_pid: std::env::var_os("KHIVE_PID"),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in [
            ("HOME", &self.prev_home),
            ("KHIVE_SOCKET", &self.prev_socket),
            ("KHIVE_PID", &self.prev_pid),
        ] {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A `HOME` that is a regular file, so no socket directory under it can ever
/// be created. Every case below roots its socket here: a boot that reaches
/// socket-directory setup then fails there instead of binding and serving
/// forever, which is what the two refusal cases would otherwise do if the
/// pairing check were removed.
fn unusable_home(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let home = dir.path().join("home-is-a-regular-file");
    std::fs::write(&home, b"regular file: create_dir_all under it must fail")
        .expect("write blocking file");
    home
}

/// Only `KHIVE_SOCKET` set: the refusal must name the variable that was set,
/// the one that was missing, and the incumbent's shared PID file and socket.
#[tokio::test]
#[serial]
async fn only_socket_override_refuses_and_names_the_set_and_missing_variables() {
    if test_process::run_in_child() {
        return;
    }

    let _env = EnvGuard::capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let home = unusable_home(&dir);
    let private_socket = home.join("private").join("khived.sock");

    std::env::set_var("HOME", &home);
    std::env::set_var("KHIVE_SOCKET", &private_socket);
    std::env::remove_var("KHIVE_PID");

    let err = run_daemon_with_boot_guard(NeverDispatch, None)
        .await
        .expect_err("a socket override without a pid override must refuse to boot");
    let message = format!("{err:#}");

    assert!(
        message.contains(PAIRING_REFUSAL_MARKER),
        "the refusal must be the pairing refusal, got: {message}"
    );
    assert!(
        message.contains("KHIVE_SOCKET") && message.contains("KHIVE_PID"),
        "the refusal must name both the variable that was set and the one that was \
         missing, got: {message}"
    );
    assert!(
        message.contains(&private_socket.display().to_string()),
        "the refusal must quote the private socket this boot was given, got: {message}"
    );
    assert!(
        message.contains(&home.join(".khive").join("khived.pid").display().to_string()),
        "the refusal must name the shared PID file this boot would have claimed, so an \
         operator can see whose file it is, got: {message}"
    );
    assert!(
        message.contains(
            &home
                .join(".khive")
                .join("khived.sock")
                .display()
                .to_string()
        ),
        "the refusal must name the socket the incumbent daemon is serving, not only the \
         socket being started, got: {message}"
    );
}

/// Only `KHIVE_PID` set: the symmetric refusal — a private PID file over the
/// shared socket is the same split rendezvous seen from the other side.
#[tokio::test]
#[serial]
async fn only_pid_override_refuses_and_names_the_set_and_missing_variables() {
    if test_process::run_in_child() {
        return;
    }

    let _env = EnvGuard::capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let home = unusable_home(&dir);
    let private_pid = dir.path().join("private").join("khived.pid");

    std::env::set_var("HOME", &home);
    std::env::set_var("KHIVE_PID", &private_pid);
    std::env::remove_var("KHIVE_SOCKET");

    let err = run_daemon_with_boot_guard(NeverDispatch, None)
        .await
        .expect_err("a pid override without a socket override must refuse to boot");
    let message = format!("{err:#}");

    assert!(
        message.contains(PAIRING_REFUSAL_MARKER),
        "the refusal must be the pairing refusal, got: {message}"
    );
    assert!(
        message.contains("KHIVE_PID") && message.contains("KHIVE_SOCKET"),
        "the refusal must name both the variable that was set and the one that was \
         missing, got: {message}"
    );
    assert!(
        message.contains(&private_pid.display().to_string()),
        "the refusal must quote the private PID file this boot was given, got: {message}"
    );
    assert!(
        message.contains(
            &home
                .join(".khive")
                .join("khived.sock")
                .display()
                .to_string()
        ),
        "the refusal must name the shared socket this boot would have bound, which is the \
         one the incumbent daemon is serving, got: {message}"
    );
    assert!(
        message.contains(&home.join(".khive").join("khived.pid").display().to_string()),
        "the refusal must name the PID file recording the incumbent, got: {message}"
    );
}

/// Both set: boot proceeds past the pairing check (it fails later, in socket
/// directory setup) and both paths come from the variables.
#[tokio::test]
#[serial]
async fn both_overrides_set_boot_past_the_pairing_check_and_resolve_from_the_variables() {
    if test_process::run_in_child() {
        return;
    }

    let _env = EnvGuard::capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let home = unusable_home(&dir);
    let private_socket = home.join("private").join("khived.sock");
    let private_pid = dir.path().join("private.pid");

    std::env::set_var("HOME", &home);
    std::env::set_var("KHIVE_SOCKET", &private_socket);
    std::env::set_var("KHIVE_PID", &private_pid);

    assert_eq!(
        socket_path(),
        private_socket,
        "with both overrides set the socket must resolve from KHIVE_SOCKET"
    );
    assert_eq!(
        pid_path(),
        private_pid,
        "with both overrides set the PID file must resolve from KHIVE_PID"
    );

    let err = run_daemon_with_boot_guard(NeverDispatch, None)
        .await
        .expect_err("this boot still fails: its socket directory cannot be created");
    let message = format!("{err:#}");
    assert!(
        !message.contains(PAIRING_REFUSAL_MARKER),
        "a fully overridden rendezvous must pass the pairing check and fail later, got: \
         {message}"
    );
}

/// Neither set: boot proceeds past the pairing check and both paths come from
/// the defaults under the khive directory.
#[tokio::test]
#[serial]
async fn neither_override_set_boots_past_the_pairing_check_and_resolves_both_defaults() {
    if test_process::run_in_child() {
        return;
    }

    let _env = EnvGuard::capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let home = unusable_home(&dir);

    std::env::set_var("HOME", &home);
    std::env::remove_var("KHIVE_SOCKET");
    std::env::remove_var("KHIVE_PID");

    assert_eq!(
        socket_path(),
        home.join(".khive").join("khived.sock"),
        "with no override the socket must resolve to the default under the khive directory"
    );
    assert_eq!(
        pid_path(),
        home.join(".khive").join("khived.pid"),
        "with no override the PID file must resolve to the default under the khive directory"
    );

    let err = run_daemon_with_boot_guard(NeverDispatch, None)
        .await
        .expect_err("this boot still fails: its socket directory cannot be created");
    let message = format!("{err:#}");
    assert!(
        !message.contains(PAIRING_REFUSAL_MARKER),
        "the default rendezvous must pass the pairing check and fail later, got: {message}"
    );
}

#[tokio::test]
#[serial]
async fn group_writable_pid_parent_refuses_daemon_startup() {
    if run_in_pid_trust_child() {
        return;
    }

    let root = std::path::PathBuf::from(std::env::var_os(PID_TRUST_ROOT).expect("fixture root"));
    let pid_file = configure_pid_trust_paths(&root, 0o770);
    // Bounded: with the refusal gone the daemon would start and serve, and an
    // unbounded await would hang the test instead of failing it.
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_daemon_with_boot_guard(NeverDispatch, None),
    )
    .await
    .expect("a group-writable PID parent must refuse startup, not start serving")
    .expect_err("a group-writable PID parent must refuse startup");
    let message = format!("{error:#}");

    assert!(
        message.contains("KHIVE_PID"),
        "wrong variable in refusal: {message}"
    );
    assert!(
        message.contains("PID-file directory") && message.contains("writable by group or other"),
        "refusal must identify the unsafe PID-file directory: {message}"
    );
    assert!(
        message.contains(&pid_file.parent().unwrap().display().to_string()),
        "refusal must identify the configured PID parent: {message}"
    );
}

#[tokio::test]
#[serial]
async fn other_writable_pid_parent_refuses_daemon_startup() {
    if run_in_pid_trust_child() {
        return;
    }

    let root = std::path::PathBuf::from(std::env::var_os(PID_TRUST_ROOT).expect("fixture root"));
    let pid_file = configure_pid_trust_paths(&root, 0o707);
    // Bounded: with the refusal gone the daemon would start and serve, and an
    // unbounded await would hang the test instead of failing it.
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_daemon_with_boot_guard(NeverDispatch, None),
    )
    .await
    .expect("an other-writable PID parent must refuse startup, not start serving")
    .expect_err("an other-writable PID parent must refuse startup");
    let message = format!("{error:#}");

    assert!(
        message.contains("KHIVE_PID"),
        "wrong variable in refusal: {message}"
    );
    assert!(
        message.contains("PID-file directory") && message.contains("writable by group or other"),
        "refusal must identify the unsafe PID-file directory: {message}"
    );
    assert!(
        message.contains(&pid_file.parent().unwrap().display().to_string()),
        "refusal must identify the configured PID parent: {message}"
    );
}

#[tokio::test]
#[serial]
async fn trusted_private_socket_and_pid_parents_allow_daemon_startup() {
    if run_in_pid_trust_child() {
        return;
    }

    let root = std::path::PathBuf::from(std::env::var_os(PID_TRUST_ROOT).expect("fixture root"));
    let pid_file = configure_pid_trust_paths(&root, 0o700);
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_started = std::sync::Arc::clone(&started);
    let _sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install child SIGTERM handler");
    let mut daemon = tokio::spawn(run_daemon_with_boot_guard_and_start(
        NeverDispatch,
        None,
        move |_| {
            assert!(
                socket_path().exists(),
                "trusted socket parent must allow bind"
            );
            assert_eq!(
                std::fs::read_to_string(&pid_file).expect("claimed PID file"),
                std::process::id().to_string()
            );
            callback_started.store(true, std::sync::atomic::Ordering::SeqCst);
        },
    ));

    let sock = socket_path();
    let mut stream = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.is_finished() {
                (&mut daemon)
                    .await
                    .expect("trusted daemon task must not panic")
                    .expect("trusted private paths must start");
                panic!("trusted daemon exited before readiness");
            }
            if let Ok(stream) = tokio::net::UnixStream::connect(&sock).await {
                break stream;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon must bind");
    let request = DaemonRequestFrame {
        namespace: "test".to_string(),
        config_id: "test-config".to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: true,
        ..Default::default()
    };
    let payload = serde_json::to_vec(&request).expect("encode readiness probe");
    write_frame(&mut stream, &payload)
        .await
        .expect("write readiness probe");
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), read_frame(&mut stream))
        .await
        .expect("daemon must serve readiness probe")
        .expect("read readiness response");
    let response: DaemonResponseFrame =
        serde_json::from_slice(&response).expect("decode readiness response");
    assert!(response.ok, "daemon readiness failed: {response:?}");
    assert_eq!(response.served_config_id.as_deref(), Some("test-config"));
    assert!(response.result.is_none());
    assert!(response.error.is_none());
    assert!(response.metrics.is_none());
    drop(stream);

    assert!(started.load(std::sync::atomic::Ordering::SeqCst));

    // SAFETY: this test runs in an isolated child and has installed its SIGTERM handler.
    let rc = unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "signal isolated daemon child");
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("trusted daemon must shut down after SIGTERM")
        .expect("trusted daemon task must not panic")
        .expect("trusted private paths must start");
}
