#![cfg(feature = "ann")]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use khive_retrieval::ann::{
    acquire_checkpoint_lock, acquire_checkpoint_lock_async, rotation_watch_loop,
};
use tokio_util::sync::CancellationToken;

/// A scratch directory under the system temp dir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let unique = format!("khive-retrieval-ann-{}-{name}", std::process::id());
        let path = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A path whose parent is a regular file, so the directory cannot be created.
fn blocked_directory(scratch: &ScratchDir) -> PathBuf {
    let blocker = scratch.path().join("blocker");
    std::fs::write(&blocker, b"regular file").expect("write blocker file");
    blocker.join("segment")
}

#[test]
fn create_directory_error_carries_the_prefix() {
    let scratch = ScratchDir::new("create-error");
    let dir = blocked_directory(&scratch);
    let shown = dir.display();

    for prefix in ["memory ANN", "ANN bridge"] {
        let result = acquire_checkpoint_lock(&dir, prefix);
        let error = result.expect_err("lock must fail");
        let expected = format!("create {prefix} checkpoint directory {shown}: ");
        assert!(error.starts_with(&expected), "got: {error}");
    }
}

#[test]
fn open_error_carries_the_prefix() {
    let scratch = ScratchDir::new("open-error");
    let lock_path = scratch.path().join(".bridge-checkpoint.lock");
    std::fs::create_dir(&lock_path).expect("occupy the lock path");
    let shown = lock_path.display();

    for prefix in ["memory ANN", "ANN bridge"] {
        let result = acquire_checkpoint_lock(scratch.path(), prefix);
        let error = result.expect_err("lock must fail");
        let expected = format!("open {prefix} lock {shown}: ");
        assert!(error.starts_with(&expected), "got: {error}");
    }
}

#[tokio::test]
async fn async_lock_forwards_the_prefix() {
    let scratch = ScratchDir::new("async-error");
    let dir = blocked_directory(&scratch);
    let shown = dir.display();

    let result = acquire_checkpoint_lock_async(dir.clone(), "memory ANN").await;
    let error = result.expect_err("lock must fail");
    let expected = format!("create memory ANN checkpoint directory {shown}: ");
    assert!(error.starts_with(&expected), "got: {error}");
}

#[test]
fn second_acquisition_waits_for_the_first_to_release() {
    let scratch = ScratchDir::new("serialize");
    let attempt = acquire_checkpoint_lock(scratch.path(), "ANN bridge");
    let first = attempt.expect("first lock");

    let dir = scratch.path().to_path_buf();
    let (sender, receiver) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let attempt = acquire_checkpoint_lock(&dir, "ANN bridge");
        let second = attempt.expect("second lock");
        sender.send(()).expect("report acquisition");
        drop(second);
    });

    let early = receiver.recv_timeout(Duration::from_millis(300));
    assert!(early.is_err(), "lock granted while held");

    drop(first);
    let late = receiver.recv_timeout(Duration::from_secs(10));
    assert!(late.is_ok(), "lock not granted after release");
    waiter.join().expect("waiter thread");
}

#[tokio::test]
async fn loop_returns_promptly_after_shutdown() {
    let shutdown = CancellationToken::new();
    let ticks = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ticks);
    // The interval is an hour, so a loop that honours shutdown never ticks. A loop that
    // ignored shutdown would tick; breaking on the third tick keeps that failure bounded.
    let tick = move || {
        let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if seen < 3 {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        }
    };
    let watch = rotation_watch_loop(Duration::from_secs(3600), shutdown.clone(), tick);

    let handle = tokio::spawn(watch);
    tokio::task::yield_now().await;
    shutdown.cancel();

    let finished = tokio::time::timeout(Duration::from_secs(10), handle).await;
    assert!(finished.is_ok(), "no return after shutdown");
    assert_eq!(ticks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn loop_stops_when_the_tick_breaks() {
    let shutdown = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let tick = move || {
        let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if seen < 3 {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        }
    };
    let watch = rotation_watch_loop(Duration::from_millis(10), shutdown, tick);

    let finished = tokio::time::timeout(Duration::from_secs(10), watch).await;
    assert!(finished.is_ok(), "loop ran past a break");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
