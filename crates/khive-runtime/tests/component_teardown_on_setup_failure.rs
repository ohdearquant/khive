//! Regression test: a daemon-setup failure BEFORE socket bind must still
//! cancel the process-wide component shutdown token (ADR-119).
//!
//! `run_daemon_with_boot_guard` performs fallible setup (socket-directory
//! creation and trust validation) before it binds. Components may already be
//! running by then — the serve path starts them first — so the teardown guard
//! has to be constructed before ANY fallible startup work, or an early error
//! return leaves supervisors holding a live token in an embedded process.
//!
//! Isolation: this test lives in its own integration-test binary on purpose.
//! It mutates `KHIVE_SOCKET` (process-global env) and fires the process-wide
//! single-shot shutdown token; neither may leak into other tests.

#![cfg(unix)]

use async_trait::async_trait;
use khive_runtime::daemon::run_daemon_with_boot_guard;
use khive_runtime::{DaemonDispatch, RequestIdentity};

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
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"regular file blocking create_dir_all").expect("write blocker");

    // The socket's parent is a regular file, so create_dir_all fails before
    // the stale-daemon cleanup, bind, or pid-write are ever reached.
    std::env::set_var("KHIVE_SOCKET", blocker.join("khived.sock"));

    let result = run_daemon_with_boot_guard(NeverDispatch, None).await;

    assert!(
        result.is_err(),
        "setup against a file-as-directory must fail"
    );
    assert!(
        khive_runtime::daemon_shutdown_token().is_cancelled(),
        "an error before bind must still cancel the component shutdown token"
    );
}
