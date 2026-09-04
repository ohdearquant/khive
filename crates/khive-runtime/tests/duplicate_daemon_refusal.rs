//! Regression test for #1874 — a second `khived` boot attempt against a
//! store already served by a live daemon must refuse loudly (an `Err` naming
//! the incumbent pid) rather than silently exit `Ok(())`.
//!
//! Before the fix, `run_daemon_with_boot_guard_inner` treated "a live,
//! responsive incumbent already owns this socket" as ordinary success: it
//! logged at `info` and returned `Ok(())`. A human (or a supervisor)
//! starting a second daemon against a store already served by one had no
//! signal that anything was wrong — both processes ran, each holding its own
//! WAL connection and read marks, exactly the two-daemon state #1874
//! describes.
//!
//! Unix-only: daemon boot/socket/pid-file machinery is `#[cfg(unix)]` only.

#![cfg(unix)]

use async_trait::async_trait;
use khive_runtime::daemon::run_daemon_in_process_test;
use khive_runtime::{DaemonDispatch, RequestIdentity};
use serial_test::serial;

#[derive(Clone)]
struct NeverDispatch;

#[async_trait]
impl DaemonDispatch for NeverDispatch {
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
        Err("dispatch not exercised by this test".to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        "test"
    }

    fn config_id(&self) -> &str {
        "test-config"
    }
}

/// Second boot attempt must fail loudly (`Err`) and name the incumbent's pid
/// — never silently succeed while a live daemon already owns the socket.
#[tokio::test]
#[serial]
async fn second_daemon_boot_refuses_loudly_while_first_is_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

    let first = tokio::spawn(run_daemon_in_process_test(NeverDispatch));

    // Poll for the socket to appear rather than a fixed sleep: bind happens
    // asynchronously inside the spawned task.
    let sock = dir.path().join("khived.sock");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !sock.exists() {
        if tokio::time::Instant::now() >= deadline {
            panic!("first daemon never bound its socket within the deadline");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let second = run_daemon_in_process_test(NeverDispatch).await;

    let err = second.expect_err(
        "a second boot attempt while the first is live and responsive must return Err, \
         not silently succeed",
    );
    let message = format!("{err:#}");
    assert!(
        message.contains(&std::process::id().to_string()),
        "the refusal must name the incumbent's pid so an operator can act on it, got: {message}"
    );
    assert!(
        message.to_lowercase().contains("already running"),
        "the refusal must say plainly that a daemon is already running, got: {message}"
    );

    // The in-process harness serves until aborted (no SIGTERM channel in a
    // test process) — the same teardown contract used elsewhere for this
    // entrypoint (see khive-mcp's `InProcessDaemonHandle::stop`).
    first.abort();
    let _ = first.await;

    std::env::remove_var("KHIVE_SOCKET");
    std::env::remove_var("KHIVE_PID");
    std::env::remove_var("KHIVE_LOCK");
}
