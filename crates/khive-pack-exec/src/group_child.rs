use std::future::{poll_fn, Future};
use std::io;
use std::process::ExitStatus;
use std::task::{Context, Poll};

use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::signal::unix::{signal, Signal, SignalKind};

const OBSERVE_POLL_BUDGET: usize = 64;

trait GroupBackend {
    fn poll_observe_unreaped(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
    fn signal_group(&mut self) -> io::Result<()>;
    fn start_kill_leader(&mut self) -> io::Result<()>;
    fn poll_reap(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupState {
    Held,
    GroupSignaled,
    Reaped,
    Lost,
}

struct GroupLifecycle<B: GroupBackend> {
    backend: B,
    state: GroupState,
    observed_exited: bool,
    exit_status: Option<ExitStatus>,
    cleanup_error: Option<io::Error>,
    reap_error: Option<io::Error>,
}

impl<B: GroupBackend> GroupLifecycle<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            state: GroupState::Held,
            observed_exited: false,
            exit_status: None,
            cleanup_error: None,
            reap_error: None,
        }
    }

    async fn wait_unreaped(&mut self) -> io::Result<()> {
        if self.state != GroupState::Held {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        poll_fn(|cx| {
            for _ in 0..OBSERVE_POLL_BUDGET {
                match self.backend.poll_observe_unreaped(cx) {
                    Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {
                        continue
                    }
                    Poll::Ready(Err(error)) => {
                        if error.raw_os_error() == Some(libc::ECHILD) {
                            self.state = GroupState::Lost;
                            self.observed_exited = false;
                        }
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(())) => {
                        self.observed_exited = true;
                        return Poll::Ready(Ok(()));
                    }
                    result => return result,
                }
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await
    }

    async fn finish(&mut self) -> io::Result<ExitStatus> {
        if self.exit_status.is_some() || self.reap_error.is_some() {
            return self.completed_result();
        }
        if self.state == GroupState::Lost {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        if self.state == GroupState::Held {
            match self.backend.signal_group() {
                Ok(()) => {}
                Err(error) if self.benign_group_error(&error) => {}
                Err(error) => {
                    let cleanup_error = self.cleanup_error.get_or_insert(error);
                    if !self.observed_exited {
                        if let Err(leader_error) = self.backend.start_kill_leader() {
                            // Neither signal was accepted. Waiting on a live
                            // leader here could block forever; Drop can retry
                            // only while the original identity is still held.
                            return Err(io::Error::new(
                                cleanup_error.kind(),
                                format!(
                                    "group signal failed: {cleanup_error}; direct child signal failed: {leader_error}"
                                ),
                            ));
                        }
                    }
                }
            }
            // Reaping releases the numeric identity, even when this future is
            // cancelled after its first poll. Never re-arm after entering wait.
            // An accepted leader signal does not certify descendant cleanup;
            // the original group failure remains separate from exit status.
            self.state = GroupState::GroupSignaled;
        }
        match poll_fn(|cx| self.backend.poll_reap(cx)).await {
            Ok(status) => {
                self.state = GroupState::Reaped;
                self.exit_status = Some(status);
            }
            Err(error) => {
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.state = GroupState::Lost;
                    self.observed_exited = false;
                }
                self.reap_error = Some(error);
            }
        }
        self.completed_result()
    }

    fn benign_group_error(&self, error: &io::Error) -> bool {
        error.raw_os_error() == Some(libc::ESRCH)
            // macOS can report EPERM when only the exited, unreaped leader
            // remains. An errno alone never proves that a live leader exited.
            || (self.observed_exited && error.raw_os_error() == Some(libc::EPERM))
    }

    fn completed_result(&self) -> io::Result<ExitStatus> {
        match (&self.cleanup_error, &self.reap_error) {
            (Some(cleanup), Some(reap)) => Err(io::Error::new(
                reap.kind(),
                format!("group signal failed: {cleanup}; child reap failed: {reap}"),
            )),
            (Some(error), None) | (None, Some(error)) => Err(copy_io_error(error)),
            (None, None) => self
                .exit_status
                .ok_or_else(|| io::Error::other("child has no reaped exit status")),
        }
    }
}

fn copy_io_error(error: &io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(error.kind(), error.to_string()),
    }
}

impl<B: GroupBackend> Drop for GroupLifecycle<B> {
    fn drop(&mut self) {
        if self.state == GroupState::Held {
            // This runs before the backend's Child drop can reap the leader.
            if let Err(error) = self.backend.signal_group() {
                if !self.benign_group_error(&error) {
                    let _ = self.backend.start_kill_leader();
                }
            }
        }
    }
}

struct OsBackend {
    child: Child,
    signal: Signal,
    pgid: libc::pid_t,
}

impl GroupBackend for OsBackend {
    fn poll_observe_unreaped(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let pgid = self.pgid;
        poll_unreaped(cx, |cx| self.signal.poll_recv(cx), || probe_unreaped(pgid))
    }

    fn signal_group(&mut self) -> io::Result<()> {
        khive_runtime::process_group::signal_process_group(self.pgid, libc::SIGKILL)
    }

    fn start_kill_leader(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }

    fn poll_reap(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        std::pin::pin!(self.child.wait()).poll(cx)
    }
}

fn poll_unreaped(
    cx: &mut Context<'_>,
    mut poll_signal: impl FnMut(&mut Context<'_>) -> Poll<Option<()>>,
    mut probe: impl FnMut() -> io::Result<bool>,
) -> Poll<io::Result<()>> {
    for _ in 0..OBSERVE_POLL_BUDGET {
        // Register before probing: exit can happen between either operation.
        let notification = poll_signal(cx);
        match probe() {
            Ok(true) => return Poll::Ready(Ok(())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Poll::Ready(Err(error)),
            Ok(false) => {}
        }
        match notification {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(())) => continue,
            Poll::Ready(None) => {
                return Poll::Ready(Err(io::Error::other("child signal stream closed")));
            }
        }
    }
    cx.waker().wake_by_ref();
    Poll::Pending
}

fn probe_unreaped(pid: libc::pid_t) -> io::Result<bool> {
    // WNOHANG may leave siginfo untouched; zero is the no-exit sentinel.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let observed_pid = unsafe { info.si_pid() };
    if observed_pid == 0 {
        Ok(false)
    } else if observed_pid == pid {
        Ok(true)
    } else {
        Err(io::Error::other(
            "child exit observation returned another pid",
        ))
    }
}

// The caller establishes a fresh session in pre_exec. This owner must be the
// only reaper: process-wide wait/SA_NOCLDWAIT would invalidate the PID anchor.
pub(crate) struct GroupChild {
    lifecycle: GroupLifecycle<OsBackend>,
}

impl GroupChild {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        let signal = signal(SignalKind::child())?;
        let child = command.kill_on_drop(false).spawn()?;
        let pgid = child.id().expect("freshly spawned child has an id") as libc::pid_t;
        Ok(Self {
            lifecycle: GroupLifecycle::new(OsBackend {
                child,
                signal,
                pgid,
            }),
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.lifecycle.backend.pgid as u32
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.lifecycle.backend.child.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.lifecycle.backend.child.stderr.take()
    }

    pub(crate) async fn wait_unreaped(&mut self) -> io::Result<()> {
        self.lifecycle.wait_unreaped().await
    }

    pub(crate) async fn finish(&mut self) -> io::Result<ExitStatus> {
        self.lifecycle.finish().await
    }

    pub(crate) fn exit_status(&self) -> Option<ExitStatus> {
        self.lifecycle.exit_status
    }
}

#[cfg(test)]
#[path = "group_child_tests.rs"]
mod tests;
