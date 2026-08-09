//! Shared daemon recovery and framing fixtures.
//!
//! The daemon tests mutate process-global socket/PID environment variables and
//! exercise real Unix framing. Keeping their counters, cleanup guard, frame
//! exchange helpers, and in-process launcher together gives recovery tests one
//! fail-closed teardown path instead of open-coded variants in `daemon.rs`.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::daemon::{
    run_daemon_in_process_test, write_frame, DaemonDispatch, DaemonRequestFrame,
    DaemonResponseFrame,
};
use tokio::net::UnixStream;

pub(super) static KILL_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(super) static SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(super) static FORCE_PID_IS_DAEMON: AtomicBool = AtomicBool::new(false);
pub(super) static FORCE_PID_IS_FOREIGN: AtomicBool = AtomicBool::new(false);
pub(super) static FORCED_CONNECT_ERROR: AtomicI32 = AtomicI32::new(0);
pub(super) static DAEMON_DISPATCH: AtomicUsize = AtomicUsize::new(0);

/// Rendezvous after every recoverer independently observes an absent daemon.
pub(super) static RECOVERY_RACE_BARRIER: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>> =
    std::sync::Mutex::new(None);

pub(super) fn reset_counters() {
    KILL_COUNT.store(0, Ordering::SeqCst);
    SPAWN_COUNT.store(0, Ordering::SeqCst);
    FORCE_PID_IS_DAEMON.store(false, Ordering::SeqCst);
    FORCE_PID_IS_FOREIGN.store(false, Ordering::SeqCst);
    FORCED_CONNECT_ERROR.store(0, Ordering::SeqCst);
    DAEMON_DISPATCH.store(0, Ordering::SeqCst);
    *RECOVERY_RACE_BARRIER
        .lock()
        .expect("barrier mutex poisoned") = None;
}

pub(super) fn clear_daemon_env() {
    std::env::remove_var("KHIVE_SOCKET");
    std::env::remove_var("KHIVE_PID");
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::remove_var("KHIVE_LOCK");
    std::env::remove_var("KHIVE_RECOVERER_LOCK");
    std::env::remove_var("KHIVE_PROCESS_REF");
}

pub(super) struct RecoveryTestGuard {
    child: Option<std::process::Child>,
}

impl RecoveryTestGuard {
    pub(super) fn new() -> Self {
        Self { child: None }
    }

    pub(super) fn track_child(&mut self, child: std::process::Child) -> u32 {
        let pid = child.id();
        self.child = Some(child);
        pid
    }

    pub(super) fn child_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().expect("test child must be tracked")
    }

    pub(super) fn kill_and_reap_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for RecoveryTestGuard {
    fn drop(&mut self) {
        self.kill_and_reap_child();
        reset_counters();
        clear_daemon_env();
    }
}

pub(super) async fn connect_when_ready(sock: &std::path::Path) -> UnixStream {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(stream) = UnixStream::connect(sock).await {
            return stream;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon never bound {sock:?} within 5s"
        );
        // Sleep, not yield_now(): the daemon binds on its own task, and a
        // millisecond-scale sleep keeps this poll from hot-spinning a worker.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

pub(super) async fn exchange(
    sock: &std::path::Path,
    frame: &DaemonRequestFrame,
) -> DaemonResponseFrame {
    let mut stream = UnixStream::connect(sock)
        .await
        .expect("connect to daemon socket");
    let payload = serde_json::to_vec(frame).expect("serialize request frame");
    write_frame(&mut stream, &payload)
        .await
        .expect("write request frame");
    let response = khive_runtime::daemon::read_frame(&mut stream)
        .await
        .expect("read response frame");
    serde_json::from_slice(&response).expect("decode response frame")
}

/// Small real dispatcher used by the in-process daemon launcher. Probe-only
/// frames bypass `dispatch`, so `dispatch_count` is also the test's exact
/// non-probe exchange oracle.
#[derive(Clone)]
pub(super) struct HarnessDispatch {
    namespace: Arc<str>,
    config_id: Arc<str>,
    dispatch_count: Arc<AtomicUsize>,
}

impl HarnessDispatch {
    pub(super) fn new(namespace: &str, config_id: &str) -> Self {
        Self {
            namespace: Arc::from(namespace),
            config_id: Arc::from(config_id),
            dispatch_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(super) fn dispatch_count(&self) -> usize {
        self.dispatch_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DaemonDispatch for HarnessDispatch {
    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        Ok(r#"{"entities":0,"edges":0,"notes":0}"#.to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn config_id(&self) -> &str {
        &self.config_id
    }
}

#[derive(Default)]
struct LauncherState {
    launched: AtomicUsize,
    running: AtomicUsize,
}

struct RunningTaskGuard(Arc<LauncherState>);

impl Drop for RunningTaskGuard {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Test launcher that exercises the real daemon server without forking the
/// test binary. Tests using it must run on a multi-thread Tokio runtime: every
/// launched daemon performs a blocking `flock` during boot. Losing in-process
/// candidates take the real socket/PID ownership exit, which also cancels the
/// runtime's process-wide component token; use only the component-free
/// [`HarnessDispatch`] fixture.
#[derive(Clone)]
pub(super) struct InProcessDaemonLauncher<D> {
    dispatcher: D,
    state: Arc<LauncherState>,
}

impl<D> InProcessDaemonLauncher<D> {
    pub(super) fn new(dispatcher: D) -> Self {
        Self {
            dispatcher,
            state: Arc::new(LauncherState::default()),
        }
    }

    pub(super) fn launched_count(&self) -> usize {
        self.state.launched.load(Ordering::SeqCst)
    }

    pub(super) fn running_count(&self) -> usize {
        self.state.running.load(Ordering::SeqCst)
    }

    pub(super) async fn wait_for_running_count(&self, expected: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.running_count() != expected {
            assert!(
                tokio::time::Instant::now() < deadline,
                "in-process daemon count did not converge to {expected}; launched={} running={}",
                self.launched_count(),
                self.running_count()
            );
            // Sleep, not yield_now(): launched daemons progress on other
            // workers (blocking flock boot); polling at millisecond scale
            // avoids a hot spin.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

pub(super) struct InProcessDaemonHandle {
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl std::fmt::Debug for InProcessDaemonHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessDaemonHandle")
            .field(
                "finished",
                &self
                    .task
                    .as_ref()
                    .map(tokio::task::JoinHandle::is_finished)
                    .unwrap_or(true),
            )
            .finish()
    }
}

impl InProcessDaemonHandle {
    pub(super) fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .map(tokio::task::JoinHandle::is_finished)
            .unwrap_or(true)
    }

    pub(super) async fn stop(mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        let aborted = !task.is_finished();
        if aborted {
            // Abort IS the teardown contract for a serving in-process daemon:
            // it otherwise serves until SIGTERM, which tests have no channel
            // to deliver. A still-running task is cancelled here on purpose.
            task.abort();
        }
        match task.await {
            Ok(result) => result.expect("in-process daemon task must exit without error"),
            Err(join_error) if aborted && join_error.is_cancelled() => {
                // The expected outcome of this stop()'s own abort.
            }
            Err(join_error) if join_error.is_panic() => {
                // Never swallow a panic: resume it so the failure surfaces
                // with its original payload and backtrace. A losing candidate
                // exits Ok(()) through the ownership fence, so a panic here
                // is a real regression in the server path.
                std::panic::resume_unwind(join_error.into_panic());
            }
            Err(join_error) => {
                panic!("in-process daemon task failed: {join_error:?}");
            }
        }
    }
}

impl Drop for InProcessDaemonHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl<D> super::DaemonLauncher for InProcessDaemonLauncher<D>
where
    D: DaemonDispatch,
{
    type Handle = InProcessDaemonHandle;

    fn launch(&self) -> std::io::Result<Self::Handle> {
        self.state.launched.fetch_add(1, Ordering::SeqCst);
        self.state.running.fetch_add(1, Ordering::SeqCst);
        let dispatcher = self.dispatcher.clone();
        let state = Arc::clone(&self.state);
        let task = tokio::spawn(async move {
            let _running = RunningTaskGuard(state);
            run_daemon_in_process_test(dispatcher).await
        });
        Ok(InProcessDaemonHandle { task: Some(task) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_process_daemon_handle_drop_aborts_task() {
        let started = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&started);
        let task_completed = Arc::clone(&completed);
        let task = tokio::spawn(async move {
            task_started.store(true, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            task_completed.store(true, Ordering::SeqCst);
            Ok(())
        });
        let handle = InProcessDaemonHandle { task: Some(task) };

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop regression fixture task must start");

        drop(handle);
        for _ in 0..20 {
            assert!(
                !completed.load(Ordering::SeqCst),
                "dropping an in-process daemon handle must abort its task"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
