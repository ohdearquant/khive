use async_trait::async_trait;
use khive_storage::event::{
    EventGroupBy, EventPageQuery, EventPageWindow, IdempotentEventBatchResult,
};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use serde_json::Value;
use serial_test::serial;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use uuid::Uuid;

const WATCHDOG: Duration = Duration::from_secs(60);

/// Wrap the bound runtime's actual raw sink; successful writes and reads still
/// pass through the real EventStore. Preflights count logical submissions,
/// while attempts/refusals count physical batches separately.
pub(super) struct ControlledStore {
    inner: Arc<dyn EventStore>,
    submitted: Mutex<Vec<Event>>,
    attempts: Mutex<Vec<Event>>,
    refused: Mutex<Vec<Uuid>>,
    reject: AtomicU8,
    hold_next: AtomicBool,
    entered: Notify,
    release: Notify,
    timed_out: AtomicBool,
    timeout_seen: Notify,
}

pub(super) struct ReleaseGate(Arc<ControlledStore>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}

impl ControlledStore {
    fn new(inner: Arc<dyn EventStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            submitted: Mutex::new(Vec::new()),
            attempts: Mutex::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
            reject: AtomicU8::new(0),
            hold_next: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            timed_out: AtomicBool::new(false),
            timeout_seen: Notify::new(),
        })
    }

    pub(super) fn refuse_config(&self) {
        self.reject.store(1, Ordering::SeqCst);
    }

    pub(super) fn refuse_all(&self) {
        self.reject.store(2, Ordering::SeqCst);
    }

    pub(super) fn recover(&self) {
        self.reject.store(0, Ordering::SeqCst);
    }

    fn gate_next(self: &Arc<Self>) -> ReleaseGate {
        assert!(!self.hold_next.swap(true, Ordering::SeqCst));
        ReleaseGate(self.clone())
    }

    async fn await_gate(&self) {
        tokio::time::timeout(WATCHDOG, self.entered.notified())
            .await
            .expect("actual ConfigLocked append entered gate");
    }

    fn submitted(&self, key: &str) -> Vec<Event> {
        self.submitted
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.payload["key"] == key)
            .cloned()
            .collect()
    }

    fn attempts(&self, key: &str) -> Vec<Event> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.kind == EventKind::ConfigLocked && event.payload["key"] == key)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl EventStore for ControlledStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
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
        self.inner.preflight_event(event)?;
        if event.kind == EventKind::ConfigLocked {
            self.submitted.lock().unwrap().push(event.clone());
        }
        Ok(())
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        self.inner.supports_idempotent_audit_batch()
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let config = events
            .iter()
            .any(|event| event.kind == EventKind::ConfigLocked);
        self.attempts.lock().unwrap().extend(events.iter().cloned());
        if config && self.hold_next.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let mode = self.reject.load(Ordering::SeqCst);
        if mode == 2 || (mode == 1 && config) {
            self.refused
                .lock()
                .unwrap()
                .extend(events.iter().map(|event| event.id));
            return Err(StorageError::Internal(
                "test nonretryable config generation refusal".into(),
            ));
        }
        self.inner.append_events_idempotent(events).await
    }
}

struct TimeoutFields(BTreeMap<String, String>);
impl tracing::field::Visit for TimeoutFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

struct TimeoutCapture(Arc<ControlledStore>);
impl tracing::Subscriber for TimeoutCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut fields = TimeoutFields(BTreeMap::new());
        event.record(&mut fields);
        if fields.0.get("verb").map(String::as_str) == Some("config.lock")
            && fields.0.get("reason").map(String::as_str) == Some("AdmissionDeadlineExpired")
        {
            self.0.timed_out.store(true, Ordering::SeqCst);
            self.0.timeout_seen.notify_one();
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

async fn capture_timeout<F: std::future::Future>(
    store: Arc<ControlledStore>,
    future: F,
) -> F::Output {
    let dispatch = tracing::Dispatch::new(TimeoutCapture(store));
    let mut future = Box::pin(future);
    std::future::poll_fn(|cx| {
        tracing::dispatcher::with_default(&dispatch, || {
            std::future::Future::poll(future.as_mut(), cx)
        })
    })
    .await
}
