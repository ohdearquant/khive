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

#[test]
fn empty_tail_preserves_candidate_order_duplicates_and_score_bits() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let high_id = Uuid::from_u128(9);
    let low_id = Uuid::from_u128(1);
    let nan = f64::from_bits(0x7ff8_0000_0000_0042);
    let candidates = vec![(high_id, 0.25_f64), (high_id, nan), (low_id, 0.75)];
    let expected: Vec<_> = candidates
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    let merged = merge_fresh_tail(candidates, Vec::new(), |_| -> Result<f64, &'static str> {
        panic!("empty tail must not score")
    })
    .expect("empty tail");
    let actual: Vec<_> = merged
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn tail_merge_preserves_f64_precision_and_descending_order() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let low = 0.5_f64;
    let high = f64::from_bits(low.to_bits() + 1);
    let carried_high = f64::from_bits(low.to_bits() + 2);
    assert_eq!(
        (low as f32).to_bits(),
        (high as f32).to_bits(),
        "fixture detects narrowing"
    );
    assert_eq!((low as f32).to_bits(), (carried_high as f32).to_bits());
    let low_id = Uuid::from_u128(1);
    let high_id = Uuid::from_u128(9);
    let carried_id = Uuid::from_u128(7);
    let mut calls = 0;
    let merged = merge_fresh_tail(
        vec![(low_id, low), (carried_id, carried_high)],
        vec![(high_id, Some(vec![1.0]))],
        |embedding| {
            assert_eq!(embedding, &[1.0]);
            calls += 1;
            Ok::<_, &'static str>(high)
        },
    )
    .expect("score tail");
    assert_eq!(calls, 1);
    let actual: Vec<_> = merged
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    assert_eq!(
        actual,
        vec![
            (carried_id, carried_high.to_bits()),
            (high_id, high.to_bits()),
            (low_id, low.to_bits())
        ]
    );
}

#[test]
fn tail_merge_scores_repeated_upserts_in_order_and_stops_at_first_error() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let repeated = Uuid::from_u128(1);
    let deleted = Uuid::from_u128(2);
    let last = Uuid::from_u128(3);
    let ops = vec![
        (repeated, Some(vec![1.0])),
        (deleted, None),
        (repeated, Some(vec![2.0])),
        (last, Some(vec![3.0])),
    ];
    let mut calls = Vec::new();
    let merged = merge_fresh_tail(Vec::new(), ops.clone(), |embedding| {
        calls.push(embedding[0]);
        Ok::<_, (&'static str, u32)>(f64::from(embedding[0]))
    })
    .expect("successful callback");
    assert_eq!(calls, vec![1.0, 2.0, 3.0]);
    assert_eq!(merged, vec![(last, 3.0), (repeated, 2.0)]);

    calls.clear();
    let error = merge_fresh_tail(vec![(deleted, 9.0)], ops, |embedding| {
        calls.push(embedding[0]);
        if embedding[0] == 2.0 {
            Err(("score rejected", 42_u32))
        } else {
            Ok(f64::from(embedding[0]))
        }
    })
    .expect_err("first failure must propagate");
    assert_eq!(error, ("score rejected", 42));
    assert_eq!(calls, vec![1.0, 2.0]);
}

#[test]
fn tail_merge_preserves_replacement_delete_and_uncoalesced_upsert_semantics() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let updated = Uuid::from_u128(1);
    let deleted = Uuid::from_u128(2);
    let untouched = Uuid::from_u128(3);
    let upsert_then_delete = Uuid::from_u128(4);
    let delete_then_upsert = Uuid::from_u128(5);
    let candidates = vec![
        (updated, 0.1),
        (updated, 0.2),
        (deleted, 0.9),
        (untouched, 0.75),
        (untouched, 0.5),
        (upsert_then_delete, 0.1),
        (delete_then_upsert, 0.1),
    ];
    let ops = vec![
        (upsert_then_delete, Some(vec![4.0])),
        (upsert_then_delete, None),
        (delete_then_upsert, None),
        (delete_then_upsert, Some(vec![5.0])),
        (updated, Some(vec![6.0])),
        (updated, Some(vec![7.0])),
        (deleted, None),
    ];
    let mut calls = Vec::new();
    let merged = merge_fresh_tail(candidates, ops, |embedding| {
        calls.push(embedding[0]);
        Ok::<_, &'static str>(f64::from(embedding[0]))
    })
    .expect("score upserts");
    assert_eq!(calls, vec![4.0, 5.0, 6.0, 7.0]);
    assert_eq!(
        merged,
        vec![
            (updated, 7.0),
            (delete_then_upsert, 5.0),
            (upsert_then_delete, 4.0),
            (untouched, 0.75),
            (untouched, 0.5),
        ]
    );
}

#[test]
fn tail_merge_orders_equal_scores_and_incomparable_scores_by_uuid() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let low = Uuid::from_u128(1);
    let middle = Uuid::from_u128(2);
    let high = Uuid::from_u128(3);
    let merged = merge_fresh_tail(
        vec![(middle, 1.0_f32)],
        vec![(high, Some(vec![1.0])), (low, Some(vec![1.0]))],
        |embedding| Ok::<_, &'static str>(embedding[0]),
    )
    .expect("equal scores");
    assert_eq!(merged, vec![(low, 1.0), (middle, 1.0), (high, 1.0)]);

    let low_nan = f64::from_bits(0x7ff8_0000_0000_0011);
    let high_nan = f64::from_bits(0x7ff8_0000_0000_0022);
    let merged = merge_fresh_tail(
        vec![(high, high_nan), (low, low_nan)],
        vec![(middle, None)],
        |_| -> Result<f64, &'static str> { panic!("deletes must not score") },
    )
    .expect("incomparable scores");
    let actual: Vec<_> = merged
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    assert_eq!(
        actual,
        vec![(low, low_nan.to_bits()), (high, high_nan.to_bits())]
    );
}

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
