//! Helpers shared by the packs that keep a file-backed ANN checkpoint.
//!
//! Neither item knows anything about an index. One is the lock every writer of a checkpoint
//! directory takes, the other is the polling loop that notices a peer process rotating a
//! segment. A pack supplies its own error-text prefix, task label and refresh step.

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
