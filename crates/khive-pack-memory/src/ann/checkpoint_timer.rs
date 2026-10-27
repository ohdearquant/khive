//! One-shot checkpoint deadlines for dirty in-memory bridges.

use super::*;
use std::sync::Weak;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

struct CheckpointClaim {
    ann: Weak<AnnState>,
    key: AnnKey,
    owner_dropped: CancellationToken,
}

impl CheckpointClaim {
    fn take(ann: &SharedAnn, key: &AnnKey) -> Option<Self> {
        let mut timers = ann
            .checkpoint_timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        timers.insert(key.clone()).then(|| Self {
            ann: Arc::downgrade(ann),
            key: key.clone(),
            owner_dropped: ann.owner_dropped.clone(),
        })
    }
}

impl Drop for CheckpointClaim {
    fn drop(&mut self) {
        if let Some(ann) = self.ann.upgrade() {
            ann.checkpoint_timers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.key);
        }
    }
}

struct CheckpointTimer {
    claim: CheckpointClaim,
    delay: Duration,
    checkpoint: Instant,
}

fn checkpoint_delay(interval: Duration, dirty_ops: u64, elapsed: Duration) -> Option<Duration> {
    if interval.is_zero() || dirty_ops == 0 {
        None
    } else {
        Some(interval.saturating_sub(elapsed))
    }
}

pub(super) async fn schedule_checkpoint(rt: &KhiveRuntime, ann: &SharedAnn, key: &AnnKey) {
    if !ann.checkpoint_timers_enabled || !ann.builds_corpus_indexes || rt.is_read_only() {
        return;
    }
    let policy = checkpoint_policy(ann);
    let deadline = ann.indexes.read().await.get(key).and_then(|bridge| {
        checkpoint_delay(
            policy.interval,
            bridge.dirty_ops,
            bridge.last_checkpoint.elapsed(),
        )
        .map(|delay| (delay, bridge.last_checkpoint))
    });
    let Some((delay, checkpoint)) = deadline else {
        return;
    };
    let Some(claim) = CheckpointClaim::take(ann, key) else {
        return;
    };
    drop(spawn_checkpoint(
        rt,
        CheckpointTimer {
            claim,
            delay,
            checkpoint,
        },
    ));
}

// Keep spawning synchronous so a deadline handoff has an independent Send future.
fn spawn_checkpoint(rt: &KhiveRuntime, timer: CheckpointTimer) -> tokio::task::JoinHandle<()> {
    let shutdown = khive_runtime::daemon_shutdown_token();
    let key = timer.claim.key.clone();
    // Pack callbacks can own ANN state; retaining them would keep the weak
    // claim alive after the last external runtime and pack were dropped.
    let rt = rt.background_view();
    let tracking = TrackedJob::register("memory_ann_checkpoint");
    tokio::spawn(async move {
        if let Some(ann) = run_timer_tracked(&rt, timer, shutdown, &tracking).await {
            // The old claim is gone before a moved deadline is re-enqueued.
            schedule_checkpoint(&rt, &ann, &key).await;
        }
    })
}

// Preserve the existing deterministic timer test entry without double-registering
// production timers. The production caller lends its synchronous registration.
#[cfg(test)]
async fn run_timer(
    rt: &KhiveRuntime,
    timer: CheckpointTimer,
    shutdown: CancellationToken,
) -> Option<SharedAnn> {
    let tracking = TrackedJob::register("memory_ann_checkpoint");
    run_timer_tracked(rt, timer, shutdown, &tracking).await
}

/// A weak claim sleeps without admission. Queue Maintenance before upgrading
/// the owner; keep the original registration/claim through the supervised attempt.
async fn run_timer_tracked(
    rt: &KhiveRuntime,
    timer: CheckpointTimer,
    shutdown: CancellationToken,
    tracking: &TrackedJob,
) -> Option<SharedAnn> {
    let CheckpointTimer {
        claim,
        delay,
        checkpoint,
    } = timer;
    let domain = rt.background_domain();
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => return None,
        _ = domain.closed() => return None,
        _ = claim.owner_dropped.cancelled() => return None,
        _ = tokio::time::sleep(delay) => {}
    }
    let admitted = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return None,
        _ = domain.closed() => return None,
        _ = claim.owner_dropped.cancelled() => return None,
        result = domain.admit(WorkClass::Maintenance, tracking) => match result {
            Ok(job) => job,
            Err(error) => {
                tracing::debug!(%error, model = %claim.key.model, "memory ANN checkpoint admission refused");
                return None;
            }
        },
    };
    let rt = rt.background_view();
    let submitted = domain.spawn_supervised(admitted, move |job| async move {
        // This synchronous owner upgrade occurs only after admission. No queued
        // future owns AnnState or an ANN lock. Cancellation skips the body.
        if job.is_cancelled() { return JobOutcome::Cancelled; }
        let Some(ann) = claim.ann.upgrade() else { return JobOutcome::Completed(None); };
        let policy = checkpoint_policy(&ann);
        let deadline = match job.run_body(async {
            ann.indexes.read().await.get(&claim.key).and_then(|bridge| {
                checkpoint_delay(policy.interval, bridge.dirty_ops, bridge.last_checkpoint.elapsed())
                    .map(|remaining| (remaining, bridge.last_checkpoint))
            })
        }).await {
            BodyOutcome::Finished(deadline) => deadline,
            BodyOutcome::Cancelled => return JobOutcome::Cancelled,
        };
        let Some((remaining, current_checkpoint)) = deadline else { return JobOutcome::Completed(None); };
        if current_checkpoint > checkpoint && !remaining.is_zero() {
            drop(claim);
            return JobOutcome::Completed(Some(ann));
        }
        let token = match rt.authorize(Namespace::local()) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, model = %claim.key.model, "memory ANN checkpoint authorization failed");
                return JobOutcome::Completed(None);
            }
        };
        // Nested phase attribution stays `warm`; scheduling remains Maintenance.
        // ensure never acquires another Warm permit. The claim prevents recursive
        // scheduling, including failed publications and terminal finalization.
        let outcome = ensure_ann_for_model_admitted(&rt, &token, &ann, &claim.key.model, &job).await;
        finish_checkpoint_attempt(claim, outcome)

    });
    let Ok(background) = submitted else {
        return None;
    };
    // Dropping this join proxy requests cancellation, but does not drop the
    // supervisor's phase finalizer or a started CPU child's same-lease hold.
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => None,
        result = background.join() => match result {
            Ok(JobOutcome::Completed(next)) => next,
            _ => None,
        },
    }
}

fn finish_checkpoint_attempt(
    claim: CheckpointClaim,
    outcome: JobOutcome<Result<AnnEnsureStatus, RuntimeError>>,
) -> JobOutcome<Option<SharedAnn>> {
    let disposition = match outcome {
        JobOutcome::Completed(Ok(status)) => {
            tracing::debug!(?status, model = %claim.key.model, "memory ANN checkpoint deadline complete");
            JobOutcome::Completed(None)
        }
        JobOutcome::Completed(Err(error)) if is_benign_shutdown_cancellation(&error) => {
            tracing::debug!(%error, model = %claim.key.model, "memory ANN checkpoint cancelled at shutdown");
            JobOutcome::Completed(None)
        }
        JobOutcome::Completed(Err(error)) => {
            tracing::warn!(%error, model = %claim.key.model, "memory ANN checkpoint deadline failed");
            JobOutcome::Completed(None)
        }
        JobOutcome::Cancelled => JobOutcome::Cancelled,
        JobOutcome::Refused(error) => JobOutcome::Refused(error),
        JobOutcome::FinalizationTimedOut => JobOutcome::FinalizationTimedOut,
    };
    drop(claim);
    disposition
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_uses_remaining_interval_and_skips_clean_or_disabled_bridges() {
        let interval = Duration::from_secs(300);
        assert_eq!(
            checkpoint_delay(interval, 1, Duration::from_secs(45)),
            Some(Duration::from_secs(255))
        );
        assert_eq!(
            checkpoint_delay(interval, 1, Duration::from_secs(301)),
            Some(Duration::ZERO)
        );
        assert_eq!(checkpoint_delay(interval, 0, Duration::ZERO), None);
        assert_eq!(checkpoint_delay(Duration::ZERO, 1, Duration::ZERO), None);
    }

    #[test]
    fn timer_claim_is_per_model_and_released_on_drop() {
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("timer-model");
        let first = CheckpointClaim::take(&ann, &key).expect("first timer claim");
        assert!(
            CheckpointClaim::take(&ann, &key).is_none(),
            "a model must have at most one checkpoint timer"
        );
        let other = CheckpointClaim::take(&ann, &AnnKey::new("other-model"))
            .expect("independent model timer");
        drop(first);
        let replacement = CheckpointClaim::take(&ann, &key)
            .expect("completed timer must release its model claim");
        drop(other);
        drop(replacement);
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
    }

    #[test]
    fn timer_claim_does_not_own_ann_state() {
        let ann = new_shared_for_role(true);
        let weak = Arc::downgrade(&ann);
        let claim = CheckpointClaim::take(&ann, &AnnKey::new("timer-model")).unwrap();
        drop(ann);
        assert!(
            weak.upgrade().is_none(),
            "timer claim must retain ANN state weakly"
        );
        drop(claim);
    }

    #[tokio::test]
    async fn checkpoint_task_releases_runtime_and_writer_when_last_ann_owner_drops() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("checkpoint-owner.db");
        let backend = Arc::new(
            khive_db::StorageBackend::sqlite_for_test_with_journal_mode_in(
                &db_path,
                true,
                Duration::from_secs(10),
                dir.path().join("locks"),
            )
            .unwrap(),
        );
        backend.prepare_core_schema().unwrap();
        assert!(backend.pool().write_queue_active());
        assert!(backend.pool().writer_task_handle().unwrap().is_some());
        let writer = backend.pool().take_writer_task_join().unwrap();
        let rt = KhiveRuntime::from_prepared_backend(
            backend.clone(),
            khive_runtime::RuntimeConfig {
                db_path: Some(db_path),
                ..khive_runtime::RuntimeConfig::no_embeddings()
            },
        )
        .unwrap();
        let background_root = rt
            .background_root()
            .expect("external domain owner for ANN-owner lifetime fixture");
        let ann = new_shared_for_role(true);
        rt.install_note_search_ann_provider(Arc::new(
            crate::note_search::MemoryNoteSearchAnnProvider::new(rt.clone(), ann.clone()),
        ));
        let key = AnnKey::new("timer-model");
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::from_secs(300),
            checkpoint: Instant::now(),
        };
        let retained_owner = ann.clone();
        let weak = Arc::downgrade(&ann);
        let owner_dropped = ann.owner_dropped.clone();
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let task = spawn_checkpoint(&rt, timer);
        drop(rt);
        drop(backend);
        drop(ann);
        tokio::task::yield_now().await;
        assert_eq!(
            Arc::strong_count(&retained_owner),
            1,
            "checkpoint runtime must not retain its installed ANN provider"
        );
        assert!(!owner_dropped.is_cancelled());
        assert!(
            !task.is_finished(),
            "a remaining owner keeps its timer alive"
        );

        drop(retained_owner);
        assert!(weak.upgrade().is_none());
        assert!(owner_dropped.is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("last owner drop must end an idle checkpoint before its 300s deadline")
            .unwrap();
        assert_eq!(tokio::time::Instant::now(), started);
        tokio::time::resume();
        drop(background_root);
        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("checkpoint exit must release the file-backed writer")
            .unwrap();
        dir.close().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn live_ann_owner_preserves_the_full_checkpoint_delay_and_releases_claim() {
        let rt = KhiveRuntime::memory().unwrap();
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("timer-model");
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::from_secs(300),
            checkpoint: Instant::now(),
        };
        let mut pending = Box::pin(run_timer(&rt, timer, CancellationToken::new()));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_secs(299)).await;
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(CheckpointClaim::take(&ann, &key).is_none());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            pending.await.is_none(),
            "an absent bridge needs no checkpoint"
        );
        assert!(!ann.owner_dropped.is_cancelled());
        assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
        let replacement = CheckpointClaim::take(&ann, &key).unwrap();
        drop(replacement);
    }

    #[tokio::test]
    async fn shutdown_cancels_deadline_and_releases_timer_claim() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("timer-model");
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::from_secs(300),
            checkpoint: Instant::now(),
        };
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let next = tokio::time::timeout(Duration::from_secs(1), run_timer(&rt, timer, shutdown))
            .await
            .expect("shutdown must cancel the checkpoint timer before its deadline");
        assert!(next.is_none());
        assert!(
            ann.checkpoint_timers.lock().unwrap().is_empty(),
            "shutdown must release the checkpoint timer claim"
        );
    }

    #[tokio::test]
    async fn elapsed_timer_hands_off_an_advanced_dirty_period() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let ann = new_shared_for_role(true);
        let key = AnnKey::new("timer-model");
        let now = Instant::now();
        let mut bridge = AnnBridge::build(
            vec![1.0, 0.0, 0.0, 1.0],
            2,
            vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            HashSet::new(),
        )
        .expect("bridge");
        bridge.dirty_ops = 1;
        bridge.last_checkpoint = now;
        ann.indexes.write().await.insert(key.clone(), bridge);
        ann.checkpoint_policy.write().unwrap().interval = Duration::from_secs(300);
        let timer = CheckpointTimer {
            claim: CheckpointClaim::take(&ann, &key).unwrap(),
            delay: Duration::ZERO,
            checkpoint: now - Duration::from_secs(1),
        };
        let next = run_timer(&rt, timer, CancellationToken::new())
            .await
            .expect("advanced dirty checkpoint must hand off its remaining deadline");
        assert!(Arc::ptr_eq(&next, &ann));
        let next_claim = CheckpointClaim::take(&next, &key)
            .expect("deadline handoff must release its old claim before rescheduling");
        assert!(CheckpointClaim::take(&next, &key).is_none());
        drop(next_claim);
    }
}

#[cfg(test)]
#[path = "checkpoint_admission_tests.rs"]
mod admission_tests;
