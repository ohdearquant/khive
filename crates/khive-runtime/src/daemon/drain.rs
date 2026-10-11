use super::DEFAULT_DRAIN_TIMEOUT_SECS;
#[cfg(unix)]
use super::{background_task_count, background_task_names};

#[cfg(unix)]
pub(super) async fn drain(active: &std::sync::atomic::AtomicUsize) -> bool {
    drain_with_timeout(active, drain_timeout()).await
}

/// Voluntary retirement keeps admitted workers and rendezvous ownership alive.
#[cfg(unix)]
pub(super) async fn drain_for_idle(
    active: &std::sync::atomic::AtomicUsize,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut warned = false;
    while active.load(std::sync::atomic::Ordering::SeqCst) + background_task_count() != 0 {
        if !warned && tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "idle drain interval elapsed; retaining workers and rendezvous until settled"
            );
            warned = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
pub(super) async fn drain_with_timeout(
    active: &std::sync::atomic::AtomicUsize,
    timeout: std::time::Duration,
) -> bool {
    use std::sync::atomic::Ordering;
    // One sequentially-consistent order spans connection handoff and tracked
    // task publication. Drain must not observe an ended connection and a
    // stale pre-publication background count as two simultaneous zeroes.
    let remaining = || active.load(Ordering::SeqCst) + background_task_count();
    if remaining() == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while remaining() > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining_connections = active.load(Ordering::SeqCst),
                remaining_background_tasks = background_task_count(),
                outstanding_background_tasks = %background_task_names().join(", "),
                "drain timeout reached; forcing shutdown"
            );
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
    true
}

#[cfg(unix)]
pub(super) async fn finish_connection_tasks(
    tasks: Vec<tokio::task::JoinHandle<()>>,
    drained: bool,
) {
    if !drained {
        for task in &tasks {
            if !task.is_finished() {
                task.abort();
            }
        }
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// The bound `drain()` waits for tracked background tasks at daemon shutdown
/// (`KHIVE_DRAIN_TIMEOUT_SECS`, default 10s). Public so component supervision
/// can clamp per-component shutdown timeouts against it — a component timeout
/// longer than the drain bound could never complete its abort/state
/// transition before the daemon returns.
pub fn drain_timeout() -> std::time::Duration {
    let secs = khive_db::env::env_parse_or("KHIVE_DRAIN_TIMEOUT_SECS", DEFAULT_DRAIN_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}
