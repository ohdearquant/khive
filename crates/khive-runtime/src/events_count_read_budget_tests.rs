use super::events_count_read_budget::{
    count_storage_error_response, restore_count_read_failure, CountReadBudget,
};
use super::forwarder::EventsSplitClient;
use super::paths::{EVENTS_PROTOCOL_VERSION, MAX_INFLIGHT_REQUEST_BYTES, REQUEST_TIMEOUT};
use super::protocol::{EventsRequest, EventsResponse, WireWriterTaskFailure, WireWriterTaskState};
use super::server::{dispatch_events_request, serve_events_conn, NamespaceStores};
use super::stores::{ForwardingEventStore, SplitEventStore};
use crate::daemon::{read_frame, write_frame};
use async_trait::async_trait;
use khive_db::StorageBackend;
use khive_storage::event::{
    EventGroupBy, EventOrderKey, EventPageQuery, EventPageRow, EventPageWindow,
};
use khive_storage::{
    capture_request_read_context, scope_request_read_deadline_at, RequestReadDeadline, SqlAccess,
    SqlStatement, SqlValue, StorageCapability,
};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use uuid::Uuid;

fn count_request(grouped: bool, seconds: Option<u64>) -> EventsRequest {
    if grouped {
        EventsRequest::CountEventsGrouped {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: "local".into(),
            operator_read_budget_secs: seconds,
            filter: EventFilter::default(),
            group_by: EventGroupBy::Verb,
        }
    } else {
        EventsRequest::CountEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: "local".into(),
            operator_read_budget_secs: seconds,
            filter: EventFilter::default(),
        }
    }
}

async fn count(store: &dyn EventStore, grouped: bool) -> StorageResult<u64> {
    if grouped {
        store
            .count_events_grouped(EventFilter::default(), EventGroupBy::Verb)
            .await
            .map(|counts| counts["fixture"])
    } else {
        store.count_events(EventFilter::default()).await
    }
}

async fn drive_until(mut ready: impl FnMut() -> bool) {
    // Yielding keeps paused-clock IPC tests from auto-advancing past an unprocessed socket.
    for _ in 0..16_384 {
        if ready() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("fixture did not reach its next observable phase");
}

async fn yield_ready_tasks() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

struct Peer {
    _dir: tempfile::TempDir,
    client: Arc<EventsSplitClient>,
    received: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Peer {
    fn delayed(seconds: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("events.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let received = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&received);
        let closed = Arc::new(AtomicBool::new(false));
        let saw_close = Arc::clone(&closed);
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let bytes = read_frame(&mut stream).await.unwrap();
            let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let request: EventsRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(wire["protocol_version"], 6);
            let response = match request {
                EventsRequest::CountEvents { .. } => EventsResponse::Count { count: 7 },
                EventsRequest::CountEventsGrouped { .. } => EventsResponse::GroupedCount {
                    counts: BTreeMap::from([("fixture".into(), 7)]),
                },
                EventsRequest::GetEvent { .. } => EventsResponse::Event { event: None },
                EventsRequest::QueryEvents {
                    namespace,
                    filter,
                    page,
                    ..
                } => {
                    assert_eq!(namespace, "local");
                    assert_eq!(
                        serde_json::to_value(filter).unwrap(),
                        serde_json::to_value(query_filter()).unwrap()
                    );
                    assert_eq!(page.offset, 0);
                    assert_eq!(page.limit, 1);
                    EventsResponse::Pageful {
                        page: Page {
                            items: vec![QueryStore::fixture_event()],
                            total: Some(1),
                        },
                    }
                }
                other => panic!("unexpected fixture request: {other:?}"),
            };
            if wire["op"].as_str().unwrap().starts_with("count_") {
                assert_eq!(wire["operator_read_budget_secs"], 30);
            } else {
                assert!(wire.get("operator_read_budget_secs").is_none());
            }
            assert!(!wire.as_object().unwrap().keys().any(|key| {
                key.contains("deadline") || key.contains("clock") || key.contains("remaining")
            }));
            let response_at = tokio::time::Instant::now() + Duration::from_secs(seconds);
            let delay = tokio::time::sleep_until(response_at);
            observed.store(true, Ordering::SeqCst);
            use tokio::io::AsyncReadExt;
            tokio::select! {
                _ = delay => {}
                closed = stream.read_u8() => {
                    assert!(closed.is_err(), "one request per connection");
                    saw_close.store(true, Ordering::SeqCst);
                    return;
                }
            }
            let _ = write_frame(&mut stream, &serde_json::to_vec(&response).unwrap()).await;
        });
        Self {
            client: EventsSplitClient::new(socket).unwrap(),
            _dir: dir,
            received,
            closed,
            task,
        }
    }
}

enum CountWork {
    Delay(u64),
    Sql(Arc<dyn SqlAccess>),
}

struct CountStore {
    work: CountWork,
    started: AtomicUsize,
    finished: AtomicBool,
    deadlines: Mutex<Vec<Option<RequestReadDeadline>>>,
}

impl CountStore {
    fn new(work: CountWork) -> Arc<Self> {
        Arc::new(Self {
            work,
            started: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            deadlines: Mutex::new(Vec::new()),
        })
    }

    async fn run_count(&self) -> StorageResult<u64> {
        self.run_read("fixture.events.count").await
    }

    async fn run_read(&self, operation: &'static str) -> StorageResult<u64> {
        let deadline = capture_request_read_context().deadline();
        self.deadlines.lock().unwrap().push(deadline);
        self.started.fetch_add(1, Ordering::SeqCst);
        let result = match &self.work {
            CountWork::Delay(seconds) => {
                khive_storage::await_request_read_phase(operation, async {
                    tokio::time::sleep(Duration::from_secs(*seconds)).await;
                    7
                })
                .await
            }
            CountWork::Sql(sql) => {
                assert!(deadline.is_none(), "the receiver must not mint a deadline");
                let mut reader = sql.reader().await?;
                reader
                    .count(SqlStatement {
                        sql: "SELECT COUNT(*) FROM events WHERE namespace = ?1".into(),
                        params: vec![SqlValue::Text("local".into())],
                        label: Some("fixture.events.count".into()),
                    })
                    .await
            }
        };
        // This async continuation runs only after the actual reader count future settles.
        // A dropped dispatch cannot set it, even if its blocking worker finishes quickly.
        self.finished.store(true, Ordering::SeqCst);
        result
    }

    fn unsupported() -> StorageError {
        StorageError::Unsupported {
            capability: StorageCapability::Events,
            operation: "count fixture".into(),
            message: "only count reads are supported".into(),
        }
    }
}

#[async_trait]
impl EventStore for CountStore {
    async fn append_event(&self, _: Event) -> StorageResult<()> {
        Err(Self::unsupported())
    }
    async fn append_events(&self, _: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        Err(Self::unsupported())
    }
    async fn get_event(&self, _: Uuid) -> StorageResult<Option<Event>> {
        Err(Self::unsupported())
    }
    async fn query_events(&self, _: EventFilter, _: PageRequest) -> StorageResult<Page<Event>> {
        self.run_read("fixture.events.query_events").await?;
        Ok(Page {
            items: vec![QueryStore::fixture_event()],
            total: Some(1),
        })
    }
    async fn count_events(&self, _: EventFilter) -> StorageResult<u64> {
        self.run_count().await
    }
    async fn count_events_grouped(
        &self,
        _: EventFilter,
        _: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        self.run_count()
            .await
            .map(|value| BTreeMap::from([("fixture".into(), value)]))
    }
}

fn query_filter() -> EventFilter {
    EventFilter {
        actors: vec!["cursor-reader".into()],
        kinds: vec![khive_types::EventKind::Audit],
        after: Some(49),
        before: Some(151),
        ..Default::default()
    }
}

async fn query(store: &dyn EventStore) -> StorageResult<Page<Event>> {
    store
        .query_events(
            query_filter(),
            PageRequest {
                offset: 0,
                limit: 1,
            },
        )
        .await
}

#[tokio::test(start_paused = true)]
async fn query_events_read_budget_crosses_ten_seconds_with_original_page_shape() {
    let peer = Peer::delayed(11);
    let store = ForwardingEventStore::new("local", Arc::clone(&peer.client));
    let task = tokio::spawn(async move { query(&store).await });
    drive_until(|| peer.received.load(Ordering::SeqCst)).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    yield_ready_tasks().await;
    assert!(
        !task.is_finished(),
        "query_events retained the fixed timeout"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    drive_until(|| task.is_finished()).await;
    let page = task.await.unwrap().unwrap();
    assert_eq!(page.total, Some(1));
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, Uuid::from_u128(1));
    assert_eq!(page.items[0].created_at, 100);
    assert_eq!(page.items[0].actor, "cursor-reader");
    peer.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn query_events_read_budget_earlier_deadline_and_cancellation_close_socket() {
    for cancel in [false, true] {
        let peer = Peer::delayed(11);
        let store = ForwardingEventStore::new("local", Arc::clone(&peer.client));
        let (cancellation, receiver) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(scope_request_read_deadline_at(
            RequestReadDeadline::after(Duration::from_secs(5)),
            khive_storage::scope_request_read_cancellation(
                receiver,
                async move { query(&store).await },
            ),
        ));
        drive_until(|| peer.received.load(Ordering::SeqCst)).await;
        tokio::time::advance(Duration::from_secs(if cancel { 1 } else { 5 })).await;
        if cancel {
            cancellation.send(true).unwrap();
        }
        drive_until(|| task.is_finished() && peer.closed.load(Ordering::SeqCst)).await;
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(&error, StorageError::Timeout { operation }
            if operation.contains("query_events") && operation.contains("operator read budget 30s")
                && operation.matches("operator read budget").count() == 1),
            "{error:?}"
        );
        peer.task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn query_events_read_budget_split_legacy_and_lane_never_renew() {
    for remote in [false, true] {
        for lane_seconds in [9, 11] {
            let legacy = CountStore::new(CountWork::Delay(20));
            let local_lane = CountStore::new(CountWork::Delay(lane_seconds));
            let peer = remote.then(|| Peer::delayed(lane_seconds));
            let lane: Arc<dyn EventStore> = match &peer {
                Some(peer) => {
                    Arc::new(ForwardingEventStore::new("local", Arc::clone(&peer.client)))
                }
                None => local_lane.clone(),
            };
            let split = SplitEventStore::new(legacy.clone(), lane);
            let started = tokio::time::Instant::now();
            // No enclosing scope: the split store must establish one for both sources.
            let task = tokio::spawn(async move { query(&split).await });
            drive_until(|| legacy.started.load(Ordering::SeqCst) == 1).await;
            let original = legacy.deadlines.lock().unwrap()[0].expect("split read deadline");
            assert_eq!(original.async_at(), started + Duration::from_secs(30));
            tokio::time::advance(Duration::from_secs(20)).await;
            drive_until(|| {
                peer.as_ref().map_or_else(
                    || local_lane.started.load(Ordering::SeqCst) == 1,
                    |peer| peer.received.load(Ordering::SeqCst),
                )
            })
            .await;
            if !remote {
                let lane_deadline = local_lane.deadlines.lock().unwrap()[0].unwrap();
                assert_eq!(lane_deadline.async_at(), original.async_at());
                assert_eq!(lane_deadline.blocking_at(), original.blocking_at());
            }
            tokio::time::advance(Duration::from_secs(lane_seconds.min(10))).await;
            drive_until(|| task.is_finished()).await;
            let result = task.await.unwrap();
            if lane_seconds == 9 {
                let page = result.unwrap();
                assert_eq!(page.total, Some(2));
                assert_eq!(page.items.len(), 1);
                assert_eq!(page.items[0].id, Uuid::from_u128(1));
            } else {
                assert!(
                    matches!(result, Err(StorageError::Timeout { .. })),
                    "{result:?}"
                );
            }
            if let Some(peer) = peer {
                drive_until(|| peer.task.is_finished()).await;
                peer.task.await.unwrap();
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn query_events_read_budget_legacy_refusal_prevents_lane_start() {
    let legacy = CountStore::new(CountWork::Delay(20));
    let lane = CountStore::new(CountWork::Delay(0));
    let split = SplitEventStore::new(legacy.clone(), lane.clone());
    let task = tokio::spawn(scope_request_read_deadline_at(
        RequestReadDeadline::after(Duration::from_secs(1)),
        async move { query(&split).await },
    ));
    drive_until(|| legacy.started.load(Ordering::SeqCst) == 1).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    drive_until(|| task.is_finished()).await;
    let error = task.await.unwrap().unwrap_err();
    assert!(
        matches!(&error, StorageError::Timeout { operation }
        if operation.starts_with("fixture.events.query_events (operator read budget 30s")
            && operation.matches("operator read budget").count() == 1),
        "{error:?}"
    );
    assert_eq!(lane.started.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn captures_retain_both_instants_of_the_original_earlier_deadline() {
    let original = RequestReadDeadline::after(Duration::from_secs(5));
    scope_request_read_deadline_at(original, async {
        let first = CountReadBudget::capture();
        assert_eq!(first.deadline.async_at(), original.async_at());
        assert_eq!(first.deadline.blocking_at(), original.blocking_at());
        tokio::time::advance(Duration::from_secs(2)).await;
        let nested = CountReadBudget::capture();
        assert_eq!(nested.deadline.async_at(), original.async_at());
        assert_eq!(nested.deadline.blocking_at(), original.blocking_at());
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn scalar_and_grouped_forwarded_counts_cross_ten_seconds_within_original_thirty() {
    // These timing fixtures use the existing 30s setting without changing process environment.
    for grouped in [false, true] {
        let peer = Peer::delayed(11);
        let store = ForwardingEventStore::new("local", Arc::clone(&peer.client));
        let original = RequestReadDeadline::after(Duration::from_secs(30));
        let task = tokio::spawn(scope_request_read_deadline_at(original, async move {
            count(&store, grouped).await
        }));
        drive_until(|| peer.received.load(Ordering::SeqCst)).await;
        tokio::time::advance(Duration::from_secs(10)).await;
        yield_ready_tasks().await;
        assert!(!task.is_finished(), "count still used the fixed 10s path");
        tokio::time::advance(Duration::from_secs(1)).await;
        drive_until(|| task.is_finished()).await;
        assert_eq!(task.await.unwrap().unwrap(), 7);
        peer.task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn original_five_second_caller_deadline_wins_for_both_count_shapes() {
    for grouped in [false, true] {
        let peer = Peer::delayed(11);
        let store = ForwardingEventStore::new("local", Arc::clone(&peer.client));
        let original = RequestReadDeadline::after(Duration::from_secs(5));
        let task = tokio::spawn(scope_request_read_deadline_at(original, async move {
            count(&store, grouped).await
        }));
        drive_until(|| peer.received.load(Ordering::SeqCst)).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        drive_until(|| task.is_finished()).await;
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(&error, StorageError::Timeout { operation }
            if operation.contains("events daemon count round-trip")
                && operation.contains("operator read budget 30s")),
            "{error:?}"
        );
        peer.task.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn split_legacy_twenty_then_lane_nine_or_eleven_never_renews() {
    for grouped in [false, true] {
        for lane_seconds in [9, 11] {
            let peer = Peer::delayed(lane_seconds);
            let legacy = CountStore::new(CountWork::Delay(20));
            let lane = Arc::new(ForwardingEventStore::new("local", Arc::clone(&peer.client)));
            let split = SplitEventStore::new(legacy.clone(), lane);
            let original = RequestReadDeadline::after(Duration::from_secs(30));
            let task = tokio::spawn(scope_request_read_deadline_at(original, async move {
                count(&split, grouped).await
            }));
            drive_until(|| legacy.started.load(Ordering::SeqCst) == 1).await;
            tokio::time::advance(Duration::from_secs(20)).await;
            drive_until(|| peer.received.load(Ordering::SeqCst)).await;
            assert_eq!(
                legacy
                    .deadlines
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|deadline| deadline.map(RequestReadDeadline::async_at))
                    .collect::<Vec<_>>(),
                vec![Some(original.async_at())]
            );
            tokio::time::advance(Duration::from_secs(lane_seconds.min(10))).await;
            drive_until(|| task.is_finished()).await;
            let result = task.await.unwrap();
            if lane_seconds == 9 {
                assert_eq!(result.unwrap(), 14);
                peer.task.await.unwrap();
            } else {
                assert!(matches!(result, Err(StorageError::Timeout { .. })));
                peer.task.abort();
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_timeout_retains_operation_and_one_budget_label_without_starting_lane() {
    let legacy = CountStore::new(CountWork::Delay(20));
    let lane = CountStore::new(CountWork::Delay(0));
    let split = SplitEventStore::new(legacy.clone(), lane.clone());
    let task = tokio::spawn(scope_request_read_deadline_at(
        RequestReadDeadline::after(Duration::from_secs(1)),
        async move { count(&split, false).await },
    ));
    drive_until(|| legacy.started.load(Ordering::SeqCst) == 1).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    drive_until(|| task.is_finished()).await;
    let error = task.await.unwrap().unwrap_err();
    assert!(
        matches!(&error, StorageError::Timeout { operation }
        if operation.starts_with("fixture.events.count (operator read budget 30s")
            && operation.matches("operator read budget").count() == 1),
        "{error:?}"
    );
    assert_eq!(lane.started.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn noncount_round_trip_retains_fixed_ten_second_timeout() {
    assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
    let peer = Peer::delayed(11);
    let client = Arc::clone(&peer.client);
    let task = tokio::spawn(scope_request_read_deadline_at(
        RequestReadDeadline::after(Duration::from_secs(5)),
        async move {
            client
                .round_trip(&EventsRequest::GetEvent {
                    protocol_version: EVENTS_PROTOCOL_VERSION,
                    namespace: "local".into(),
                    id: Uuid::nil(),
                })
                .await
        },
    ));
    drive_until(|| peer.received.load(Ordering::SeqCst)).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    yield_ready_tasks().await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(5)).await;
    drive_until(|| task.is_finished()).await;
    assert!(matches!(
        task.await.unwrap(),
        Err(StorageError::Timeout { .. })
    ));
    peer.task.abort();
}

#[tokio::test]
async fn invalid_count_attribution_and_version_refuse_before_namespace_cache() {
    let backend = StorageBackend::memory().unwrap();
    let stores: NamespaceStores = Arc::new(Mutex::new(std::collections::HashMap::new()));
    assert_eq!(EVENTS_PROTOCOL_VERSION, 6);
    for grouped in [false, true] {
        for seconds in [
            None,
            Some(0),
            Some(khive_storage::request_context::MAX_REQUEST_READ_TIMEOUT_SECS + 1),
        ] {
            let mut wire = serde_json::to_value(count_request(grouped, seconds)).unwrap();
            if seconds.is_none() {
                wire.as_object_mut()
                    .unwrap()
                    .remove("operator_read_budget_secs");
            }
            let response =
                dispatch_events_request(serde_json::from_value(wire).unwrap(), &backend, &stores)
                    .await;
            assert!(matches!(
                response,
                EventsResponse::Error {
                    retryable: false,
                    ..
                }
            ));
            assert!(stores.lock().unwrap().is_empty());
        }
        let mut wire = serde_json::to_value(count_request(grouped, Some(30))).unwrap();
        wire["protocol_version"] = serde_json::json!(5);
        wire.as_object_mut()
            .unwrap()
            .remove("operator_read_budget_secs");
        let response =
            dispatch_events_request(serde_json::from_value(wire).unwrap(), &backend, &stores).await;
        assert!(
            matches!(response, EventsResponse::Error { message, retryable: false, .. }
            if message.contains("protocol version"))
        );
        assert!(stores.lock().unwrap().is_empty());
    }
    for grouped in [false, true] {
        for seconds in [
            1,
            khive_storage::request_context::MAX_REQUEST_READ_TIMEOUT_SECS,
        ] {
            let response =
                dispatch_events_request(count_request(grouped, Some(seconds)), &backend, &stores)
                    .await;
            assert!(matches!(
                response,
                EventsResponse::Count { count: 0 } | EventsResponse::GroupedCount { .. }
            ));
        }
    }
}

#[test]
fn typed_read_failures_preserve_fields_retry_policy_and_actual_operation() {
    for admission in [false, true] {
        let source = if admission {
            StorageError::AdmissionTimeout {
                operation: "fixture scalar admission".into(),
                timeout_ms: 17,
                pool_identity: Some("fixture-one-reader".into()),
            }
        } else {
            StorageError::Timeout {
                operation: "fixture grouped SQL".into(),
            }
        };
        let retryable = source.is_retryable();
        let response = count_storage_error_response(source, 30);
        let wire = serde_json::to_vec(&response).unwrap();
        let error = restore_count_read_failure(serde_json::from_slice(&wire).unwrap()).unwrap_err();
        assert_eq!(error.is_retryable(), retryable);
        match error {
            StorageError::AdmissionTimeout {
                operation,
                timeout_ms,
                pool_identity,
            } => {
                assert_eq!(timeout_ms, 17);
                assert_eq!(pool_identity.as_deref(), Some("fixture-one-reader"));
                assert_eq!(operation.to_string(), "fixture scalar admission (operator read budget 30s; earlier caller deadline retained)");
            }
            StorageError::Timeout { operation } => {
                assert_eq!(operation.to_string(),
                "fixture grouped SQL (operator read budget 30s; earlier caller deadline retained)")
            }
            other => panic!("typed read failure lost: {other:?}"),
        }
    }
}

#[test]
fn contradictory_read_failure_retry_message_or_writer_evidence_is_rejected() {
    for field in ["retryable", "message", "writer_task_failure"] {
        let response = count_storage_error_response(
            StorageError::Timeout {
                operation: "fixture SQL".into(),
            },
            30,
        );
        let mut wire = serde_json::to_value(response).unwrap();
        let replacement = match field {
            "retryable" => serde_json::json!(!wire[field].as_bool().unwrap()),
            "message" => serde_json::json!("a contradictory diagnostic"),
            _ => serde_json::to_value(WireWriterTaskFailure::RequestFailed {
                request_state: WireWriterTaskState::NotStarted,
            })
            .unwrap(),
        };
        wire[field] = replacement;
        let response = serde_json::from_value(wire).unwrap();
        assert!(matches!(
            restore_count_read_failure(response),
            Err(StorageError::Serialization { .. })
        ));
    }
}

struct SqlFixture {
    _dir: tempfile::TempDir,
    pool: Arc<khive_db::ConnectionPool>,
    sql: Arc<dyn SqlAccess>,
    store: Arc<CountStore>,
    progress: Arc<AtomicUsize>,
    frame_budget: Arc<tokio::sync::Semaphore>,
    backend: Arc<StorageBackend>,
    stores: NamespaceStores,
}

impl SqlFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            khive_db::ConnectionPool::new(khive_db::PoolConfig {
                path: Some(dir.path().join("count.db")),
                max_readers: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        pool.writer()
            .unwrap()
            .autocommit(|conn| {
                Ok(conn.execute_batch(
                    "CREATE TABLE count_fixture(n INTEGER PRIMARY KEY); \
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) \
             INSERT INTO count_fixture SELECT x FROM n; \
             CREATE VIEW events AS SELECT 'local' AS namespace FROM count_fixture a \
             CROSS JOIN count_fixture b CROSS JOIN count_fixture c;",
                )?)
            })
            .unwrap();
        let sql: Arc<dyn SqlAccess> = Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));
        let store = CountStore::new(CountWork::Sql(Arc::clone(&sql)));
        let stores: NamespaceStores = Arc::new(Mutex::new(std::collections::HashMap::new()));
        stores.lock().unwrap().insert("local".into(), store.clone());
        Self {
            _dir: dir,
            pool,
            sql,
            store,
            stores,
            progress: Arc::new(AtomicUsize::new(0)),
            frame_budget: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUEST_BYTES)),
            backend: Arc::new(StorageBackend::memory().unwrap()),
        }
    }

    fn serve(&self, stream: UnixStream) -> JoinHandle<()> {
        let (backend, stores, budget) = (
            Arc::clone(&self.backend),
            Arc::clone(&self.stores),
            Arc::clone(&self.frame_budget),
        );
        tokio::spawn(khive_db::scope_test_read_progress(
            Arc::clone(&self.progress),
            async move {
                serve_events_conn(stream, backend, stores, budget).await;
            },
        ))
    }

    async fn observe_sql(&self, task: &JoinHandle<()>, payload_len: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.progress.load(Ordering::SeqCst) == 0 {
                assert!(!task.is_finished(), "COUNT ended before SQLite progress");
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        assert_eq!(
            self.pool
                .reader_acquisition_snapshot()
                .active_pooled_checkouts,
            1
        );
        assert_eq!(
            self.frame_budget.available_permits(),
            MAX_INFLIGHT_REQUEST_BYTES - payload_len
        );
    }

    async fn await_settlement_and_reuse(&self, task: JoinHandle<()>) {
        tokio::time::timeout(
            khive_db::sqlite_interrupt_grace_from_env() + Duration::from_secs(2),
            task,
        )
        .await
        .expect("events connection did not await SQLite settlement")
        .unwrap();
        assert!(
            self.store.finished.load(Ordering::SeqCst),
            "dispatch was dropped instead of awaited"
        );
        let snapshot = self.pool.reader_acquisition_snapshot();
        assert_eq!(snapshot.active_pooled_checkouts, 0);
        assert_eq!(snapshot.available_reader_admission_slots, 1);
        assert_eq!(
            self.frame_budget.available_permits(),
            MAX_INFLIGHT_REQUEST_BYTES
        );
        let count = self
            .sql
            .reader()
            .await
            .unwrap()
            .count(SqlStatement {
                sql: "SELECT COUNT(*) FROM count_fixture".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(count, 1000);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scalar_and_grouped_disconnect_await_actual_sqlite_and_recover_same_pool() {
    for grouped in [false, true] {
        let fixture = SqlFixture::new();
        let (mut client, server) = UnixStream::pair().unwrap();
        let payload = serde_json::to_vec(&count_request(grouped, Some(30))).unwrap();
        let task = fixture.serve(server);
        write_frame(&mut client, &payload).await.unwrap();
        fixture.observe_sql(&task, payload.len()).await;
        drop(client);
        fixture.await_settlement_and_reuse(task).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_client_expiry_closes_socket_then_awaits_actual_sqlite_settlement() {
    let fixture = SqlFixture::new();
    let socket = fixture._dir.path().join("events.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = EventsSplitClient::new(socket).unwrap();
    let deadline = RequestReadDeadline::after(Duration::from_secs(2));
    let call = tokio::spawn(async move {
        let budget = CountReadBudget {
            operator_read_budget_secs: 30,
            deadline,
        };
        client
            .round_trip_count(&count_request(false, Some(30)), &budget)
            .await
    });
    let (stream, _) = listener.accept().await.unwrap();
    let task = fixture.serve(stream);
    let payload_len = serde_json::to_vec(&count_request(false, Some(30)))
        .unwrap()
        .len();
    fixture.observe_sql(&task, payload_len).await;
    let error = call.await.unwrap().unwrap_err();
    assert!(matches!(error, StorageError::Timeout { .. }));
    // Client Timeout alone is deliberately insufficient: join the real connection task.
    fixture.await_settlement_and_reuse(task).await;
}

#[derive(Clone, Copy)]
enum QueryWork {
    Fast,
    Slow,
    Delay(u64),
}

struct QueryStore {
    sql: Arc<dyn SqlAccess>,
    work: QueryWork,
    started: AtomicUsize,
    finished: AtomicBool,
}

impl QueryStore {
    fn new(sql: Arc<dyn SqlAccess>, work: QueryWork) -> Arc<Self> {
        Arc::new(Self {
            sql,
            work,
            started: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
        })
    }

    fn unsupported() -> StorageError {
        StorageError::Unsupported {
            capability: StorageCapability::Events,
            operation: "query fixture".into(),
            message: "only query reads are supported".into(),
        }
    }

    async fn read_fixture_sql(&self, label: &'static str) -> StorageResult<()> {
        assert!(
            capture_request_read_context().deadline().is_none(),
            "the query receiver must not mint a deadline"
        );
        self.started.fetch_add(1, Ordering::SeqCst);
        let result = async {
            if let QueryWork::Delay(seconds) = self.work {
                khive_storage::await_request_read_phase(
                    label,
                    tokio::time::sleep(Duration::from_secs(seconds)),
                )
                .await?;
            }
            let mut reader = self.sql.reader().await?;
            let sql = match self.work {
                QueryWork::Fast | QueryWork::Delay(_) => "SELECT COUNT(*) FROM count_fixture",
                // Finite real SQLite work, interrupted through the owned read context.
                QueryWork::Slow => "SELECT COUNT(*) FROM events CROSS JOIN count_fixture d",
            };
            reader
                .count(SqlStatement {
                    sql: sql.into(),
                    params: vec![],
                    label: Some(label.into()),
                })
                .await
        }
        .await;
        // The owned reader scope has ended; a dropped dispatch cannot set this witness.
        self.finished.store(true, Ordering::SeqCst);
        result.map(|_| ())
    }

    fn fixture_event() -> Event {
        let mut event = Event::new(
            "local",
            "cursor.fixture",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Note,
            "cursor-reader",
        );
        event.id = Uuid::from_u128(1);
        event.created_at = 100;
        event
    }
}

#[async_trait]
impl EventStore for QueryStore {
    async fn append_event(&self, _: Event) -> StorageResult<()> {
        Err(Self::unsupported())
    }
    async fn append_events(&self, _: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        Err(Self::unsupported())
    }
    async fn get_event(&self, _: Uuid) -> StorageResult<Option<Event>> {
        Err(Self::unsupported())
    }
    async fn query_events(&self, _: EventFilter, _: PageRequest) -> StorageResult<Page<Event>> {
        self.read_fixture_sql("fixture.events.query")
            .await
            .map(|_| {
                // This typed fixture exercises routing/lifetime, not production event decoding.
                Page {
                    items: vec![Self::fixture_event()],
                    total: None,
                }
            })
    }
    async fn query_event_page(&self, query: EventPageQuery) -> StorageResult<EventPageWindow> {
        crate::event_page::validate_page_query(&query)?;
        self.read_fixture_sql("fixture.events.query_page").await?;
        let event = Self::fixture_event();
        // Typed fixture output after real SQL work, not a production decoder witness.
        Ok(EventPageWindow {
            rows: vec![EventPageRow {
                order_key: EventOrderKey {
                    created_at_us: event.created_at,
                    physical_id: event.id.to_string(),
                },
                event,
            }],
            budget_stop: None,
        })
    }
    async fn count_events(&self, _: EventFilter) -> StorageResult<u64> {
        Err(Self::unsupported())
    }
    async fn count_events_grouped(
        &self,
        _: EventFilter,
        _: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        Err(Self::unsupported())
    }
}

fn query_request() -> EventsRequest {
    EventsRequest::QueryEvents {
        protocol_version: EVENTS_PROTOCOL_VERSION,
        namespace: "local".into(),
        filter: EventFilter::default(),
        page: PageRequest {
            offset: 0,
            limit: 1,
        },
    }
}

fn event_page_request() -> EventsRequest {
    EventsRequest::QueryEventPage {
        protocol_version: EVENTS_PROTOCOL_VERSION,
        namespace: "local".into(),
        query: EventPageQuery {
            since_us: 50,
            until_us: 150,
            kinds: vec![],
            actors: vec![],
            exclude_namespaces: vec![],
            after: None,
            max_rows: 1,
        },
    }
}

fn serve_query_with_cleanup(
    fixture: &SqlFixture,
    stream: UnixStream,
    cleanup: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    let (backend, stores, budget, progress) = (
        Arc::clone(&fixture.backend),
        Arc::clone(&fixture.stores),
        Arc::clone(&fixture.frame_budget),
        Arc::clone(&fixture.progress),
    );
    tokio::spawn(async move {
        khive_storage::scope_request_read_cancellation(
            cleanup,
            khive_db::scope_test_read_progress(progress, async move {
                serve_events_conn(stream, backend, stores, budget).await;
            }),
        )
        .await;
    })
}

async fn cleanup_query_peer(
    mut task: JoinHandle<()>,
    cleanup: tokio::sync::watch::Sender<bool>,
    fixture: &Arc<SqlFixture>,
) {
    let _ = cleanup.send(true);
    match tokio::time::timeout(
        khive_db::sqlite_interrupt_grace_from_env() + Duration::from_secs(2),
        &mut task,
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            // Keep private files available if a panicking task left settlement unknown.
            std::mem::forget(Arc::clone(fixture));
            panic!("query_fixture_server_panicked; private fixture retained");
        }
        Err(_) => {
            std::mem::forget(task);
            std::mem::forget(Arc::clone(fixture));
            panic!("query_fixture_cleanup_unjoined; task and private fixture retained");
        }
    }
}

fn assert_query_resources_returned(fixture: &SqlFixture) {
    let state = fixture.pool.reader_acquisition_snapshot();
    assert_eq!(state.active_pooled_checkouts, 0);
    assert_eq!(state.available_reader_admission_slots, 1);
    assert_eq!(
        fixture.frame_budget.available_permits(),
        MAX_INFLIGHT_REQUEST_BYTES
    );
}

async fn assert_framed_query_page(fixture: &Arc<SqlFixture>) {
    let store = QueryStore::new(Arc::clone(&fixture.sql), QueryWork::Fast);
    fixture
        .stores
        .lock()
        .unwrap()
        .insert("local".into(), store.clone());
    let payload = serde_json::to_vec(&query_request()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let (cleanup, cleanup_rx) = tokio::sync::watch::channel(false);
    let task = serve_query_with_cleanup(fixture, server, cleanup_rx);
    let sent = write_frame(&mut client, &payload).await;
    let response = if sent.is_ok() {
        Some(tokio::time::timeout(Duration::from_secs(2), read_frame(&mut client)).await)
    } else {
        None
    };
    drop(client);
    cleanup_query_peer(task, cleanup, fixture).await;
    // Baseline assertions occur only after the owned server task has joined.
    assert!(sent.is_ok(), "query_fixture_valid_frame_write");
    let bytes = response
        .expect("query_fixture_response_started")
        .expect("query_fixture_response_deadline")
        .expect("query_fixture_response_read");
    let EventsResponse::Pageful { page } =
        serde_json::from_slice(&bytes).expect("query_fixture_typed_response")
    else {
        panic!("query_fixture_nonempty_page_required");
    };
    assert_eq!(page.items.len(), 1, "query_fixture_nonempty_page_required");
    assert!(page.total.is_none());
    assert_eq!(page.items[0].id, Uuid::from_u128(1));
    assert_eq!(page.items[0].created_at, 100);
    assert_eq!(store.started.load(Ordering::SeqCst), 1);
    assert!(store.finished.load(Ordering::SeqCst));
    assert_query_resources_returned(fixture);
}

async fn assert_framed_event_page_window(fixture: &Arc<SqlFixture>) {
    let store = QueryStore::new(Arc::clone(&fixture.sql), QueryWork::Fast);
    fixture
        .stores
        .lock()
        .unwrap()
        .insert("local".into(), store.clone());
    let payload = serde_json::to_vec(&event_page_request()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let (cleanup, cleanup_rx) = tokio::sync::watch::channel(false);
    let task = serve_query_with_cleanup(fixture, server, cleanup_rx);
    let sent = write_frame(&mut client, &payload).await;
    let response = if sent.is_ok() {
        Some(tokio::time::timeout(Duration::from_secs(2), read_frame(&mut client)).await)
    } else {
        None
    };
    drop(client);
    cleanup_query_peer(task, cleanup, fixture).await;
    assert!(sent.is_ok(), "event_page_fixture_valid_frame_write");
    let bytes = response
        .expect("event_page_fixture_response_started")
        .expect("event_page_fixture_response_deadline")
        .expect("event_page_fixture_response_read");
    let EventsResponse::EventPageWindow { window } =
        serde_json::from_slice(&bytes).expect("event_page_fixture_typed_response")
    else {
        panic!("event_page_fixture_nonempty_window_required");
    };
    assert_eq!(
        window.rows.len(),
        1,
        "event_page_fixture_nonempty_window_required"
    );
    assert!(window.budget_stop.is_none());
    let row = &window.rows[0];
    assert_eq!(row.event.id, Uuid::from_u128(1));
    assert_eq!(row.event.created_at, 100);
    assert_eq!(row.order_key.created_at_us, row.event.created_at);
    assert_eq!(row.order_key.physical_id, row.event.id.to_string());
    assert_eq!(store.started.load(Ordering::SeqCst), 1);
    assert!(store.finished.load(Ordering::SeqCst));
    assert_query_resources_returned(fixture);
}

async fn query_disconnect_evidence_and_recover_same_pool(
    fixture: &Arc<SqlFixture>,
    request: EventsRequest,
) -> (bool, bool, bool, bool) {
    let store = QueryStore::new(Arc::clone(&fixture.sql), QueryWork::Slow);
    fixture
        .stores
        .lock()
        .unwrap()
        .insert("local".into(), store.clone());
    fixture.progress.store(0, Ordering::SeqCst);
    let payload = serde_json::to_vec(&request).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let (cleanup, cleanup_rx) = tokio::sync::watch::channel(false);
    let task = serve_query_with_cleanup(fixture, server, cleanup_rx);
    let sent = write_frame(&mut client, &payload).await;
    let observed_progress = if sent.is_ok() {
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.progress.load(Ordering::SeqCst) == 0 && !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
            && fixture.progress.load(Ordering::SeqCst) > 0
    } else {
        false
    };
    let active = fixture.pool.reader_acquisition_snapshot();
    let setup_valid = sent.is_ok()
        && observed_progress
        && !task.is_finished()
        && store.started.load(Ordering::SeqCst) == 1
        && active.active_pooled_checkouts == 1
        && fixture.frame_budget.available_permits() == MAX_INFLIGHT_REQUEST_BYTES - payload.len();
    drop(client);

    if setup_valid {
        let until = tokio::time::Instant::now()
            + khive_db::sqlite_interrupt_grace_from_env()
            + Duration::from_secs(2);
        while !task.is_finished() && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    // Save actual settlement/resource evidence before independent cleanup can change it.
    let settled_before_cleanup = task.is_finished() && store.finished.load(Ordering::SeqCst);
    let before = fixture.pool.reader_acquisition_snapshot();
    let reader_returned_before_cleanup = before.active_pooled_checkouts == 0;
    let admission_returned_before_cleanup = before.available_reader_admission_slots == 1;
    let frame_returned_before_cleanup =
        fixture.frame_budget.available_permits() == MAX_INFLIGHT_REQUEST_BYTES;

    // This watch is outside the server's optional peer-disconnect scope, including under C1.
    cleanup_query_peer(task, cleanup, fixture).await;
    assert!(
        setup_valid,
        "query_fixture_sql_progress_reader_and_frame_required"
    );
    assert!(
        store.finished.load(Ordering::SeqCst),
        "query_fixture_cleanup_must_settle_sql"
    );
    assert_query_resources_returned(fixture);
    let count = fixture
        .sql
        .reader()
        .await
        .unwrap()
        .count(SqlStatement {
            sql: "SELECT COUNT(*) FROM count_fixture".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(count, 1000, "query_fixture_same_pool_reuse");
    (
        settled_before_cleanup,
        reader_returned_before_cleanup,
        admission_returned_before_cleanup,
        frame_returned_before_cleanup,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_events_disconnect_settles_sqlite_and_recovers_same_pool() {
    let fixture = Arc::new(SqlFixture::new());
    assert_framed_query_page(&fixture).await;
    eprintln!("query_events_nonempty_sqlite_baseline_ok");
    let (
        settled_before_cleanup,
        reader_returned_before_cleanup,
        admission_returned_before_cleanup,
        frame_returned_before_cleanup,
    ) = query_disconnect_evidence_and_recover_same_pool(&fixture, query_request()).await;
    assert_framed_query_page(&fixture).await;

    assert!(
        settled_before_cleanup && reader_returned_before_cleanup
            && admission_returned_before_cleanup && frame_returned_before_cleanup,
        "query_peer_disconnect_settled_resources_before_cleanup: settled={settled_before_cleanup} reader={reader_returned_before_cleanup} admission={admission_returned_before_cleanup} frame={frame_returned_before_cleanup}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_event_page_disconnect_settles_sqlite_and_recovers_same_pool() {
    let fixture = Arc::new(SqlFixture::new());
    assert_framed_event_page_window(&fixture).await;
    eprintln!("query_event_page_nonempty_sqlite_baseline_ok");
    let (
        settled_before_cleanup,
        reader_returned_before_cleanup,
        admission_returned_before_cleanup,
        frame_returned_before_cleanup,
    ) = query_disconnect_evidence_and_recover_same_pool(&fixture, event_page_request()).await;
    assert_framed_event_page_window(&fixture).await;

    assert!(
        settled_before_cleanup && reader_returned_before_cleanup
            && admission_returned_before_cleanup && frame_returned_before_cleanup,
        "query_event_page_peer_disconnect_settled_resources_before_cleanup: settled={settled_before_cleanup} reader={reader_returned_before_cleanup} admission={admission_returned_before_cleanup} frame={frame_returned_before_cleanup}"
    );
}

#[tokio::test]
async fn query_events_read_budget_client_expiry_settles_sqlite_and_recovers_same_pool() {
    let fixture = Arc::new(SqlFixture::new());
    assert_framed_query_page(&fixture).await;
    let store = QueryStore::new(Arc::clone(&fixture.sql), QueryWork::Slow);
    fixture
        .stores
        .lock()
        .unwrap()
        .insert("local".into(), store.clone());
    fixture.progress.store(0, Ordering::SeqCst);
    let socket = fixture._dir.path().join("query-expiry.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = EventsSplitClient::new(socket).unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
    tokio::time::pause();
    let deadline = RequestReadDeadline::after(Duration::from_secs(2));
    let mut call = tokio::spawn(scope_request_read_deadline_at(deadline, async move {
        ForwardingEventStore::new("local", client)
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
    }));
    drive_until(|| accept.is_finished()).await;
    let (cleanup, receiver) = tokio::sync::watch::channel(false);
    let task = serve_query_with_cleanup(&fixture, accept.await.unwrap(), receiver);
    let observed_until = std::time::Instant::now() + Duration::from_secs(2);
    while fixture.progress.load(Ordering::SeqCst) == 0
        && !task.is_finished()
        && std::time::Instant::now() < observed_until
    {
        tokio::task::yield_now().await;
    }
    let active = fixture.pool.reader_acquisition_snapshot();
    let setup_valid = fixture.progress.load(Ordering::SeqCst) > 0
        && !task.is_finished()
        && active.active_pooled_checkouts == 1
        && store.started.load(Ordering::SeqCst) == 1
        && fixture.frame_budget.available_permits()
            == MAX_INFLIGHT_REQUEST_BYTES - serde_json::to_vec(&query_request()).unwrap().len();
    // Setup ran on a live clock. Cross Tokio's rounded-up millisecond tick,
    // then hold time fixed while the real client observes its original deadline.
    let expiry_tick = deadline.async_at() + Duration::from_millis(1);
    tokio::time::advance(expiry_tick - tokio::time::Instant::now()).await;
    let completion_until = std::time::Instant::now() + Duration::from_secs(5);
    while !call.is_finished() && std::time::Instant::now() < completion_until {
        tokio::task::yield_now().await;
    }
    let observed_expiry_tick = tokio::time::Instant::now();
    let result = if call.is_finished() {
        Some((&mut call).await.unwrap())
    } else {
        // A regressed client bound must fail without leaving hostile SQLite work behind.
        call.abort();
        let _ = (&mut call).await;
        None
    };
    tokio::time::resume();
    let until = tokio::time::Instant::now()
        + khive_db::sqlite_interrupt_grace_from_env()
        + Duration::from_secs(2);
    while !task.is_finished() && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let settled = task.is_finished() && store.finished.load(Ordering::SeqCst);
    let before = fixture.pool.reader_acquisition_snapshot();
    let returned = before.active_pooled_checkouts == 0
        && before.available_reader_admission_slots == 1
        && fixture.frame_budget.available_permits() == MAX_INFLIGHT_REQUEST_BYTES;
    let progress = fixture.progress.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let stopped = fixture.progress.load(Ordering::SeqCst) == progress;
    cleanup_query_peer(task, cleanup, &fixture).await;
    assert_framed_query_page(&fixture).await;
    assert_eq!(
        observed_expiry_tick, expiry_tick,
        "client completion must not advance beyond the original deadline's timer tick"
    );
    assert!(
        matches!(&result, Some(Err(StorageError::Timeout { operation })) if operation.contains("query_events") && operation.contains("operator read budget")),
        "client must expire at its original deadline plus one timer tick: {result:?}"
    );
    assert!(
        setup_valid && settled && returned && stopped,
        "expiry must settle SQL and return reader/frame permits before independent cleanup"
    );
}

#[tokio::test]
async fn query_events_read_budget_900s_receiver_has_no_independent_deadline() {
    const NAME: &str = "events_split::count_read_budget_tests::query_events_read_budget_900s_receiver_has_no_independent_deadline";
    const CHILD: &str = "KHIVE_EVENTS_QUERY_900S_CHILD";
    if std::env::var(CHILD).ok().as_deref() != Some(NAME) {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("completed");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, NAME)
            .env("KHIVE_REQUEST_READ_TIMEOUT_SECS", "900")
            .env("KHIVE_EVENTS_QUERY_900S_DONE", &marker)
            .spawn()
            .unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                assert_eq!(std::fs::read_to_string(marker).unwrap(), NAME);
                return;
            }
            if std::time::Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                panic!("900s receiver fixture wall deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(
        khive_storage::request_read_timeout_from_env(),
        Duration::from_secs(900)
    );
    let fixture = Arc::new(SqlFixture::new());
    let store = QueryStore::new(Arc::clone(&fixture.sql), QueryWork::Delay(40));
    fixture
        .stores
        .lock()
        .unwrap()
        .insert("local".into(), store.clone());
    let socket = fixture._dir.path().join("query-900.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = EventsSplitClient::new(socket).unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
    tokio::time::pause();
    let call = tokio::spawn(async move {
        ForwardingEventStore::new("local", client)
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
    });
    drive_until(|| accept.is_finished()).await;
    let (cleanup, receiver) = tokio::sync::watch::channel(false);
    let task = serve_query_with_cleanup(&fixture, accept.await.unwrap(), receiver);
    drive_until(|| store.started.load(Ordering::SeqCst) == 1).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    yield_ready_tasks().await;
    let pending_after_thirty = !call.is_finished() && !store.finished.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(10)).await;
    // SQL is real but bounded; resume so its blocking worker gets the ordinary cleanup grace.
    tokio::time::resume();
    let result = tokio::time::timeout(Duration::from_secs(2), call).await;
    cleanup_query_peer(task, cleanup, &fixture).await;
    assert!(
        pending_after_thirty,
        "receiver introduced an independent 30s timer"
    );
    let page = result.unwrap().unwrap().unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, Uuid::from_u128(1));
    assert!(store.finished.load(Ordering::SeqCst));
    assert_query_resources_returned(&fixture);
    std::fs::write(
        std::env::var_os("KHIVE_EVENTS_QUERY_900S_DONE").unwrap(),
        NAME,
    )
    .unwrap();
}
