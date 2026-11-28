use super::*;
#[cfg(unix)]
use khive_runtime::event_plane_read_test_support::TRANSPORT_PAGE_ROWS;
#[cfg(unix)]
use khive_storage::EventFilter;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn actual_event_counts_handler_refuses_an_expired_original_request() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize_with_visibility(Namespace::local(), runtime.visible_namespaces().to_vec())
        .unwrap();
    let pack = BrainPack::new(runtime.clone());
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime));
    let registry = builder.build().unwrap();
    let expired = || khive_storage::RequestReadDeadline::after(std::time::Duration::ZERO);

    // The handler checks the original deadline before any read and names the operator budget.
    let direct = khive_storage::scope_request_read_deadline_at(
        expired(),
        pack.handle_event_counts(&token, json!({"since": "2026-10-01T00:00:00Z"})),
    )
    .await;
    assert!(
        matches!(&direct, Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { operation })) if operation.contains("operator read budget") && operation.contains("brain.event_counts")),
        "direct handler result: {direct:?}"
    );

    // Through the registry the pack's state load reads first and meets the expired deadline,
    // so the request is refused before any count runs. That load reports the timeout as an
    // internal error naming it, not as a typed storage timeout.
    let dispatched = khive_storage::scope_request_read_deadline_at(
        expired(),
        registry.dispatch(
            "brain.event_counts",
            json!({"since": "2026-10-01T00:00:00Z"}),
        ),
    )
    .await;
    let error = dispatched.expect_err("an expired request must not return counts");
    assert!(
        error.to_string().contains("timeout"),
        "dispatched error: {error:?}"
    );
    let live = registry
        .dispatch(
            "brain.event_counts",
            json!({"since": "2026-10-01T00:00:00Z"}),
        )
        .await
        .unwrap();
    assert!(live["window_event_total"].as_u64().is_some());
}

/// The runtime reader owns the typed count deadline, while real dispatch first
/// preserves the brain state-load prelude and its distinct SQL-read operation.
#[tokio::test]
async fn extracted_count_reader_keeps_typed_timeout_and_dispatch_prelude() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize_with_visibility(Namespace::local(), runtime.visible_namespaces().to_vec())
        .unwrap();
    let params = json!({"since": "2026-10-01T00:00:00Z"});
    let expired = || khive_storage::RequestReadDeadline::after(std::time::Duration::ZERO);

    let direct = khive_storage::scope_request_read_deadline_at(
        expired(),
        runtime.read_event_counts(&token, params.clone()),
    )
    .await;
    assert!(
        matches!(&direct, Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { operation }))
            if operation.contains("brain.event_counts") && operation.contains("operator read budget")),
        "extracted reader must retain the typed original-request timeout: {direct:?}"
    );

    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime));
    let registry = builder.build().unwrap();
    let dispatched = khive_storage::scope_request_read_deadline_at(
        expired(),
        registry.dispatch("brain.event_counts", params.clone()),
    )
    .await;
    assert!(
        matches!(&dispatched, Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { operation }))
            if operation == "pool_reader.query_row"),
        "real dispatch must retain the state-load prelude before the typed reader: {dispatched:?}"
    );
    let live = registry
        .dispatch("brain.event_counts", params)
        .await
        .unwrap();
    assert!(
        live["window_event_total"].as_u64().is_some(),
        "an expired prelude must not poison the subsequent healthy dispatch: {live}"
    );
}

#[cfg(unix)]
mod exhaustive_forwarded {
    use super::*;
    use khive_runtime::daemon::{read_frame, write_frame};
    use khive_runtime::events_split::{
        EventsRequest, EventsResponse, EventsSplitConfig, TestRegistryGuard,
    };
    use khive_storage::{Page, RequestReadDeadline, StorageError};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::net::UnixListener;

    const ACTOR: &str = "actor:budget-reader";
    const ROWS: usize = TRANSPORT_PAGE_ROWS as usize + 1;
    // Pausing after real setup retains a fractional millisecond. Tokio rounds
    // deadlines up to a timer tick, so advancing to the exact instant can leave
    // the sleep pending forever while drive_until deliberately holds time still.
    const TIMER_TICK: Duration = Duration::from_millis(1);

    async fn advance_past_timer(at: tokio::time::Instant) {
        let wake_at = at + TIMER_TICK;
        tokio::time::advance(wake_at - tokio::time::Instant::now()).await;
        assert_eq!(tokio::time::Instant::now(), wake_at);
    }

    struct Peer(tokio::task::JoinHandle<()>);
    impl Drop for Peer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn drive_until(
        phase: &str,
        mut ready: impl FnMut() -> bool,
        describe: &impl Fn() -> String,
    ) {
        let frozen = tokio::time::Instant::now();
        let started = std::time::Instant::now();
        loop {
            assert_eq!(
                tokio::time::Instant::now(),
                frozen,
                "virtual time advanced waiting for {phase}: {}",
                describe()
            );
            if ready() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "exhaustive IPC fixture did not reach {phase} within 5s wall time: {}",
                describe()
            );
            // Allow real SQLite workers and IPC to progress without advancing timers.
            tokio::task::yield_now().await;
        }
    }

    fn check_filter(namespace: &str, filter: &EventFilter) {
        assert_eq!(namespace, "local");
        assert_eq!(filter.actors, vec![ACTOR.to_string()]);
        assert!(
            filter.kinds.is_empty(),
            "exercise the unfiltered kind window"
        );
        assert_eq!(filter.after, Some(-1));
    }

    fn peer(
        listener: UnixListener,
        delays: [u64; 2],
        pages: Arc<AtomicUsize>,
        cursors: Arc<Mutex<Vec<Option<i64>>>>,
        operator_seconds: u64,
    ) -> Peer {
        let rows = Arc::new(
            (0..ROWS)
                .map(|index| {
                    let mut event = Event::new(
                        "local",
                        if index % 2 == 0 {
                            "fixture.even"
                        } else {
                            "fixture.odd"
                        },
                        khive_types::EventKind::Audit,
                        khive_types::SubstrateKind::Note,
                        ACTOR,
                    );
                    event.id = uuid::Uuid::from_u128(index as u128 + 1);
                    event.created_at = (ROWS - index) as i64 + 100;
                    event
                })
                .collect::<Vec<_>>(),
        );
        Peer(tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    completed = connections.join_next(), if !connections.is_empty() => { completed.unwrap().unwrap(); }
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        let (rows, pages, cursors) = (rows.clone(), pages.clone(), cursors.clone());
                        connections.spawn(async move {
                            let bytes = read_frame(&mut stream).await.unwrap();
                            let wire: Value = serde_json::from_slice(&bytes).unwrap();
                            assert_eq!(wire["protocol_version"], 6);
                            assert!(!wire.as_object().unwrap().keys().any(|key| key.contains("deadline") || key.contains("clock") || key.contains("remaining")));
                            let response = match serde_json::from_slice::<EventsRequest>(&bytes).unwrap() {
                                EventsRequest::CountEventsGrouped { namespace, filter, group_by, operator_read_budget_secs, .. } => {
                                    check_filter(&namespace, &filter);
                                    assert_eq!(filter.before, Some(1_000_000));
                                    assert!(matches!(group_by, khive_storage::event::EventGroupBy::Kind));
                                    assert_eq!(operator_read_budget_secs, Some(operator_seconds));
                                    EventsResponse::GroupedCount { counts: [("audit".into(), ROWS as u64)].into() }
                                }
                                EventsRequest::QueryEvents { namespace, filter, page, .. } => {
                                    check_filter(&namespace, &filter);
                                    assert!(wire.get("operator_read_budget_secs").is_none());
                                    assert_eq!(page.offset, 0);
                                    assert_eq!(page.limit, TRANSPORT_PAGE_ROWS);
                                    let index = pages.load(Ordering::SeqCst);
                                    assert!(index < 2, "two pages exhaust this fixture");
                                    cursors.lock().unwrap().push(filter.before);
                                    assert_eq!(filter.before, Some(if index == 0 { 1_000_000 } else { 103 }));
                                    let items = rows.iter().filter(|row| row.namespace == namespace
                                        && filter.actors.contains(&row.actor)
                                        && filter.after.is_none_or(|after| row.created_at > after)
                                        && filter.before.is_none_or(|before| row.created_at < before))
                                        .take(page.limit as usize).cloned().collect::<Vec<_>>();
                                    assert_eq!(items.len(), if index == 0 { TRANSPORT_PAGE_ROWS as usize } else { 2 });
                                    // The second page includes the first page's boundary row.
                                    if index == 1 { assert_eq!(items[0].id, uuid::Uuid::from_u128(TRANSPORT_PAGE_ROWS as u128)); }
                                    let response_at = tokio::time::Instant::now() + Duration::from_secs(delays[index]);
                                    let delay = tokio::time::sleep_until(response_at);
                                    pages.fetch_add(1, Ordering::SeqCst);
                                    delay.await;
                                    EventsResponse::Pageful { page: Page { items, total: None } }
                                }
                                EventsRequest::AppendEventsIdempotent { events, .. } => EventsResponse::Idempotent {
                                    result: khive_storage::event::IdempotentEventBatchResult {
                                        rows: vec![khive_storage::event::EventAppendDisposition::Inserted; events.len()],
                                    },
                                },
                                EventsRequest::AppendEvents { events, .. } => EventsResponse::Appended {
                                    summary: khive_storage::BatchWriteSummary { attempted: events.len() as u64, affected: events.len() as u64, ..Default::default() },
                                },
                                other => panic!("unexpected exhaustive request: {other:?}"),
                            };
                            // A refused caller closes before a delayed response is ready.
                            let _ = write_frame(&mut stream, &serde_json::to_vec(&response).unwrap()).await;
                        });
                    }
                }
            }
        }))
    }

    async fn run_case(
        delays: [u64; 2],
        budget_seconds: u64,
        operator_seconds: u64,
        succeeds: bool,
    ) {
        assert_eq!(
            khive_storage::request_read_timeout_from_env(),
            Duration::from_secs(operator_seconds)
        );
        let dir = tempfile::Builder::new()
            .prefix("ev6254-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let _registry_guard = TestRegistryGuard::new(dir.path());
        let socket = dir.path().join("events.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let pages = Arc::new(AtomicUsize::new(0));
        let cursors = Arc::new(Mutex::new(Vec::new()));
        let mut peer = peer(
            listener,
            delays,
            pages.clone(),
            cursors.clone(),
            operator_seconds,
        );
        let runtime = KhiveRuntime::new_for_test(khive_runtime::RuntimeConfig {
            db_path: Some(dir.path().join("domain.db")),
            actor_id: Some("budget-reader".into()),
            events_split: Some(EventsSplitConfig {
                db_path: dir.path().join("events.db"),
                socket_path: Some(socket),
            }),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(BrainPack::new(runtime));
        builder.with_actor_id(Some("budget-reader".into()));
        let registry = Arc::new(builder.build().unwrap());
        registry
            .dispatch("brain.profiles", json!({}))
            .await
            .expect("warm real BrainPack state before timing");
        tokio::time::pause();
        let original = RequestReadDeadline::after(Duration::from_secs(budget_seconds));
        let dispatched = registry.clone();
        let call = tokio::spawn(khive_storage::scope_request_read_deadline_at(
            original,
            async move {
                dispatched
                    .dispatch(
                        "brain.event_counts",
                        json!({
                            "since": "1970-01-01T00:00:00Z", "until": "1970-01-01T00:00:01Z",
                            "actor": ACTOR, "exhaustive": true, "group_by": ["verb", "actor"],
                        }),
                    )
                    .await
            },
        ));
        let describe = || {
            format!(
                "delays={delays:?}, caller={budget_seconds}s, operator={operator_seconds}s, \
                 pages={}, call_finished={}, peer_finished={}, virtual_elapsed={:?}",
                pages.load(Ordering::SeqCst),
                call.is_finished(),
                peer.0.is_finished(),
                tokio::time::Instant::now()
                    .duration_since(original.async_at() - Duration::from_secs(budget_seconds))
            )
        };
        drive_until(
            "first page",
            || pages.load(Ordering::SeqCst) == 1 || call.is_finished() || peer.0.is_finished(),
            &describe,
        )
        .await;
        assert_eq!(
            pages.load(Ordering::SeqCst),
            1,
            "actual exhaustive page must start: {}",
            describe()
        );
        let started = original.async_at() - Duration::from_secs(budget_seconds);
        let first_step = delays[0].min(budget_seconds).min(30);
        advance_past_timer(started + Duration::from_secs(first_step)).await;
        if delays[0] > first_step && budget_seconds > first_step {
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            assert!(
                !call.is_finished(),
                "long-budget exhaustive page stopped at 30s"
            );
            advance_past_timer(started + Duration::from_secs(delays[0].min(budget_seconds))).await;
        }
        if budget_seconds > delays[0] {
            drive_until(
                "second page",
                || pages.load(Ordering::SeqCst) == 2 || call.is_finished() || peer.0.is_finished(),
                &describe,
            )
            .await;
            assert_eq!(
                pages.load(Ordering::SeqCst),
                2,
                "the first full page must not end the aggregate: {}",
                describe()
            );
            // Keep expiry tied to the original deadline; the first page's
            // timer tick must not renew or extend the remaining allowance.
            let response_at = tokio::time::Instant::now() + Duration::from_secs(delays[1]);
            advance_past_timer(response_at.min(original.async_at())).await;
        }
        drive_until(
            "call completion",
            || call.is_finished() || peer.0.is_finished(),
            &describe,
        )
        .await;
        assert!(
            !peer.0.is_finished(),
            "framed peer failed its protocol assertions: {}",
            describe()
        );
        let result = call.await.unwrap();
        let elapsed = tokio::time::Instant::now()
            .duration_since(original.async_at() - Duration::from_secs(budget_seconds));
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(2), registry.shutdown_audit_batch())
            .await
            .unwrap()
            .unwrap();
        peer.0.abort();
        let _ = (&mut peer.0).await;
        if succeeds {
            let value = result.expect("complete exhaustive aggregate");
            assert_eq!(
                elapsed,
                Duration::from_secs(delays.iter().sum()) + TIMER_TICK * 2
            );
            assert_eq!(value["total"], ROWS);
            assert_eq!(value["window_event_total"], ROWS);
            assert_eq!(value["truncated"], false);
            assert_eq!(value["exhaustive"], true);
            assert_eq!(value["scope"]["namespace"], "local");
            assert_eq!(value["counts_by_kind"], json!({"audit": ROWS}));
            assert_eq!(value["counts_by_actor"], json!({(ACTOR): ROWS}));
            assert_eq!(
                value["counts_by_verb"],
                json!({"fixture.even": ROWS.div_ceil(2), "fixture.odd": ROWS / 2})
            );
            assert_eq!(
                value["counts_by_verb_and_actor"],
                json!({"fixture.even": {(ACTOR): ROWS.div_ceil(2)}, "fixture.odd": {(ACTOR): ROWS / 2}})
            );
            assert_eq!(*cursors.lock().unwrap(), vec![Some(1_000_000), Some(103)]);
        } else {
            assert_eq!(elapsed, Duration::from_secs(budget_seconds) + TIMER_TICK);
            assert!(
                matches!(&result, Err(RuntimeError::Storage(StorageError::Timeout { operation }))
                if operation.contains("query_events") && operation.contains(&format!("operator read budget {operator_seconds}s"))),
                "expired exhaustive read must refuse the entire result: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn exhaustive_forwarded_multipage_uses_one_deadline_and_refuses_partial_results() {
        run_case([11, 9], 30, 30, true).await;
        run_case([20, 11], 30, 30, false).await;
    }

    #[tokio::test]
    async fn exhaustive_forwarded_900s_child_crosses_thirty_and_retains_earlier_deadline() {
        const NAME: &str = "handlers::event_counts_read_budget_tests::exhaustive_forwarded::exhaustive_forwarded_900s_child_crosses_thirty_and_retains_earlier_deadline";
        const CHILD: &str = "KHIVE_EXHAUSTIVE_900S_CHILD";
        if std::env::var(CHILD).ok().as_deref() != Some(NAME) {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("completed");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, NAME)
                .env("KHIVE_REQUEST_READ_TIMEOUT_SECS", "900")
                .env("KHIVE_EXHAUSTIVE_900S_DONE", &marker)
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
                    panic!("900s exhaustive fixture wall deadline");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        run_case([40, 0], 900, 900, true).await;
        run_case([40, 0], 5, 900, false).await;
        std::fs::write(
            std::env::var_os("KHIVE_EXHAUSTIVE_900S_DONE").unwrap(),
            NAME,
        )
        .unwrap();
    }
}
