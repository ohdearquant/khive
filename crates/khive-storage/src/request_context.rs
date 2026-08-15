//! Backend-neutral request cancellation and deadline propagation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant as WallInstant};

use crate::{StorageError, StorageResult};

/// Default nonzero ceiling for request-owned read work, including compose.
pub const DEFAULT_REQUEST_READ_TIMEOUT_SECS: u64 = 30;

/// Resolve the operator-visible request-read ceiling. Invalid/zero values fail
/// closed to the documented nonzero default rather than disabling the guard.
pub fn request_read_timeout_from_env() -> Duration {
    let seconds = std::env::var("KHIVE_REQUEST_READ_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=3_600).contains(seconds))
        .unwrap_or(DEFAULT_REQUEST_READ_TIMEOUT_SECS);
    Duration::from_secs(seconds)
}

/// One absolute request deadline represented on async and blocking clocks.
///
/// Tokio's instant is authoritative for async timeout selection, including
/// paused-clock tests. The wall instant lets blocking backends enforce the
/// same non-renewing deadline without depending on Tokio's runtime clock.
#[derive(Clone, Copy, Debug)]
pub struct RequestReadDeadline {
    async_at: tokio::time::Instant,
    blocking_at: WallInstant,
}

impl RequestReadDeadline {
    /// Create a deadline `duration` from now.
    pub fn after(duration: Duration) -> Self {
        Self {
            async_at: tokio::time::Instant::now() + duration,
            blocking_at: WallInstant::now() + duration,
        }
    }

    /// Tokio-clock instant used by request coordinators and timeout selection.
    pub fn async_at(self) -> tokio::time::Instant {
        self.async_at
    }

    /// Wall-clock instant used by blocking backend cancellation checks.
    pub fn blocking_at(self) -> WallInstant {
        self.blocking_at
    }

    fn earlier(self, other: Self) -> Self {
        if self.async_at <= other.async_at {
            self
        } else {
            other
        }
    }
}

/// The first request-level condition that stopped read-only work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestReadStopReason {
    /// An explicit cancellation signal fired or its sender disappeared.
    Cancelled,
    /// The request's absolute deadline elapsed.
    Deadline,
}

/// Opaque snapshot of the current request read context.
///
/// Backend implementations capture this once before crossing into blocking
/// work so nested scopes and spawned children share one absolute deadline.
#[derive(Clone, Default)]
pub struct RequestReadContext {
    cancellations: Arc<[tokio::sync::watch::Receiver<bool>]>,
    deadline: Option<RequestReadDeadline>,
}

impl RequestReadContext {
    /// Return the request's absolute deadline, when one is installed.
    pub fn deadline(&self) -> Option<RequestReadDeadline> {
        self.deadline
    }

    /// Return the currently observed stop cause without waiting.
    pub fn stop_reason(&self) -> Option<RequestReadStopReason> {
        if self.cancellations.iter().any(receiver_cancelled) {
            Some(RequestReadStopReason::Cancelled)
        } else if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at)
        {
            Some(RequestReadStopReason::Deadline)
        } else {
            None
        }
    }

    /// Wait until one merged cancellation source fires or the deadline elapses.
    pub async fn wait_for_stop(self) -> RequestReadStopReason {
        let cancellation_receivers = self.cancellations;
        let request_deadline = self.deadline;
        let cancellation = async move {
            match cancellation_receivers.len() {
                0 => std::future::pending::<()>().await,
                1 => wait_for_receiver_cancellation(cancellation_receivers[0].clone()).await,
                2 => {
                    tokio::select! {
                        _ = wait_for_receiver_cancellation(cancellation_receivers[0].clone()) => {},
                        _ = wait_for_receiver_cancellation(cancellation_receivers[1].clone()) => {},
                    }
                }
                _ => wait_for_receiver_set(cancellation_receivers).await,
            }
        };
        let deadline = async move {
            match request_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.async_at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancellation => RequestReadStopReason::Cancelled,
            _ = deadline => RequestReadStopReason::Deadline,
        }
    }
}

tokio::task_local! {
    static REQUEST_READ_CONTEXT: RequestReadContext;
}

/// Capture the current request context for a backend operation.
pub fn capture_request_read_context() -> RequestReadContext {
    REQUEST_READ_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Constrain `candidate` by the current request's deadline without installing
/// a new scope.
pub fn effective_request_read_deadline(candidate: RequestReadDeadline) -> RequestReadDeadline {
    capture_request_read_context()
        .deadline
        .map_or(candidate, |existing| existing.earlier(candidate))
}

/// Scope `future` to an explicit read-cancellation signal.
///
/// Backends observe the signal only for work they classify as read-only.
pub async fn scope_request_read_cancellation<F>(
    cancellation: tokio::sync::watch::Receiver<bool>,
    future: F,
) -> F::Output
where
    F: Future,
{
    let mut context = capture_request_read_context();
    let mut cancellations = Vec::with_capacity(context.cancellations.len() + 1);
    cancellations.extend(context.cancellations.iter().cloned());
    cancellations.push(cancellation);
    context.cancellations = cancellations.into();
    REQUEST_READ_CONTEXT.scope(context, future).await
}

/// Scope `future` to one relative read deadline, preserving an earlier outer
/// deadline instead of renewing it.
pub async fn scope_request_read_deadline<F>(duration: Duration, future: F) -> F::Output
where
    F: Future,
{
    scope_request_read_deadline_at(RequestReadDeadline::after(duration), future).await
}

/// Scope `future` to an already-created absolute read deadline.
pub async fn scope_request_read_deadline_at<F>(
    deadline: RequestReadDeadline,
    future: F,
) -> F::Output
where
    F: Future,
{
    let mut context = capture_request_read_context();
    context.deadline = Some(match context.deadline {
        Some(existing) => existing.earlier(deadline),
        None => deadline,
    });
    REQUEST_READ_CONTEXT.scope(context, future).await
}

/// Capture the current request read context for a spawned child future.
pub fn inherit_request_read_context<F>(future: F) -> impl Future<Output = F::Output> + Send
where
    F: Future + Send,
{
    let inherited = REQUEST_READ_CONTEXT.try_with(Clone::clone).ok();
    async move {
        match inherited {
            Some(context) => REQUEST_READ_CONTEXT.scope(context, future).await,
            None => future.await,
        }
    }
}

/// Intention-revealing alias for cancellation-only child tasks.
pub fn inherit_request_read_cancellation<F>(future: F) -> impl Future<Output = F::Output> + Send
where
    F: Future + Send,
{
    inherit_request_read_context(future)
}

/// Return whether the current request has stopped its read-only work.
pub fn request_read_is_cancelled() -> bool {
    capture_request_read_context().stop_reason().is_some()
}

/// Refuse to begin another request-owned read phase after cancellation.
pub fn ensure_request_read_active(operation: &'static str) -> StorageResult<()> {
    if request_read_is_cancelled() {
        Err(StorageError::Timeout {
            operation: operation.into(),
        })
    } else {
        Ok(())
    }
}

/// Wait until the current request's read-only work is cancelled or timed out.
pub async fn wait_for_request_read_cancellation() {
    let _ = capture_request_read_context().wait_for_stop().await;
}

/// Await one request-owned read phase under the current absolute deadline.
pub async fn await_request_read_phase<F>(
    operation: &'static str,
    future: F,
) -> StorageResult<F::Output>
where
    F: Future,
{
    ensure_request_read_active(operation)?;
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = wait_for_request_read_cancellation() => Err(StorageError::Timeout {
            operation: operation.into(),
        }),
        output = &mut future => {
            ensure_request_read_active(operation)?;
            Ok(output)
        }
    }
}

fn receiver_cancelled(receiver: &tokio::sync::watch::Receiver<bool>) -> bool {
    *receiver.borrow() || receiver.has_changed().is_err()
}

async fn wait_for_receiver_cancellation(mut receiver: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_for_receiver_set(receivers: Arc<[tokio::sync::watch::Receiver<bool>]>) {
    let mut waits: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = receivers
        .iter()
        .cloned()
        .map(|receiver| {
            Box::pin(wait_for_receiver_cancellation(receiver))
                as Pin<Box<dyn Future<Output = ()> + Send>>
        })
        .collect();
    std::future::poll_fn(move |cx| {
        if waits
            .iter_mut()
            .any(|wait| wait.as_mut().poll(cx).is_ready())
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nested_scopes_merge_cancellation_sources() {
        let (outer_tx, outer_rx) = tokio::sync::watch::channel(false);
        let (_inner_tx, inner_rx) = tokio::sync::watch::channel(false);

        let stopped = scope_request_read_cancellation(
            outer_rx,
            scope_request_read_cancellation(inner_rx, async move {
                outer_tx.send(true).expect("outer scope remains live");
                wait_for_request_read_cancellation().await;
                request_read_is_cancelled()
            }),
        )
        .await;

        assert!(stopped);
    }

    #[tokio::test]
    async fn sender_loss_is_request_abandonment() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        drop(tx);

        let stopped = scope_request_read_cancellation(rx, async {
            capture_request_read_context().stop_reason()
        })
        .await;

        assert_eq!(stopped, Some(RequestReadStopReason::Cancelled));
    }

    #[tokio::test]
    async fn spawned_child_inherits_the_same_cancellation() {
        let (tx, rx) = tokio::sync::watch::channel(false);

        let stopped = scope_request_read_cancellation(rx, async move {
            let child = tokio::spawn(inherit_request_read_context(async {
                wait_for_request_read_cancellation().await;
                request_read_is_cancelled()
            }));
            tx.send(true).expect("child receiver remains live");
            child.await.expect("child task")
        })
        .await;

        assert!(stopped);
    }

    #[tokio::test]
    async fn nested_deadline_keeps_the_earlier_absolute_instant() {
        let earlier = RequestReadDeadline::after(Duration::from_secs(10));
        let later = RequestReadDeadline::after(Duration::from_secs(20));

        let effective = scope_request_read_deadline_at(earlier, async {
            effective_request_read_deadline(later)
        })
        .await;

        assert_eq!(effective.async_at(), earlier.async_at());
    }

    #[tokio::test]
    async fn async_phase_cannot_degrade_cancelled_work_to_success() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).expect("receiver remains live");

        let result = scope_request_read_cancellation(rx, async {
            await_request_read_phase("test.phase", async { 7_u8 }).await
        })
        .await;

        assert!(matches!(result, Err(StorageError::Timeout { .. })));
    }
}
