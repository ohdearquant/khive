//! One-shot checkpoint deadlines for dirty in-memory bridges.

use super::*;
use std::sync::Weak;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

struct CheckpointClaim {
    ann: Weak<AnnState>,
    key: AnnKey,
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
    spawn_checkpoint(
        rt.clone(),
        CheckpointTimer {
            claim,
            delay,
            checkpoint,
        },
    );
}

// Keep spawning synchronous so a deadline handoff has an independent Send future.
fn spawn_checkpoint(rt: KhiveRuntime, timer: CheckpointTimer) {
    let shutdown = khive_runtime::daemon_shutdown_token();
    let key = timer.claim.key.clone();
    // Runtime hooks can own ANN state, so a weak claim alone does not bound its
    // lifetime. Retention is one finite delay plus one cancellable attempt.
    khive_runtime::track_named_background_task("memory_ann_checkpoint", async move {
        if let Some(ann) = run_timer(&rt, timer, shutdown).await {
            // run_timer has released the old claim. Another writer may win this
            // claim, in which case its deadline already covers the new period.
            schedule_checkpoint(&rt, &ann, &key).await;
        }
    });
}

/// Return a new dirty period only when a publication advanced its deadline.
/// Errors and an unchanged deadline never enqueue an automatic retry.
async fn run_timer(
    rt: &KhiveRuntime,
    timer: CheckpointTimer,
    shutdown: CancellationToken,
) -> Option<SharedAnn> {
    let CheckpointTimer {
        claim,
        delay,
        checkpoint,
    } = timer;
    let result = tokio::select! {
        biased;
        _ = shutdown.cancelled() => None,
        result = async {
            tokio::time::sleep(delay).await;
            let ann = claim.ann.upgrade()?;
            let policy = checkpoint_policy(&ann);
            let deadline = ann.indexes.read().await.get(&claim.key).and_then(|bridge| {
                checkpoint_delay(policy.interval, bridge.dirty_ops, bridge.last_checkpoint.elapsed())
                    .map(|remaining| (remaining, bridge.last_checkpoint))
            });
            let (remaining, current_checkpoint) = deadline?;
            if current_checkpoint > checkpoint && !remaining.is_zero() {
                return Some(ann);
            }
            let token = match rt.authorize(Namespace::local()) {
                Ok(token) => token,
                Err(error) => {
                    tracing::warn!(%error, model = %claim.key.model, "memory ANN checkpoint authorization failed");
                    return None;
                }
            };
            // Keep the claim through ensure so its completion cannot recursively
            // schedule the same deadline, including after a failed publication.
            match ensure_ann_for_model(rt, &token, &ann, &claim.key.model).await {
                Ok(status) => tracing::debug!(?status, model = %claim.key.model, "memory ANN checkpoint deadline complete"),
                Err(error) if is_benign_shutdown_cancellation(&error) => {
                    tracing::debug!(%error, model = %claim.key.model, "memory ANN checkpoint cancelled at shutdown");
                }
                Err(error) => tracing::warn!(%error, model = %claim.key.model, "memory ANN checkpoint deadline failed"),
            }
            None
        } => result,
    };
    drop(claim);
    result
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
