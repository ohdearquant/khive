//! Physical-close coverage using the controlled append gates from
//! tests/read_verb_admission_exhaustion.rs, without changing terminal outcomes.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use khive_storage::event::IdempotentEventBatchResult;
use khive_storage::{BatchWriteSummary, EventFilter, Page, PageRequest, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use tokio::sync::Notify;

#[derive(Default)]
struct ControlledStore {
    started: Notify,
    release: Notify,
    resource_released: Arc<AtomicBool>,
    rows: Mutex<Vec<Event>>,
    fail: bool,
}

struct AppendResource(Arc<AtomicBool>);

impl Drop for AppendResource {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl EventStore for ControlledStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.rows.lock().push(event);
        Ok(())
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let n = events.len() as u64;
        self.rows.lock().extend(events);
        Ok(BatchWriteSummary {
            attempted: n,
            affected: n,
            ..BatchWriteSummary::default()
        })
    }

    async fn get_event(&self, id: uuid::Uuid) -> StorageResult<Option<Event>> {
        Ok(self
            .rows
            .lock()
            .iter()
            .find(|event| event.id == id)
            .cloned())
    }

    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        unimplemented!("physical close does not query events")
    }

    async fn count_events(&self, _filter: EventFilter) -> StorageResult<u64> {
        Ok(self.rows.lock().len() as u64)
    }

    fn preflight_event(&self, _event: &Event) -> StorageResult<()> {
        Ok(())
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let _resource = AppendResource(Arc::clone(&self.resource_released));
        self.started.notify_one();
        self.release.notified().await;
        if self.fail {
            return Err(StorageError::Internal("controlled append failure".into()));
        }
        let mut stored = self.rows.lock();
        let mut rows = Vec::new();
        for event in events {
            if let Some(existing) = stored.iter().find(|existing| existing.id == event.id) {
                rows.push(if *existing == event {
                    EventAppendDisposition::AlreadyPresentIdentical
                } else {
                    EventAppendDisposition::IdentityConflict
                });
            } else {
                stored.push(event);
                rows.push(EventAppendDisposition::Inserted);
            }
        }
        Ok(IdempotentEventBatchResult { rows })
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

fn row() -> PreparedAuditRow {
    PreparedAuditRow {
        event: Event::new(
            "local",
            "memory.recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "test:physical-close",
        )
        .with_outcome(EventOutcome::Success),
        producer: AuditProducer::RecallExecuted,
    }
}

fn batch(store: &Arc<ControlledStore>) -> Arc<AuditBatch> {
    AuditBatch::new(
        store.clone(),
        AuditBatchConfig {
            admission_deadline: Duration::from_millis(10),
            resolution_deadline: Duration::from_millis(10),
            max_commit_attempts: std::num::NonZeroU8::new(1).unwrap(),
            ..AuditBatchConfig::default()
        },
    )
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("controlled physical-close fixture must finish")
}

async fn abandoned(fail: bool) -> (Arc<AuditBatch>, Arc<ControlledStore>) {
    let store = Arc::new(ControlledStore {
        fail,
        ..ControlledStore::default()
    });
    let batch = batch(&store);
    let submitted = Arc::clone(&batch);
    let submit = tokio::spawn(async move { submitted.submit(row()).await });
    bounded(store.started.notified()).await;
    assert_eq!(
        bounded(submit).await.unwrap(),
        Err(AuditTerminalReason::AdmissionDeadlineExpired)
    );
    // Existing terminal close must still complete while the accepted append is
    // held: the new physical barrier must not silently strengthen this API.
    assert_eq!(bounded(batch.close_and_drain()).await, Ok(()));
    let snapshot = batch.test_snapshot();
    assert_eq!(snapshot.outstanding_abandoned_appends, 1);
    assert_eq!(snapshot.per_generation.len(), 1);
    assert_eq!(
        snapshot.per_generation[0].terminal_reason,
        Some(AuditTerminalReason::DriverAppendAbandoned)
    );
    assert!(!store.resource_released.load(Ordering::SeqCst));
    (batch, store)
}

async fn assert_physical_close_pending(batch: &AuditBatch) {
    assert!(
        tokio::time::timeout(
            Duration::from_millis(5),
            batch.close_and_wait_for_physical_completion(),
        )
        .await
        .is_err(),
        "physical close must wait for the held append/task to release its resources"
    );
}

fn assert_abandoned_accounting(batch: &AuditBatch, failed: bool) {
    let snapshot = batch.test_snapshot();
    assert_eq!(snapshot.outstanding_abandoned_appends, 0);
    assert_eq!(snapshot.committed_rows, 0);
    let metrics = batch.health_metrics();
    assert_eq!(metrics.flush_failures, 1);
    assert_eq!(metrics.degraded_rows, 1);
    assert!(metrics.degraded);
    assert_eq!(metrics.late_append_commits, u64::from(!failed));
    assert_eq!(metrics.late_append_failures, u64::from(failed));
}

#[tokio::test(start_paused = true)]
async fn physical_close_waits_for_abandoned_append_and_keeps_late_commit_accounting() {
    let (batch, store) = abandoned(false).await;
    assert_physical_close_pending(&batch).await;
    assert_eq!(
        batch.submit(row()).await,
        Err(AuditTerminalReason::AdmissionClosed)
    );
    assert_eq!(store.rows.lock().len(), 0);
    store.release.notify_one();
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert!(store.resource_released.load(Ordering::SeqCst));
    assert_eq!(store.rows.lock().len(), 1);
    assert_abandoned_accounting(&batch, false);
}

#[tokio::test(start_paused = true)]
async fn physical_close_keeps_late_append_failure_accounting() {
    let (batch, store) = abandoned(true).await;
    assert_physical_close_pending(&batch).await;
    store.release.notify_one();
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert!(store.resource_released.load(Ordering::SeqCst));
    assert_eq!(store.rows.lock().len(), 0);
    assert_abandoned_accounting(&batch, true);
}

#[tokio::test(start_paused = true)]
async fn cancelled_physical_close_resumes_without_losing_task_completion() {
    let (batch, store) = abandoned(false).await;
    // Each timeout drops the shutdown future, while the batch must retain the
    // same in-flight append and completion witnesses across both attempts.
    assert_physical_close_pending(&batch).await;
    assert_physical_close_pending(&batch).await;
    assert!(!store.resource_released.load(Ordering::SeqCst));
    assert_eq!(batch.test_snapshot().outstanding_abandoned_appends, 1);
    store.release.notify_one();
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert!(store.resource_released.load(Ordering::SeqCst));
    assert_abandoned_accounting(&batch, false);
}

#[tokio::test(start_paused = true)]
async fn physical_close_normal_commit_is_idempotent_and_refuses_new_admission() {
    let store = Arc::new(ControlledStore::default());
    let batch = batch(&store);
    store.release.notify_one();
    assert_eq!(
        bounded(batch.submit(row())).await,
        Ok(AuditCommitOutcome::Committed)
    );
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert!(store.resource_released.load(Ordering::SeqCst));
    assert_eq!(store.rows.lock().len(), 1);
    assert_eq!(batch.test_snapshot().committed_rows, 1);
    assert_eq!(
        batch.submit(row()).await,
        Err(AuditTerminalReason::AdmissionClosed)
    );
    let metrics = batch.health_metrics();
    assert_eq!(metrics.flush_failures, 0);
    assert_eq!(metrics.degraded_rows, 0);
    assert!(!metrics.degraded);
    assert_eq!(metrics.late_append_commits, 0);
    assert_eq!(metrics.late_append_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn physical_close_waits_for_tokio_completion_after_reservation_release() {
    let store = Arc::new(ControlledStore::default());
    let batch = batch(&store);
    let reservation = batch.inner.reserve_physical();
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_task = Arc::clone(&reached);
    let release_task = Arc::clone(&release);
    let resource_released = Arc::new(AtomicBool::new(false));
    let task_resource = Arc::clone(&resource_released);
    let task = tokio::spawn(async move {
        let _resource = AppendResource(task_resource);
        drop(reservation);
        reached_task.notify_one();
        release_task.notified().await;
    });
    batch.inner.physical_tasks.lock().push(task.abort_handle());
    bounded(reached.notified()).await;
    assert_eq!(batch.inner.state.lock().physical_reservations, 0);
    assert!(!task.is_finished());
    assert_physical_close_pending(&batch).await;
    assert!(!resource_released.load(Ordering::SeqCst));
    release.notify_one();
    assert_eq!(
        bounded(batch.close_and_wait_for_physical_completion()).await,
        Ok(())
    );
    assert!(task.is_finished());
    assert!(resource_released.load(Ordering::SeqCst));
    bounded(task).await.unwrap();
}
