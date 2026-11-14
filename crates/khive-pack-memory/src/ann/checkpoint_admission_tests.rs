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

fn quiet_checkpoint_child() -> bool {
    let mut fixture = None;
    khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_QUIET_CHECKPOINT_CHILD",
        false,
        |command| {
            let mut builder = tempfile::Builder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let directory = builder.tempdir().unwrap();
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env("HOME", directory.path())
                .env("KHIVE_TEST_HARNESS", "1");
            fixture = Some(directory);
        },
    )
}

const QUIET_CHECKPOINT_MODEL: &str = "quiet-checkpoint-model";
struct QuietCheckpointEmbedder;
#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for QuietCheckpointEmbedder {
    fn name(&self) -> &str {
        QUIET_CHECKPOINT_MODEL
    }
    fn dimensions(&self) -> usize {
        2
    }
    async fn build(&self) -> Result<Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(Arc::new(QuietCheckpointEmbedder))
    }
}
#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for QuietCheckpointEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts
            .iter()
            .map(|text| vec![1.0, text.len() as f32])
            .collect())
    }
    fn supports_model(&self, _: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "quiet-checkpoint-fixture"
    }
}

async fn quiet_checkpoint_fixture(cancel: bool) {
    let directory = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(directory.path().join("checkpoint.db")),
        volume_lock_dir: Some(directory.path().join("locks")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    rt.try_register_embedder(QuietCheckpointEmbedder).unwrap();
    assert_eq!(
        rt.registered_embedding_model_names(),
        vec![QUIET_CHECKPOINT_MODEL.to_owned()]
    );
    assert_eq!(
        rt.embedding_models_for_note_kind("memory"),
        vec![QUIET_CHECKPOINT_MODEL.to_owned()]
    );
    let _close = CloseOnDrop::new(&rt);
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut expected_ids = HashSet::new();
    for content in ["first quiet checkpoint note", "second distinct note"] {
        let note = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                content,
                Some(0.8),
                0.01,
                None,
                vec![],
                // Note-kind policy selects the registered custom provider;
                // explicit model overrides accept built-in lattice aliases only.
                None,
            )
            .await
            .unwrap();
        expected_ids.insert(note.id);
    }
    let ann = new_shared_for_role(true);
    assert_eq!(
        ensure_ann_for_model(&rt, &token, &ann, QUIET_CHECKPOINT_MODEL)
            .await
            .unwrap(),
        AnnEnsureStatus::Built { vectors: 2 }
    );
    let watermark_before = read_own_watermark(&rt, QUIET_CHECKPOINT_MODEL)
        .await
        .unwrap()
        .unwrap();
    assert!(
        watermark_before > 0,
        "production ensure durably protects the seeded corpus"
    );
    let third = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "third new checkpoint tail note",
            Some(0.8),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .unwrap();
    expected_ids.insert(third.id);
    let key = AnnKey::new(QUIET_CHECKPOINT_MODEL);
    let old = Instant::now() - Duration::from_secs(301);
    {
        let mut indexes = ann.indexes.write().await;
        let bridge = indexes.get_mut(&key).unwrap();
        bridge.dirty_ops = 1;
        bridge.last_checkpoint = old;
    }
    ann.checkpoint_policy.write().unwrap().interval = Duration::from_secs(300);
    let events = rt.events(&token).unwrap();
    let filter = khive_storage::EventFilter {
        verbs: vec!["memory.ann_warm".into()],
        ..Default::default()
    };
    let page = khive_storage::PageRequest {
        limit: 100,
        offset: 0,
    };
    let before: HashSet<_> = events
        .query_events(filter.clone(), page.clone())
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|row| row.id)
        .collect();
    let timer = CheckpointTimer {
        claim: CheckpointClaim::take(&ann, &key).unwrap(),
        delay: Duration::ZERO,
        checkpoint: old,
    };
    let shutdown = CancellationToken::new();
    let tracking = TrackedJob::register("quiet_checkpoint_fixture");
    let quiet_deadline =
        khive_runtime::resource_admission::request_quiet(if cancel { 300 } else { 1 }).unwrap();
    let mut attempt = Box::pin(run_timer_tracked(&rt, timer, shutdown.clone(), &tracking));
    if cancel {
        queued(attempt.as_mut()).await;
        assert!(ann.checkpoint_timers.lock().unwrap().contains(&key));
        shutdown.cancel();
        assert!(attempt.await.is_none());
        while !ann.checkpoint_timers.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(khive_runtime::resource_admission::quiet_until().is_some());
        assert_eq!(
            read_own_watermark(&rt, QUIET_CHECKPOINT_MODEL)
                .await
                .unwrap(),
            Some(watermark_before)
        );
        assert_eq!(ann.indexes.read().await.get(&key).unwrap().dirty_ops, 1);
    } else {
        assert!(attempt.await.is_none());
        assert_eq!(khive_runtime::resource_admission::quiet_until(), None);
        assert!(
            read_own_watermark(&rt, QUIET_CHECKPOINT_MODEL)
                .await
                .unwrap()
                .unwrap()
                > watermark_before,
            "actual checkpoint advances durable protection over the new vector"
        );
        let indexes = ann.indexes.read().await;
        let bridge = indexes
            .get(&key)
            .expect("checkpoint retains populated bridge");
        assert_eq!(
            bridge.dirty_ops, 0,
            "successful checkpoint clears dirtiness"
        );
        assert!(bridge.last_checkpoint > old);
        assert_eq!(
            bridge.id_map.iter().copied().collect::<HashSet<_>>(),
            expected_ids
        );
    }
    let rows: Vec<_> = events
        .query_events(filter, page)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|row| !before.contains(&row.id))
        .collect();
    if cancel {
        assert!(
            rows.is_empty(),
            "cancelled pre-entry timer invents no phase pair"
        );
    } else {
        assert_eq!(rows.len(), 2);
        let started = rows
            .iter()
            .find(|row| row.kind == khive_types::EventKind::PhaseStarted)
            .unwrap();
        assert!(
            started.created_at >= quiet_deadline.timestamp_micros(),
            "actual phase-entry timestamp, not observer scheduling, proves deferral"
        );
        let completed = rows
            .iter()
            .find(|row| row.kind == khive_types::EventKind::PhaseCompleted)
            .unwrap();
        assert!(matches!(
            completed.payload["path"].as_str(),
            Some("full_build" | "incremental_in_place" | "incremental_checkpoint")
        ));
    }
    assert!(ann.checkpoint_timers.lock().unwrap().is_empty());
    drop(
        CheckpointClaim::take(&ann, &key).expect("quiet attempt releases its single-flight claim"),
    );
}

#[tokio::test]
async fn quiet_defers_real_checkpoint_then_resumes() {
    if quiet_checkpoint_child() {
        return;
    }
    WallClock::new().wait(quiet_checkpoint_fixture(false)).await;
}

#[tokio::test]
async fn quiet_checkpoint_shutdown_releases_claim_without_phase() {
    if quiet_checkpoint_child() {
        return;
    }
    WallClock::new().wait(quiet_checkpoint_fixture(true)).await;
}
