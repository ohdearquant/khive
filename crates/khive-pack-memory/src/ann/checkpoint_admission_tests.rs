use super::super::resource_admission_tests::{run_sync_case, CloseOnDrop, WallClock};
use super::*;

// Each caller is enclosed by its one real-wall case budget.
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    future.await
}

async fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "timer must be waiting at this fixture boundary"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

// Advance the real zero-delay timer before observing the admission boundary;
// the first poll alone could still be the sleep, even with Duration::ZERO.
async fn queued<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    poll_pending(future.as_mut()).await;
    bounded(tokio::time::sleep(Duration::from_millis(1))).await;
    poll_pending(future.as_mut()).await;
}

#[tokio::test]
async fn idle_checkpoint_claim_holds_no_permit_or_strong_owner() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("idle-budget-model");
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::from_secs(300),
            checkpoint: Instant::now(),
        };
        let count = Arc::strong_count(&ann);
        let tracking = TrackedJob::register("memory_idle_checkpoint_test");
        let shutdown = CancellationToken::new();
        let mut future = Box::pin(run_timer_tracked(&rt, timer, shutdown.clone(), &tracking));
        poll_pending(future.as_mut()).await;
        assert_eq!(
            Arc::strong_count(&ann),
            count,
            "sleeping timer must stay weak"
        );
        let handle = rt.background_domain();
        let occupied = bounded(handle.admit(WorkClass::Maintenance, &tracking))
            .await
            .unwrap();
        shutdown.cancel();
        assert!(bounded(future).await.is_none());
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
        drop(occupied);
    })
    .await;
}

#[tokio::test]
async fn queued_checkpoint_owner_drop_cancels_before_strong_upgrade() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_queued_owner_test");
        let occupied = handle
            .admit(WorkClass::Maintenance, &tracking)
            .await
            .unwrap();
        let ann = new_shared_for_role(true);
        let weak = Arc::downgrade(&ann);
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &AnnKey::new("queued-owner-model")).unwrap(),
            delay: Duration::ZERO,
            checkpoint: Instant::now(),
        };
        let mut future = Box::pin(run_timer_tracked(
            &rt,
            timer,
            CancellationToken::new(),
            &tracking,
        ));
        queued(future.as_mut()).await;
        assert_eq!(
            Arc::strong_count(&ann),
            1,
            "queued admission must still hold only a weak ANN claim"
        );
        drop(ann);
        assert!(
            weak.upgrade().is_none(),
            "queued Maintenance must not retain the ANN owner"
        );
        assert!(
            bounded(future).await.is_none(),
            "owner drop must cancel the queued claim"
        );
        drop(occupied);
    })
    .await;
}

#[tokio::test]
async fn queued_checkpoint_shutdown_releases_claim_without_attempt() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_queued_shutdown_test");
        let occupied = handle
            .admit(WorkClass::Maintenance, &tracking)
            .await
            .unwrap();
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("queued-shutdown-model");
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::ZERO,
            checkpoint: Instant::now(),
        };
        let shutdown = CancellationToken::new();
        let mut future = Box::pin(run_timer_tracked(&rt, timer, shutdown.clone(), &tracking));
        queued(future.as_mut()).await;
        shutdown.cancel();
        assert!(bounded(future).await.is_none());
        assert!(
            ann.checkpoint_timers.lock().unwrap().is_empty(),
            "queued cancellation must release the per-model claim"
        );
        drop(occupied);
        let replacement =
            CheckpointClaim::take(&ann, &key).expect("claim reusable after cancelled queue");
        drop(replacement);
    })
    .await;
}

#[tokio::test]
async fn checkpoint_rechecks_moved_deadline_after_admission_and_hands_off_claim() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_deadline_after_queue_test");
        let occupied = handle
            .admit(WorkClass::Maintenance, &tracking)
            .await
            .unwrap();
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("moved-budget-model");
        let old = Instant::now() - Duration::from_secs(301);
        let mut bridge = AnnBridge::build(
            vec![1.0, 0.0, 0.0, 1.0],
            2,
            vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            HashSet::new(),
        )
        .unwrap();
        bridge.dirty_ops = 1;
        bridge.last_checkpoint = old;
        ann.indexes.write().await.insert(key.clone(), bridge);
        ann.checkpoint_policy.write().unwrap().interval = Duration::from_secs(300);
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::ZERO,
            checkpoint: old,
        };
        let mut future = Box::pin(run_timer_tracked(
            &rt,
            timer,
            CancellationToken::new(),
            &tracking,
        ));
        queued(future.as_mut()).await;
        ann.indexes
            .write()
            .await
            .get_mut(&key)
            .unwrap()
            .last_checkpoint = Instant::now();
        drop(occupied);
        let next = bounded(future)
            .await
            .expect("moved dirty period must be handed off after admission");
        assert!(Arc::ptr_eq(&next, &ann));
        let replacement = CheckpointClaim::take(&ann, &key)
            .expect("handoff releases old claim before rescheduling");
        assert!(CheckpointClaim::take(&ann, &key).is_none());
        drop(replacement);
    })
    .await;
}

#[tokio::test]
async fn domain_close_wakes_idle_checkpoint_with_live_ann_owner() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("domain-close-idle-model");
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::from_secs(300),
            checkpoint: Instant::now(),
        };
        let tracking = TrackedJob::register("memory_domain_close_timer_test");
        let mut future = Box::pin(run_timer_tracked(
            &rt,
            timer,
            CancellationToken::new(),
            &tracking,
        ));
        poll_pending(future.as_mut()).await;
        rt.background_domain().control().close(Instant::now());
        assert!(
            bounded(future).await.is_none(),
            "domain close must wake an idle timer without an ANN owner drop"
        );
        assert!(!ann.owner_dropped.is_cancelled());
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
    })
    .await;
}

#[test]
fn checkpoint_unsuccessful_dispositions_survive_claim_release() {
    run_sync_case(move || {
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("typed-checkpoint-disposition-model");
        let claim = CheckpointClaim::take(&ann, &key).unwrap();
        assert!(
            matches!(
                finish_checkpoint_attempt(claim, JobOutcome::Cancelled),
                JobOutcome::Cancelled
            ),
            "checkpoint cancellation must not become Completed(None)"
        );
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
        let claim = CheckpointClaim::take(&ann, &key).expect("cancelled attempt releases claim");
        let refused = finish_checkpoint_attempt(
            claim,
            JobOutcome::Refused(PriorityError::Unsupported {
                target: "checkpoint-typed-refusal-fixture",
            }),
        );
        assert!(
            matches!(
                refused,
                JobOutcome::Refused(PriorityError::Unsupported {
                    target: "checkpoint-typed-refusal-fixture",
                })
            ),
            "checkpoint priority refusal must retain its typed reason"
        );
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
        let claim = CheckpointClaim::take(&ann, &key).expect("refused attempt releases claim");
        assert!(
            matches!(
                finish_checkpoint_attempt(claim, JobOutcome::FinalizationTimedOut),
                JobOutcome::FinalizationTimedOut
            ),
            "checkpoint finalization timeout must not become Completed(None)"
        );
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
        drop(CheckpointClaim::take(&ann, &key).expect("timed-out attempt releases claim"));
    });
}
