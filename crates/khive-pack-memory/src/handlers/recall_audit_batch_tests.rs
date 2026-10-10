//! Real recall producers on their registry's ADR-133 queue. Native execution
//! remains a separate acceptance gate; the static packet is not test evidence.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::audit_batch::{
    AuditBatch, AuditBatchConfig, AuditBatchControl, AuditProducer, PreparedAuditRow,
};
use khive_runtime::{
    KhiveRuntime, Namespace, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::event::{
    EventAppendDisposition, EventGroupBy, EventPageQuery, EventPageWindow,
    IdempotentEventBatchResult,
};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageCapability,
    StorageError, StorageResult, WriterTaskRequestState,
};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};
use serial_test::serial;
use tokio::sync::Notify;
use tracing::field::{Field, Visit};
use uuid::Uuid;

use super::{emit_recall_executed_event, RecallExecutedFields, ServeAttribution};
use crate::MemoryPack;

const REQUEST_NS: &str = "recall-audit-request";
const MODEL: &str = "recall-audit-4952-model";
const WATCHDOG: Duration = Duration::from_secs(5);

/// This wraps the real SQLite sink without a token decorator. It records the
/// actual batch rows and delegates the actual write/identity/observation logic.
struct SpyStore {
    inner: Arc<dyn EventStore>,
    batches: Mutex<Vec<Vec<Event>>>,
    dispositions: Mutex<Vec<Vec<EventAppendDisposition>>>,
    direct: AtomicUsize,
    reject_recall: AtomicBool,
    fail_recall_once: AtomicBool,
    ambiguous_recall_once: AtomicBool,
    hold_next: AtomicBool,
    entered: Notify,
    release: Notify,
}

struct ReleaseGate(Arc<SpyStore>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}

impl SpyStore {
    fn new(inner: Arc<dyn EventStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            batches: Mutex::new(Vec::new()),
            dispositions: Mutex::new(Vec::new()),
            direct: AtomicUsize::new(0),
            reject_recall: AtomicBool::new(false),
            fail_recall_once: AtomicBool::new(false),
            ambiguous_recall_once: AtomicBool::new(false),
            hold_next: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }

    fn gate_next(self: &Arc<Self>) -> ReleaseGate {
        assert!(!self.hold_next.swap(true, Ordering::SeqCst));
        ReleaseGate(self.clone())
    }

    async fn await_gate(&self) {
        tokio::time::timeout(WATCHDOG, self.entered.notified())
            .await
            .expect("actual append entered gate");
    }

    fn recall_attempts(&self, query: &str) -> Vec<Event> {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter(|e| e.kind == EventKind::RecallExecuted && e.payload["query"] == query)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl EventStore for SpyStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.direct.fetch_add(1, Ordering::SeqCst);
        self.inner.append_event(event).await
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.inner.append_events(events).await
    }
    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }
    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.inner.query_events(filter, page).await
    }
    async fn query_event_page(&self, query: EventPageQuery) -> StorageResult<EventPageWindow> {
        self.inner.query_event_page(query).await
    }
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(filter).await
    }
    async fn count_events_grouped(
        &self,
        filter: EventFilter,
        group_by: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        self.inner.count_events_grouped(filter, group_by).await
    }
    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        if event.kind == EventKind::RecallExecuted && self.reject_recall.load(Ordering::SeqCst) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Events,
                operation: "preflight_event".into(),
                message: "test refuses only recall telemetry".into(),
            });
        }
        self.inner.preflight_event(event)
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        self.inner.supports_idempotent_audit_batch()
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let contains_recall = events.iter().any(|e| e.kind == EventKind::RecallExecuted);
        self.batches.lock().unwrap().push(events.clone());
        if self.hold_next.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if contains_recall && self.fail_recall_once.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Internal(
                "test nonretryable recall generation failure".into(),
            ));
        }
        let result = self.inner.append_events_idempotent(events).await?;
        self.dispositions.lock().unwrap().push(result.rows.clone());
        if contains_recall && self.ambiguous_recall_once.swap(false, Ordering::SeqCst) {
            // Persist first, then lose the acknowledgement. The actual SQLite
            // idempotency comparison must recognize the unchanged retry.
            return Err(StorageError::writer_task_terminated(
                WriterTaskRequestState::SideEffectsUnknown,
            ));
        }
        Ok(result)
    }
}

struct Fixture {
    registry: VerbRegistry,
    spy: Arc<SpyStore>,
    runtime: KhiveRuntime,
    ann: crate::ann::SharedAnn,
    // Keep the database directory alive until its runtime/stores are dropped.
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(config: AuditBatchConfig, model: bool, configured: bool) -> Self {
        let dir = tempfile::tempdir().expect("database directory");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(dir.path().join("recall.db")),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".into(), "memory".into()],
            ..RuntimeConfig::default()
        })
        .expect("file-backed runtime");
        if model {
            runtime.register_embedder(crate::test_support::HashVecProvider {
                model_name: MODEL.into(),
                dims: 16,
            });
        }
        let pack = MemoryPack::new(crate::test_support::with_receipt_credentials(
            runtime.clone(),
        ));
        let ann = pack.ann.clone();
        let raw = runtime
            .backend()
            .events_for_namespace("registry-default")
            .expect("raw SQLite sink");
        let spy = SpyStore::new(raw);
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(pack);
        builder
            .with_default_namespace("registry-default")
            .with_actor_id(Some("registry-actor".into()));
        if configured {
            builder.with_event_store(spy.clone());
        }
        builder.with_audit_batch_config(config);
        let registry = builder.build().expect("registry");
        assert!(
            runtime
                .backend()
                .pool()
                .writer_task_handle()
                .expect("writer lookup")
                .is_some(),
            "real writer task required"
        );
        assert_eq!(registry.audit_batch_handle().is_some(), configured);
        Self {
            registry,
            spy,
            runtime,
            ann,
            _dir: dir,
        }
    }

    fn batch(&self) -> Arc<AuditBatch> {
        self.registry
            .audit_batch_handle()
            .expect("actual owning batch")
    }

    async fn recall(&self, query: &str) -> Value {
        self.registry
            .dispatch_with_identity(
                "memory.recall",
                json!({"query":query, "limit":10}),
                Some(RequestIdentity {
                    namespace: REQUEST_NS.into(),
                    actor_id: Some("request-actor".into()),
                    ..RequestIdentity::default()
                }),
            )
            .await
            .expect("real recall dispatch")
    }

    async fn direct_recall(&self, query: &str) -> Value {
        // Isolate the pure producer from dispatch's independent obligation.
        // This is the real handler with the same runtime and owning registry,
        // not a standalone AuditBatch or a hand-built RecallExecuted row.
        let token = self
            .runtime
            .authorize(Namespace::parse(REQUEST_NS).unwrap())
            .unwrap();
        let pack = MemoryPack::new(crate::test_support::with_receipt_credentials(
            self.runtime.clone(),
        ));
        pack.handle_recall(
            &token,
            json!({"query":query,"limit":10}),
            &self.registry,
            None,
        )
        .await
        .expect("handler result survives pure telemetry failure")
    }

    async fn seed(&self, content: &str) -> Uuid {
        let token = self
            .runtime
            .authorize(Namespace::parse(REQUEST_NS).unwrap())
            .unwrap();
        self.runtime
            .create_note(&token, "memory", None, content, Some(0.7), None, vec![])
            .await
            .unwrap()
            .id
    }

    async fn rows(&self, query: &str) -> Vec<Event> {
        let token = self
            .runtime
            .authorize(Namespace::parse(REQUEST_NS).unwrap())
            .unwrap();
        self.runtime
            .events(&token)
            .unwrap()
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::RecallExecuted],
                    ..Default::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items
            .into_iter()
            .filter(|e| e.payload["query"] == query)
            .collect()
    }

    async fn warm(&self) {
        // Keep setup deterministic even when the measured fixture has only
        // one pending slot: warm dispatch/config obligations independently
        // of its background pure row, then reopen pure telemetry admission.
        self.spy.reject_recall.store(true, Ordering::SeqCst);
        assert!(self
            .recall("4952 warm absent violet hazelnut")
            .await
            .as_array()
            .unwrap()
            .is_empty());
        self.settle().await;
        self.spy.reject_recall.store(false, Ordering::SeqCst);
    }

    async fn settle(&self) {
        producers_idle().await;
        if let Some(batch) = self.registry.audit_batch_handle() {
            tokio::time::timeout(WATCHDOG, batch.quiesce())
                .await
                .expect("batch watchdog")
                .expect("batch idle");
        }
    }

    async fn finish(&self) {
        self.settle().await;
        tokio::time::timeout(WATCHDOG, self.registry.shutdown_audit_batch())
            .await
            .expect("close watchdog")
            .expect("accepted rows drained");
    }
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WATCHDOG, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observable-state watchdog");
}

async fn producers_idle() {
    wait_for(|| khive_runtime::background_task_count() == 0).await;
}

fn blocker() -> PreparedAuditRow {
    PreparedAuditRow {
        event: Event::new(
            REQUEST_NS,
            "test.blocker",
            EventKind::Audit,
            SubstrateKind::Event,
            "actor:test",
        ),
        producer: AuditProducer::DispatchSucceeded,
    }
}

fn fields(query: &str, targets: Vec<String>) -> RecallExecutedFields {
    RecallExecutedFields {
        actor: "caller payload actor".into(),
        served_by_profile_id: None,
        serve_attribution: ServeAttribution::Unspecified,
        query_raw: query.into(),
        query_class: khive_brain_core::compute_query_class(query),
        target_ids: targets,
        latency_us: 17,
        ann_degraded: false,
        ann_degraded_reason: None,
    }
}

#[derive(Default)]
struct WarningVisitor(HashMap<String, String>);
impl Visit for WarningVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().into(),
            format!("{value:?}").trim_matches('"').into(),
        );
    }
}
struct Capture(Arc<Mutex<Vec<HashMap<String, String>>>>);
impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            let mut v = WarningVisitor::default();
            event.record(&mut v);
            self.0.lock().unwrap().push(v.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
fn capture() -> (
    Arc<Mutex<Vec<HashMap<String, String>>>>,
    tracing::dispatcher::DefaultGuard,
) {
    let rows = Arc::new(Mutex::new(Vec::new()));
    let dispatch = tracing::Dispatch::new(Capture(rows.clone()));
    let guard = tracing::dispatcher::set_default(&dispatch);
    (rows, guard)
}
fn assert_reason(rows: &Arc<Mutex<Vec<HashMap<String, String>>>>, reason: &str) {
    let rows = rows.lock().unwrap();
    let emitter: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.get("message")
                .is_some_and(|m| m.contains("recall_executed batch submission failed"))
        })
        .collect();
    assert_eq!(
        emitter.len(),
        1,
        "one best-effort emitter warning: {rows:?}"
    );
    assert_eq!(emitter[0].get("reason").map(String::as_str), Some(reason));
    assert_eq!(
        emitter[0].get("event_kind").map(String::as_str),
        Some("recall_executed")
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn actual_registry_batches_hit_and_miss_with_request_attribution() {
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    let query = "copper orchid precise payload witness";
    let id = f.seed(query).await;
    assert_eq!(f.recall(query).await.as_array().unwrap().len(), 1);
    f.settle().await;
    let rows = f.rows(query).await;
    assert_eq!(rows.len(), 1);
    let event = &rows[0];
    let attempted = f.spy.recall_attempts(query);
    assert_eq!(attempted, rows);
    assert_eq!(event.namespace, REQUEST_NS);
    assert_eq!(event.actor, "actor:request-actor");
    assert_eq!(event.verb, "memory.recall");
    assert_eq!(event.substrate, SubstrateKind::Event);
    assert_eq!(event.payload["actor"], "actor:request-actor");
    assert_eq!(event.payload["selected"], json!([id.to_string()]));
    assert_eq!(event.payload["candidates"], event.payload["selected"]);
    assert_eq!(event.payload["query"], query);
    assert_eq!(
        event.payload["query_class"],
        khive_brain_core::compute_query_class(query)
    );
    assert_eq!(event.payload["result_kind"], "note");
    assert_eq!(event.payload["result_count"], 1);
    assert_eq!(event.payload["served_by_profile_id"], Value::Null);
    assert_eq!(
        event.payload["serve_attribution"],
        json!(ServeAttribution::Unspecified)
    );
    assert_eq!(event.payload["degraded"], false);
    assert!(event.payload.get("degraded_reason").is_none());
    assert_eq!(event.payload["latency_us"], event.duration_us);
    assert!(event.created_at > 0);
    assert_eq!(event.op_index, None);
    assert_eq!(event.ref_resolution, None);
    let miss = "unseeded quartz dahlia";
    assert!(f.recall(miss).await.as_array().unwrap().is_empty());
    f.settle().await;
    let rows = f.rows(miss).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].payload["selected"], json!([]));
    assert_eq!(rows[0].payload["result_count"], 0);
    assert_eq!(f.spy.recall_attempts(miss), rows);
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn actual_degraded_recall_uses_batch_and_preserves_reason() {
    let f = Fixture::new(AuditBatchConfig::default(), true, true);
    let query = "degraded fts violet orchard witness";
    let id = f.seed(query).await;
    let key = crate::ann::AnnKey::new(MODEL);
    let held = crate::ann::hold_model_warm_lock_for_test(&f.ann, &key).await;
    let value = f
        .registry
        .dispatch_with_identity(
            "memory.recall",
            json!({
                "query": query,
                "limit": 10,
                "config": {"ann_ready_timeout_ms": 25, "recall_deadline_ms": 1000}
            }),
            Some(RequestIdentity {
                namespace: REQUEST_NS.into(),
                actor_id: Some("request-actor".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(value.as_array().unwrap()[0]["degraded"], "ann_unavailable");
    let empty_query = "unseeded onyx cypress";
    let empty = f
        .registry
        .dispatch_with_identity(
            "memory.recall",
            json!({
                "query": empty_query,
                "limit": 10,
                "config": {"ann_ready_timeout_ms": 25, "recall_deadline_ms": 1000}
            }),
            Some(RequestIdentity {
                namespace: REQUEST_NS.into(),
                actor_id: Some("request-actor".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert!(empty["results"].as_array().unwrap().is_empty());
    assert_eq!(empty["degraded"], true);
    drop(held);
    f.settle().await;
    let empty_rows = f.rows(empty_query).await;
    assert_eq!(empty_rows.len(), 1);
    assert_eq!(f.spy.recall_attempts(empty_query), empty_rows);
    assert_eq!(
        empty_rows[0].payload["degraded_reason"],
        empty["degraded_reason"]
    );
    assert_eq!(empty_rows[0].payload["selected"], json!([]));
    assert_eq!(empty_rows[0].payload["degraded"], true);
    let rows = f.rows(query).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(f.spy.recall_attempts(query), rows);
    assert_eq!(rows[0].payload["selected"], json!([id.to_string()]));
    assert_eq!(rows[0].payload["degraded"], true);
    assert!(rows[0].payload["degraded_reason"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert_eq!(rows[0].actor, "actor:request-actor");
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn real_recalls_share_actual_queue_and_one_writer_generation() {
    const N: usize = 4;
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    f.warm().await;
    let batch = f.batch();
    assert!(Arc::ptr_eq(
        &batch,
        &f.registry.clone().audit_batch_handle().unwrap()
    ));
    let before = batch.test_snapshot();
    let acquisitions = f
        .runtime
        .backend()
        .pool()
        .writer_acquisition_snapshot()
        .writer_task_acquisitions;
    let gate = f.spy.gate_next();
    let owner = batch.clone();
    let sentinel = tokio::spawn(async move { owner.submit(blocker()).await });
    f.spy.await_gate().await;
    let mut calls = Vec::new();
    for i in 0..N {
        let registry = f.registry.clone();
        calls.push(tokio::spawn(async move {
            registry
                .dispatch_with_identity(
                    "memory.recall",
                    json!({"query":format!("absent amber telemetry {i}"),"limit":10}),
                    Some(RequestIdentity {
                        namespace: REQUEST_NS.into(),
                        actor_id: Some("request-actor".into()),
                        ..Default::default()
                    }),
                )
                .await
        }));
    }
    // Actual accepted state on the SAME registry batch. A preflight callback
    // or delay cannot prove the two producer families reached this queue.
    wait_for(|| batch.test_snapshot().pending_rows == 2 * N).await;
    let queued = batch.test_snapshot();
    assert_eq!(
        queued.submitted_rows - before.submitted_rows,
        1 + (2 * N) as u64
    );
    assert!(queued.in_flight_generation.is_some());
    drop(gate);
    sentinel.await.unwrap().unwrap();
    for call in calls {
        assert!(call.await.unwrap().unwrap().as_array().unwrap().is_empty());
    }
    f.settle().await;
    let after = batch.test_snapshot();
    assert_eq!(after.store_batch_calls - before.store_batch_calls, 2);
    assert_eq!(
        after.committed_rows - before.committed_rows,
        1 + (2 * N) as u64
    );
    let generations = f.spy.batches.lock().unwrap();
    let combined = generations.last().unwrap();
    assert_eq!(combined.len(), 2 * N);
    assert_eq!(
        combined
            .iter()
            .filter(|e| e.kind == EventKind::RecallExecuted)
            .count(),
        N
    );
    assert_eq!(
        combined
            .iter()
            .filter(|e| e.kind == EventKind::Audit)
            .count(),
        N
    );
    drop(generations);
    let after_acquisitions = f
        .runtime
        .backend()
        .pool()
        .writer_acquisition_snapshot()
        .writer_task_acquisitions;
    assert_eq!(
        after_acquisitions - acquisitions,
        2,
        "one sentinel plus one shared real writer acquisition; no per-recall direct append"
    );
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn preflight_refusal_preserves_real_dispatch_without_fallback() {
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    f.warm().await;
    let (warnings, _guard) = capture();
    f.spy.reject_recall.store(true, Ordering::SeqCst);
    let query = "preflight refusal absent witness";
    assert!(f.recall(query).await.as_array().unwrap().is_empty());
    f.settle().await;
    assert_reason(&warnings, "PreflightRejected");
    assert!(f.rows(query).await.is_empty());
    assert!(f.spy.recall_attempts(query).is_empty());
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
    assert_eq!(f.batch().health_metrics().degraded_rows, 0);
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn generation_failure_preserves_handler_and_counts_pure_degradation() {
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    f.warm().await;
    let (warnings, _guard) = capture();
    let before = f.batch().health_metrics().degraded_rows;
    let query = "nonretryable pure generation hit witness";
    f.seed(query).await;
    f.spy.fail_recall_once.store(true, Ordering::SeqCst);
    assert_eq!(f.direct_recall(query).await.as_array().unwrap().len(), 1);
    f.settle().await;
    assert_reason(&warnings, "StoreFailure");
    assert!(f.rows(query).await.is_empty());
    assert_eq!(f.spy.recall_attempts(query).len(), 1);
    assert_eq!(f.batch().health_metrics().degraded_rows, before + 1);
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn closed_batch_refuses_late_real_handler_telemetry_without_fallback() {
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    f.warm().await;
    f.finish().await;
    let (warnings, _guard) = capture();
    let query = "closed admission absent witness";
    assert!(f.direct_recall(query).await.as_array().unwrap().is_empty());
    producers_idle().await;
    assert_reason(&warnings, "AdmissionClosed");
    assert!(f.rows(query).await.is_empty());
    assert!(f.spy.recall_attempts(query).is_empty());
    assert_eq!(f.batch().health_metrics().degraded_rows, 0);
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn full_queue_refuses_real_handler_row_without_extra_writer_or_fallback() {
    let f = Fixture::new(
        AuditBatchConfig {
            max_pending_rows: std::num::NonZeroUsize::new(1).unwrap(),
            ..Default::default()
        },
        false,
        true,
    );
    f.warm().await;
    let gate = f.spy.gate_next();
    let batch = f.batch();
    let owner = batch.clone();
    let sentinel = tokio::spawn(async move { owner.submit(blocker()).await });
    f.spy.await_gate().await;
    let owner = batch.clone();
    let filler = tokio::spawn(async move { owner.submit(blocker()).await });
    wait_for(|| batch.test_snapshot().pending_rows == 1).await;
    let before = batch.test_snapshot().submitted_rows;
    let acquisitions = f
        .runtime
        .backend()
        .pool()
        .writer_acquisition_snapshot()
        .writer_task_acquisitions;
    let (warnings, _guard) = capture();
    let query = "queue exhausted absent witness";
    assert!(f.direct_recall(query).await.as_array().unwrap().is_empty());
    producers_idle().await;
    assert_reason(&warnings, "QueueAdmissionExhausted");
    assert_eq!(batch.test_snapshot().submitted_rows, before);
    assert!(f.rows(query).await.is_empty());
    assert!(f.spy.recall_attempts(query).is_empty());
    assert_eq!(
        f.runtime
            .backend()
            .pool()
            .writer_acquisition_snapshot()
            .writer_task_acquisitions,
        acquisitions
    );
    assert_eq!(batch.health_metrics().degraded_rows, 0);
    drop(gate);
    sentinel.await.unwrap().unwrap();
    filler.await.unwrap().unwrap();
    f.finish().await;
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn timed_out_real_producer_row_commits_later_once_on_same_batch() {
    let f = Fixture::new(
        AuditBatchConfig {
            admission_deadline: Duration::from_millis(25),
            resolution_deadline: Duration::from_secs(5),
            ..Default::default()
        },
        false,
        true,
    );
    f.warm().await;
    let (warnings, _guard) = capture();
    let gate = f.spy.gate_next();
    let query = "accepted deadline absent witness";
    assert!(f.direct_recall(query).await.as_array().unwrap().is_empty());
    f.spy.await_gate().await;
    producers_idle().await;
    assert_reason(&warnings, "AdmissionDeadlineExpired");
    assert!(f.rows(query).await.is_empty());
    assert!(f.batch().test_snapshot().in_flight_generation.is_some());
    let original = f.spy.recall_attempts(query);
    assert_eq!(original.len(), 1);
    drop(gate);
    f.finish().await;
    assert_eq!(f.rows(query).await, original);
    assert_eq!(f.spy.recall_attempts(query).len(), 1);
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn cancelled_emitter_waiter_does_not_remove_or_resubmit_accepted_row() {
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    f.warm().await;
    let gate = f.spy.gate_next();
    let rt = f.runtime.clone();
    let registry = f.registry.clone();
    let token = rt.authorize(Namespace::parse(REQUEST_NS).unwrap()).unwrap();
    let query = "cancelled accepted emitter";
    let task = tokio::spawn(async move {
        emit_recall_executed_event(&rt, &token, &registry, fields(query, vec![])).await
    });
    f.spy.await_gate().await;
    let original = f.spy.recall_attempts(query);
    assert_eq!(original.len(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(gate);
    f.finish().await;
    assert_eq!(f.rows(query).await, original);
    assert_eq!(f.spy.recall_attempts(query).len(), 1);
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn ambiguous_ack_retries_identical_produced_event_and_observations_once() {
    let f = Fixture::new(AuditBatchConfig::default(), false, true);
    let id = f.seed("ambiguous seed orchid").await;
    f.warm().await;
    f.spy.ambiguous_recall_once.store(true, Ordering::SeqCst);
    let query = "ambiguous accepted emitter";
    let token = f
        .runtime
        .authorize(Namespace::parse(REQUEST_NS).unwrap())
        .unwrap();
    emit_recall_executed_event(
        &f.runtime,
        &token,
        &f.registry,
        fields(query, vec![id.to_string()]),
    )
    .await;
    f.finish().await;
    let attempts = f.spy.recall_attempts(query);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    assert_eq!(f.rows(query).await, vec![attempts[0].clone()]);
    let dispositions = f.spy.dispositions.lock().unwrap();
    assert_eq!(
        dispositions.last().unwrap(),
        &vec![EventAppendDisposition::AlreadyPresentIdentical]
    );
    drop(dispositions);
    // Read the actual observation projection after the ambiguous retry, not
    // a fake store's in-memory deduplication imitation.
    let access = f.runtime.sql();
    let mut reader = access.reader().await.unwrap();
    let observed = reader
        .query_all(khive_storage::types::SqlStatement {
            sql: "SELECT role, entity_id, position, referent_kind FROM event_observations \
                  WHERE event_id = ?1 ORDER BY role, position"
                .into(),
            params: vec![khive_storage::types::SqlValue::Text(
                attempts[0].id.to_string(),
            )],
            label: Some("test.recall.batch.retry_projection".into()),
        })
        .await
        .unwrap();
    assert_eq!(observed.len(), 2);
    for (row, role) in observed.iter().zip(["candidate", "selected"]) {
        assert_eq!(row.text("role").unwrap(), role);
        assert_eq!(row.text("entity_id").unwrap(), id.to_string());
        assert_eq!(row.i64("position").unwrap(), 0);
        assert_eq!(row.text("referent_kind").unwrap(), "note");
    }
    assert_eq!(attempts[0].payload["actor"], "caller payload actor");
    assert_eq!(
        attempts[0].actor,
        format!("{}:{}", token.actor().kind, token.actor().id)
    );
    assert_eq!(attempts[0].namespace, REQUEST_NS);
    assert_eq!(attempts[0].duration_us, 17);
    assert_eq!(f.spy.direct.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial(background_tasks)]
#[serial(config_ledger)]
async fn no_batch_real_dispatch_retains_original_runtime_append() {
    let f = Fixture::new(AuditBatchConfig::default(), false, false);
    let query = "legacy no batch absent witness";
    assert!(f.recall(query).await.as_array().unwrap().is_empty());
    f.finish().await;
    let rows = f.rows(query).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor, "actor:request-actor");
    assert_eq!(rows[0].payload["result_count"], 0);
    assert!(f.spy.batches.lock().unwrap().is_empty());
    assert!(f.registry.audit_batch_handle().is_none());
}
