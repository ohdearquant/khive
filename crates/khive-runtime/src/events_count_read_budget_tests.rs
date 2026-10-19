use super::*;
use khive_storage::{
    capture_request_read_context, scope_request_read_deadline_at, RequestReadDeadline, SqlAccess,
    SqlStatement, SqlValue, StorageCapability,
};
use std::sync::Mutex;
use tokio::task::JoinHandle;

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
    task: JoinHandle<()>,
}

impl Peer {
    fn delayed(seconds: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("events.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let received = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&received);
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
            observed.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let _ = write_frame(&mut stream, &serde_json::to_vec(&response).unwrap()).await;
        });
        Self {
            client: EventsSplitClient::new(socket).unwrap(),
            _dir: dir,
            received,
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
    deadlines: Mutex<Vec<Option<tokio::time::Instant>>>,
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
        let deadline = capture_request_read_context().deadline();
        self.deadlines
            .lock()
            .unwrap()
            .push(deadline.map(RequestReadDeadline::async_at));
        self.started.fetch_add(1, Ordering::SeqCst);
        let result = match &self.work {
            CountWork::Delay(seconds) => {
                khive_storage::await_request_read_phase("fixture.events.count", async {
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
        Err(Self::unsupported())
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
                *legacy.deadlines.lock().unwrap(),
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
