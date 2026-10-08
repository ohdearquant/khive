//! Helpers shared by the packs that keep a file-backed ANN checkpoint.
//!
//! Checkpoint helpers provide the directory lock and peer-rotation polling loop.
//! Packs retain their error-text prefixes, task labels and refresh steps.
//! The [`corpus`] module builds the SQL for corpus counts and write-log probes
//! from each consumer's namespace, field and live-row predicates.
//!
//! The [`registry`] submodule holds the durable consumer registration lifecycle that gates
//! compaction of the ANN write log.

pub mod corpus;
pub mod registry;

use std::fs::File;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Name of the lock file taken inside a checkpoint directory.
const CHECKPOINT_LOCK_FILE: &str = ".bridge-checkpoint.lock";

/// Take the exclusive checkpoint lock for `dir`, creating the directory when it is missing.
///
/// The lock is held until the returned file is dropped. `prefix` names the caller in every
/// error message: `"memory ANN"` produces `"open memory ANN lock ..."`.
#[doc(hidden)]
pub fn acquire_checkpoint_lock(dir: &Path, prefix: &str) -> Result<File, String> {
    std::fs::create_dir_all(dir).map_err(|error| {
        format!(
            "create {prefix} checkpoint directory {}: {error}",
            dir.display()
        )
    })?;
    let lock_path = dir.join(CHECKPOINT_LOCK_FILE);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open {prefix} lock {}: {error}", lock_path.display()))?;
    lock.lock()
        .map_err(|error| format!("acquire {prefix} lock {}: {error}", lock_path.display()))?;
    Ok(lock)
}

/// Async form of [`acquire_checkpoint_lock`].
///
/// The blocking acquisition runs on the blocking pool, so a contended lock never stalls an
/// executor thread.
#[doc(hidden)]
pub async fn acquire_checkpoint_lock_async(
    dir: PathBuf,
    prefix: &'static str,
) -> Result<File, String> {
    tokio::task::spawn_blocking(move || acquire_checkpoint_lock(&dir, prefix))
        .await
        .map_err(|error| format!("{prefix} lock task failed: {error}"))?
}

/// Claim a one-shot watcher synchronously and return its unpolled future.
///
/// The caller keeps backend selection and task spawning. The watcher retains only
/// a weak ANN reference between ticks; `refresh` receives an owned handle for one
/// tick and must not capture another strong handle to the ANN. Dropping the future
/// does not reset `started`. Timing and cancellation follow [`rotation_watch_loop`].
#[doc(hidden)]
pub fn rotation_watch_future<T, F, Fut>(
    ann: &std::sync::Arc<T>,
    started: &std::sync::atomic::AtomicBool,
    ann_root: PathBuf,
    interval: Duration,
    shutdown: CancellationToken,
    mut refresh: F,
) -> Option<impl Future<Output = ()> + Send + 'static>
where
    T: Send + Sync + 'static,
    F: FnMut(std::sync::Arc<T>, PathBuf) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    use std::sync::atomic::Ordering;

    if started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let ann = std::sync::Arc::downgrade(ann);
    let tick = move || {
        let ann = ann.upgrade();
        let ann_root = ann_root.clone();
        let refresh = ann.map(|ann| refresh(ann, ann_root));
        async move {
            let Some(refresh) = refresh else {
                return ControlFlow::Break(());
            };
            refresh.await;
            ControlFlow::Continue(())
        }
    };
    Some(rotation_watch_loop(interval, shutdown, tick))
}

/// Call `tick` every `interval` until `shutdown` is cancelled or `tick` returns
/// [`ControlFlow::Break`].
///
/// This returns the loop as a future so the caller keeps ownership of spawning and task
/// tracking, which live above this crate. The first tick fires one `interval` after the future
/// is first polled, ticks missed while `tick` is running are skipped rather than replayed, and
/// shutdown is observed between ticks, never during one.
#[doc(hidden)]
pub async fn rotation_watch_loop<F, Fut>(
    interval: Duration,
    shutdown: CancellationToken,
    mut tick: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ControlFlow<()>>,
{
    let start = tokio::time::Instant::now() + interval;
    let mut ticks = tokio::time::interval_at(start, interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticks.tick() => {}
        }
        if tick().await.is_break() {
            break;
        }
    }
}
