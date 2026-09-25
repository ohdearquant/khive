//! Regression test: a daemon-setup failure BEFORE socket bind must still
//! cancel the process-wide component shutdown token (ADR-119).
//!
//! `run_daemon_with_boot_guard` performs fallible setup (socket-directory
//! creation and trust validation) before it binds. Host components now start
//! only after ownership succeeds, but a failed candidate must still cancel
//! its process-wide token. The guard therefore precedes all fallible setup.
//!
//! The fixture runs as the sole test in a child with private daemon paths.
//! Its changes to `KHIVE_SOCKET`, `KHIVE_PID`, and the process-wide single-shot
//! shutdown token cannot affect another test.

#![cfg(unix)]

#[path = "../src/test_process.rs"]
mod test_process;

use async_trait::async_trait;
use khive_runtime::daemon::run_daemon_with_boot_guard_and_start;
use khive_runtime::{DaemonDispatch, RequestIdentity};

#[derive(Clone)]
struct NeverDispatch;

#[async_trait]
impl DaemonDispatch for NeverDispatch {
    fn plan(&self, _ops: &str) -> String {
        panic!("setup failure must not plan a request")
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
        Err("dispatch must not be reached: setup fails before bind".to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        "test"
    }

    fn config_id(&self) -> &str {
        "test-config"
    }
}

#[tokio::test]
async fn setup_failure_before_bind_cancels_component_token() {
    if test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"regular file blocking create_dir_all").expect("write blocker");

    // The socket's parent is a regular file, so create_dir_all fails before
    // the stale-daemon cleanup, bind, or pid-write are ever reached.
    //
    // Both rendezvous overrides are set because setting exactly one of them
    // is refused before any of that setup work runs (#2656), and this test
    // would then pass on a refusal it does not target.
    std::env::set_var("KHIVE_SOCKET", blocker.join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));

    let started = std::sync::atomic::AtomicBool::new(false);
    let result = run_daemon_with_boot_guard_and_start(NeverDispatch, None, |_| {
        started.store(true, std::sync::atomic::Ordering::SeqCst);
    })
    .await;
    assert!(
        !started.load(std::sync::atomic::Ordering::SeqCst),
        "a setup failure must not start host components"
    );

    assert!(
        result.is_err(),
        "setup against a file-as-directory must fail"
    );
    assert!(
        khive_runtime::daemon_shutdown_token().is_cancelled(),
        "an error before bind must still cancel the component shutdown token"
    );
}
