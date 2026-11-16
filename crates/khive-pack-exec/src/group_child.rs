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
    exit_status: Option<ExitStatus>,
}

impl<B: GroupBackend> GroupLifecycle<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            state: GroupState::Held,
            exit_status: None,
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
                        }
                        return Poll::Ready(Err(error));
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
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        if self.state == GroupState::Lost {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        if self.state == GroupState::Held {
            match self.backend.signal_group() {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => return Err(error),
            }
            // Reaping releases the numeric identity, even when this future is
            // cancelled after its first poll. Never re-arm after entering wait.
            self.state = GroupState::GroupSignaled;
        }
        let status = poll_fn(|cx| self.backend.poll_reap(cx)).await?;
        self.state = GroupState::Reaped;
        self.exit_status = Some(status);
        Ok(status)
    }
}

impl<B: GroupBackend> Drop for GroupLifecycle<B> {
    fn drop(&mut self) {
        if self.state == GroupState::Held {
            // This runs before the backend's Child drop can reap the leader.
            let _ = self.backend.signal_group();
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
}

#[cfg(test)]
#[path = "group_child_tests.rs"]
mod tests;
