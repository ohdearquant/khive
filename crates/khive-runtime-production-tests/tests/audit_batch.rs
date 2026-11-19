use std::num::{NonZeroU8, NonZeroUsize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_runtime::audit_batch::{
    AuditBatch, AuditBatchConfig, AuditBatchControl, AuditBatchHealthMetrics, AuditCommitOutcome,
    AuditProducer, AuditTerminalReason, PreparedAuditRow,
};
use khive_storage::event::{EventAppendDisposition, IdempotentEventBatchResult};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::{
    Event, EventFilter, EventStore, StorageCapability, StorageError, StorageResult,
};
use khive_types::{EventKind, SubstrateKind};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

struct AppendCall {
    events: Vec<Event>,
    reply: oneshot::Sender<StorageResult<IdempotentEventBatchResult>>,
}

struct ScriptedStore {
    entered: mpsc::UnboundedSender<AppendCall>,
    finished: mpsc::UnboundedSender<()>,
    calls: AtomicUsize,
}

fn scripted_store() -> (
    Arc<ScriptedStore>,
    mpsc::UnboundedReceiver<AppendCall>,
    mpsc::UnboundedReceiver<()>,
) {
    let (entered, calls) = mpsc::unbounded_channel();
    let (finished, completions) = mpsc::unbounded_channel();
    (
        Arc::new(ScriptedStore {
            entered,
            finished,
            calls: AtomicUsize::new(0),
        }),
        calls,
        completions,
    )
}

fn unsupported<T>(operation: &'static str) -> StorageResult<T> {
    Err(StorageError::Unsupported {
        capability: StorageCapability::Events,
        operation: operation.into(),
        message: "audit test requires the idempotent batch route".into(),
    })
}

#[async_trait]
impl EventStore for ScriptedStore {
    async fn append_event(&self, _event: Event) -> StorageResult<()> {
        unsupported("append_event")
    }

    async fn append_events(&self, _events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        unsupported("append_events")
    }

    async fn get_event(&self, _id: Uuid) -> StorageResult<Option<Event>> {
        unsupported("get_event")
    }

    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        unsupported("query_events")
    }

    async fn count_events(&self, _filter: EventFilter) -> StorageResult<u64> {
        unsupported("count_events")
    }

    fn preflight_event(&self, _event: &Event) -> StorageResult<()> {
        Ok(())
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (reply, response) = oneshot::channel();
        assert!(self.entered.send(AppendCall { events, reply }).is_ok());
        let result = response.await.expect("the test resolves every append");
        let _ = self.finished.send(());
        result
    }
}

fn config() -> AuditBatchConfig {
    AuditBatchConfig {
        max_pending_rows: NonZeroUsize::new(16).unwrap(),
        max_rows_per_generation: NonZeroUsize::new(8).unwrap(),
        max_commit_attempts: NonZeroU8::new(1).unwrap(),
        retry_backoff: Duration::ZERO,
        admission_deadline: Duration::from_secs(1),
        resolution_deadline: Duration::from_secs(1),
        max_abandoned_appends: NonZeroUsize::new(1).unwrap(),
    }
}

fn row(id: u128, producer: AuditProducer) -> PreparedAuditRow {
    let mut event = Event::new(
        "local",
        format!("ordinary.audit.{id}"),
        EventKind::Audit,
        SubstrateKind::Event,
        "test:ordinary-audit-consumer",
    );
    event.id = Uuid::from_u128(id);
    event.created_at = 1;
    PreparedAuditRow { event, producer }
}

fn assert_health(batch: &AuditBatch, flush_failures: u64, degraded_rows: u64, degraded: bool) {
    assert_eq!(
        batch.health_metrics(),
        AuditBatchHealthMetrics {
            flush_failures,
            degraded_rows,
            degraded,
            late_append_commits: 0,
            late_append_failures: 0,
        }
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn committed_generation_resolves_all_public_waiters() {
    let (store, mut calls, _completions) = scripted_store();
    let batch = AuditBatch::new(store.clone(), config());
    let first = row(1, AuditProducer::DispatchSucceeded);
    let second = row(2, AuditProducer::RecallExecuted);
    let expected = vec![first.event.clone(), second.event.clone()];
    let (first_result, second_result, ()) = tokio::join!(
        biased;
        batch.submit(first),
        batch.submit(second),
        async {
            let call = calls.recv().await.expect("generation reaches the store");
            assert_eq!(call.events, expected);
            call.reply.send(Ok(IdempotentEventBatchResult {
                rows: vec![EventAppendDisposition::Inserted, EventAppendDisposition::AlreadyPresentIdentical],
            })).expect("both waiters remain live");
        },
    );
    assert_eq!(first_result, Ok(AuditCommitOutcome::Committed));
    assert_eq!(
        second_result,
        Ok(AuditCommitOutcome::AlreadyPresentIdentical)
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    assert_health(&batch, 0, 0, false);
    batch.close_and_drain().await.unwrap();
    assert_eq!(
        batch.submit(row(3, AuditProducer::DispatchSucceeded)).await,
        Err(AuditTerminalReason::AdmissionClosed),
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    assert_health(&batch, 0, 0, false);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_generation_resolves_all_public_waiters_and_health() {
    let (store, mut calls, _completions) = scripted_store();
    let batch = AuditBatch::new(store.clone(), config());
    let first = row(10, AuditProducer::GateDenied);
    let second = row(11, AuditProducer::RecallExecuted);
    let third = row(12, AuditProducer::ConfigLocked);
    let expected = vec![
        first.event.clone(),
        second.event.clone(),
        third.event.clone(),
    ];
    let (first_result, second_result, third_result, ()) = tokio::join!(
        biased;
        batch.submit(first),
        batch.submit(second),
        batch.submit(third),
        async {
            let call = calls.recv().await.expect("generation reaches the store");
            assert_eq!(call.events, expected);
            call.reply.send(Err(StorageError::InvalidInput {
                capability: StorageCapability::Events,
                operation: "append_events_idempotent".into(),
                message: "terminal scripted store failure".into(),
            })).expect("all waiters remain live");
        },
    );
    assert_eq!(first_result, Err(AuditTerminalReason::StoreFailure));
    assert_eq!(second_result, Err(AuditTerminalReason::StoreFailure));
    assert_eq!(third_result, Err(AuditTerminalReason::StoreFailure));
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    assert_health(&batch, 1, 2, true);
    batch.close_and_drain().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn ordinary_build_counts_abandoned_and_shed_generations() {
    let (store, mut calls, mut completions) = scripted_store();
    let config = config();
    let admission_deadline = config.admission_deadline;
    let driver_bound = config.resolution_deadline.saturating_mul(3);
    let batch = AuditBatch::new(store.clone(), config);
    let first = row(20, AuditProducer::DispatchSucceeded);
    let second = row(21, AuditProducer::GateDenied);
    let expected = vec![first.event.clone(), second.event.clone()];
    let (first_result, second_result, held) = tokio::join!(
        biased;
        batch.submit(first),
        batch.submit(second),
        async {
            let call = calls.recv().await.expect("blocked generation reaches the store");
            assert_eq!(call.events, expected);
            tokio::time::advance(admission_deadline).await;
            call
        },
    );
    assert_eq!(
        first_result,
        Err(AuditTerminalReason::AdmissionDeadlineExpired)
    );
    assert_eq!(
        second_result,
        Err(AuditTerminalReason::AdmissionDeadlineExpired)
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    assert_health(&batch, 0, 0, false);

    tokio::time::advance(driver_bound).await;
    batch.quiesce().await.unwrap();
    assert_health(&batch, 1, 0, false);

    let (third_result, fourth_result) = tokio::join!(
        biased;
        batch.submit(row(22, AuditProducer::DispatchSucceeded)),
        batch.submit(row(23, AuditProducer::GateDenied)),
    );
    assert_eq!(third_result, Err(AuditTerminalReason::StoreWedged));
    assert_eq!(fourth_result, Err(AuditTerminalReason::StoreWedged));
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    assert_health(&batch, 2, 0, false);

    let (fifth_result, sixth_result) = tokio::join!(
        biased;
        batch.submit(row(24, AuditProducer::RecallExecuted)),
        batch.submit(row(25, AuditProducer::ConfigLocked)),
    );
    assert_eq!(fifth_result, Err(AuditTerminalReason::StoreWedged));
    assert_eq!(sixth_result, Err(AuditTerminalReason::StoreWedged));
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    assert_health(&batch, 3, 2, true);

    held.reply
        .send(Ok(IdempotentEventBatchResult {
            rows: vec![EventAppendDisposition::Inserted; 2],
        }))
        .expect("release the one abandoned append");
    completions
        .recv()
        .await
        .expect("the abandoned store call returns");
    batch.close_and_drain().await.unwrap();
}
