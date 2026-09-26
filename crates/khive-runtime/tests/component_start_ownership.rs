#![cfg(unix)]

use async_trait::async_trait;
use khive_runtime::daemon::{
    daemon_shutdown_token, pid_path, run_daemon_with_boot_guard_and_start, socket_path,
};
use khive_runtime::{DaemonDispatch, RequestIdentity};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct NeverDispatch;

#[async_trait]
impl DaemonDispatch for NeverDispatch {
    fn plan(&self, _ops: &str) -> String {
        panic!("failed setup must not serve")
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
        panic!("failed setup must not serve")
    }
    async fn warm_all(&self) {}
    fn namespace(&self) -> &str {
        "local"
    }
    fn config_id(&self) -> &str {
        "component-start-ownership"
    }
}

#[test]
fn failed_establishment_never_starts_components_and_always_cancels() {
    for scenario in [
        "unpaired",
        "untrusted",
        "bind",
        "pid",
        "incumbent",
        "startup-panic",
    ] {
        let dir = tempfile::Builder::new()
            .prefix("kh-start-")
            .tempdir_in("/tmp")
            .unwrap();
        let child_home = dir.path().join("home");
        std::fs::create_dir(&child_home).expect("empty component child HOME");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "component_start_ownership_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .envs(
                std::env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("KHIVE_")),
            )
            .env("HOME", &child_home)
            .env_remove("LATTICE_MODEL_CACHE")
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_COMPONENT_START_CASE", scenario)
            .env("KHIVE_SOCKET", dir.path().join("s"))
            .env("KHIVE_PID", dir.path().join("p"))
            .env("KHIVE_LOCK", dir.path().join("l"))
            .env("KHIVE_RECOVERER_LOCK", dir.path().join("r"))
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let completed = loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    break false;
                }
            }
        };
        let output = child.wait_with_output().unwrap();
        assert!(
            completed && output.status.success(),
            "{scenario}: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("COMPONENT_START_OWNERSHIP_VERIFIED"),
            "{scenario}: {output:?}"
        );
        assert!(
            std::fs::read_dir(child_home).unwrap().next().is_none(),
            "component-start child must leave its private HOME empty: {scenario}"
        );
    }
}

struct Incumbent(std::process::Child);
impl Drop for Incumbent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "isolated child of failed_establishment_never_starts_components_and_always_cancels"]
async fn component_start_ownership_child() {
    assert!(!daemon_shutdown_token().is_cancelled());
    let scenario = std::env::var("KHIVE_COMPONENT_START_CASE").unwrap();
    let sock = socket_path();
    let mut incumbent = None;
    match scenario.as_str() {
        "unpaired" => std::env::remove_var("KHIVE_PID"),
        "untrusted" => std::fs::set_permissions(
            sock.parent().unwrap(),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap(),
        "bind" => std::fs::create_dir(&sock).unwrap(),
        "pid" => std::env::set_var("KHIVE_PID", sock.parent().unwrap().join("absent/p")),
        "incumbent" => {
            let child = Incumbent(Command::new("sleep").arg("30").spawn().unwrap());
            std::fs::write(pid_path(), child.0.id().to_string()).unwrap();
            // A live PID alone is a reused PID and is reclaimed; an incumbent
            // daemon also holds its PID file's lock, so this one holds it too.
            let lock = std::fs::File::open(pid_path()).unwrap();
            lock.lock().unwrap();
            incumbent = Some((child, lock));
        }
        "startup-panic" => {}
        _ => panic!("unknown scenario"),
    }
    let starts = Arc::new(AtomicUsize::new(0));
    let callback_starts = starts.clone();
    let panic_at_start = scenario == "startup-panic";
    let result = tokio::spawn(run_daemon_with_boot_guard_and_start(
        NeverDispatch,
        None,
        move |_| {
            callback_starts.fetch_add(1, Ordering::SeqCst);
            assert!(std::fs::metadata(socket_path())
                .unwrap()
                .file_type()
                .is_socket());
            assert_eq!(
                std::fs::read_to_string(pid_path()).unwrap(),
                std::process::id().to_string()
            );
            assert!(!panic_at_start, "injected startup panic after ownership");
        },
    ))
    .await;
    if panic_at_start {
        assert!(result.unwrap_err().is_panic());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    } else {
        let error = result.unwrap().expect_err("establishment must fail");
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        match scenario.as_str() {
            "unpaired" => assert!(error
                .to_string()
                .contains("two halves of one daemon rendezvous")),
            "untrusted" => assert!(error.to_string().contains("writable by")),
            "bind" => assert!(sock.is_dir()),
            "pid" => assert!(
                std::fs::symlink_metadata(&sock).is_err(),
                "the PID claim precedes socket bind, so a failed claim must leave no socket"
            ),
            "incumbent" => assert!(error.to_string().contains("owns the daemon PID file")),
            _ => unreachable!(),
        }
    }
    assert!(daemon_shutdown_token().is_cancelled());
    drop(incumbent);
    println!("COMPONENT_START_OWNERSHIP_VERIFIED");
}
