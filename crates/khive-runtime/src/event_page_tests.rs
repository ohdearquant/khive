use super::*;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use crate::events_split::EventsSplitConfig;
use crate::events_split::SplitEventStore;
#[cfg(unix)]
use crate::RuntimeConfig;
use crate::{ActorRef, StorageBackend};
use khive_storage::{EventFilter, PageRequest};
use khive_types::SubstrateKind;

fn token(visible: &[&str]) -> NamespaceToken {
    NamespaceToken::mint_with_visibility(
        Namespace::local(),
        visible
            .iter()
            .map(|ns| Namespace::parse(ns).unwrap())
            .collect(),
        ActorRef {
            kind: "actor".into(),
            id: "alice".into(),
        },
    )
}

fn event(namespace: &str, id: u128, time: i64) -> Event {
    let mut event = Event::new(
        namespace,
        "fixture",
        EventKind::Audit,
        SubstrateKind::Event,
        "actor:alice",
    );
    event.id = Uuid::from_u128(id);
    event.created_at = time;
    event
}

fn request() -> EventReadPageRequest {
    EventReadPageRequest {
        since_us: 0,
        until_us: Some(1000),
        kinds: vec![EventKind::Audit],
        actors: vec!["actor:alice".into()],
        namespaces: None,
        exclude_namespaces: Vec::new(),
        limit: 2,
        after: None,
    }
}

fn query(max_rows: u32) -> EventPageQuery {
    EventPageQuery {
        since_us: 0,
        until_us: 1000,
        kinds: vec![EventKind::Audit],
        actors: vec!["actor:alice".into()],
        exclude_namespaces: Vec::new(),
        after: None,
        max_rows,
    }
}

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, ns: &str, rows: Vec<Event>) {
    runtime
        .events(&token.with_namespace(Namespace::parse(ns).unwrap()))
        .unwrap()
        .append_events(rows)
        .await
        .unwrap();
}

#[tokio::test]
async fn namespace_intersection_exclusion_and_sparse_pages_keep_the_full_ordered_set() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&["team", "excluded"]);
    seed(
        &runtime,
        &token,
        "local",
        vec![event("local", 1, 10), event("local", 5, 10)],
    )
    .await;
    seed(
        &runtime,
        &token,
        "team",
        vec![event("team", 3, 10), event("team", 7, 10)],
    )
    .await;
    seed(
        &runtime,
        &token,
        "excluded",
        (8..50).map(|id| event("excluded", id, 5)).collect(),
    )
    .await;
    seed(&runtime, &token, "hidden", vec![event("hidden", 99, 1)]).await;
    let local = runtime.page_events(&token, request()).await.unwrap();
    assert_eq!(
        local
            .events
            .iter()
            .map(|e| e.id.as_u128())
            .collect::<Vec<_>>(),
        vec![1, 5]
    );
    let mut req = request();
    req.namespaces = Some(vec![
        "team".into(),
        "local".into(),
        "excluded".into(),
        "hidden".into(),
    ]);
    req.exclude_namespaces = vec!["excluded".into()];
    let mut ids = Vec::new();
    loop {
        let page = runtime.page_events(&token, req.clone()).await.unwrap();
        assert_eq!(page.namespaces, vec!["local", "team"]);
        assert!(!page.events.is_empty());
        assert!(page
            .events
            .iter()
            .all(|event| event.namespace != "excluded" && event.namespace != "hidden"));
        ids.extend(page.events.iter().map(|e| e.id.as_u128()));
        if !page.has_more {
            assert!(page.next_after.is_none());
            break;
        }
        req.after = page.next_after;
    }
    assert_eq!(ids, vec![1, 3, 5, 7]);
    for ns in ["hidden", "valid_but_absent"] {
        let mut req = request();
        req.namespaces = Some(vec![ns.into()]);
        let page = runtime.page_events(&token, req).await.unwrap();
        assert!(page.events.is_empty() && page.namespaces.is_empty() && !page.has_more);
    }
    for scopes in [Vec::new(), vec!["excluded".into()]] {
        let mut req = request();
        req.namespaces = Some(scopes);
        req.exclude_namespaces = vec!["excluded".into()];
        let page = runtime.page_events(&token, req).await.unwrap();
        assert!(page.events.is_empty() && !page.has_more && page.next_after.is_none());
    }
}

#[tokio::test]
async fn cursor_binds_current_principal_filters_and_visible_scope_but_not_limit() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&["team"]);
    seed(
        &runtime,
        &token,
        "local",
        (1..=5).map(|id| event("local", id, 10)).collect(),
    )
    .await;
    let mut original = request();
    original.namespaces = Some(vec!["local".into(), "team".into()]);
    let first = runtime.page_events(&token, original.clone()).await.unwrap();
    original.after = first.next_after;
    let mut resumed = original.clone();
    resumed.limit = 1;
    resumed.until_us = None;
    resumed.kinds.push(EventKind::Audit);
    resumed.namespaces = Some(vec!["team".into(), "local".into(), "local".into()]);
    let page = runtime.page_events(&token, resumed).await.unwrap();
    assert_eq!(page.until_us, 1000);
    assert_eq!(page.events[0].id, Uuid::from_u128(3));
    let mut changed = Vec::new();
    let mut req = original.clone();
    req.since_us = 1;
    changed.push(req);
    let mut req = original.clone();
    req.until_us = Some(999);
    changed.push(req);
    let mut req = original.clone();
    req.kinds.clear();
    changed.push(req);
    let mut req = original.clone();
    req.actors.clear();
    changed.push(req);
    let mut req = original.clone();
    req.namespaces = None;
    changed.push(req);
    let mut req = original.clone();
    req.exclude_namespaces.push("team".into());
    changed.push(req);
    for req in changed {
        let error = runtime.page_events(&token, req).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid event page cursor or changed query scope"));
    }
    let narrower = NamespaceToken::mint_authorized(Namespace::local(), token.actor().clone());
    assert!(runtime
        .page_events(&narrower, original.clone())
        .await
        .is_err());
    let other = NamespaceToken::mint_with_visibility(
        Namespace::local(),
        vec![Namespace::parse("team").unwrap()],
        ActorRef {
            kind: "actor".into(),
            id: "bob".into(),
        },
    );
    assert!(runtime.page_events(&other, original).await.is_err());
}

#[tokio::test]
async fn compound_cursor_walks_over_transport_sized_ties_and_documents_late_keys() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&[]);
    seed(
        &runtime,
        &token,
        "local",
        (1..=4100).map(|id| event("local", id * 2, 10)).collect(),
    )
    .await;
    let mut req = request();
    req.limit = 1000;
    let first = runtime.page_events(&token, req.clone()).await.unwrap();
    assert_eq!(first.events.len(), 1000);
    req.after = first.next_after;
    seed(
        &runtime,
        &token,
        "local",
        vec![
            event("local", 1, 10),
            event("local", 9001, 9),
            event("local", 9002, 11),
        ],
    )
    .await;
    let mut ids = first.events.into_iter().map(|e| e.id).collect::<Vec<_>>();
    loop {
        let page = runtime.page_events(&token, req.clone()).await.unwrap();
        ids.extend(page.events.into_iter().map(|e| e.id));
        if !page.has_more {
            break;
        }
        req.after = page.next_after;
    }
    let mut expected = (1..=4100)
        .map(|id| Uuid::from_u128(id * 2))
        .collect::<Vec<_>>();
    expected.push(Uuid::from_u128(9002));
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn first_page_freezes_omitted_until_and_invalid_bounds_refuse() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&[]);
    seed(
        &runtime,
        &token,
        "local",
        (1..=3).map(|id| event("local", id, 10)).collect(),
    )
    .await;
    let mut req = request();
    req.until_us = None;
    let first = runtime.page_events(&token, req.clone()).await.unwrap();
    req.after = first.next_after;
    let second = runtime.page_events(&token, req).await.unwrap();
    assert_eq!(first.until_us, second.until_us);
    assert_eq!(second.events.len(), 1);
    for limit in [0, 1001, u32::MAX] {
        let mut req = request();
        req.limit = limit;
        assert!(runtime.page_events(&token, req).await.is_err());
    }
    for (since, until) in [(1000, 1000), (1001, 1000), (i64::MIN, 1000), (0, i64::MAX)] {
        let mut req = request();
        req.since_us = since;
        req.until_us = Some(until);
        assert!(runtime.page_events(&token, req).await.is_err());
    }
    let mut req = request();
    req.namespaces = Some(vec!["local".into(); 17]);
    assert!(runtime.page_events(&token, req).await.is_err());
    let mut req = request();
    req.exclude_namespaces = vec!["local".into(); 33];
    assert!(runtime.page_events(&token, req).await.is_err());
}

#[test]
fn cursor_decoder_is_bounded_canonical_and_preserves_physical_uuid_bytes() {
    let key = EventOrderKey {
        created_at_us: 123456,
        physical_id: "00000000-0000-0000-0000-00000000ABCD".into(),
    };
    let binding = "a".repeat(64);
    let cursor = encode_cursor(999999, &key, &binding);
    let decoded = decode_cursor(&cursor).unwrap();
    assert_eq!(decoded.key, key);
    assert_eq!(decoded.binding, binding);
    for malformed in [
        "x".repeat(513),
        cursor.replacen("ep1", "ep2", 1),
        cursor.replacen("999999", "0999999", 1),
        cursor.replacen("123456", "+123456", 1),
        format!("{cursor}:extra"),
        cursor.replacen(&binding, &"A".repeat(64), 1),
        format!("ep1:999999:123456:{}:{binding}", "00".repeat(36)),
    ] {
        assert!(decode_cursor(&malformed).is_err());
    }
}

#[test]
fn returned_row_invariants_refuse_instead_of_hiding_a_removed_predicate() {
    let row = EventPageRow {
        event: event("local", 1, 10),
        order_key: EventOrderKey {
            created_at_us: 10,
            physical_id: Uuid::from_u128(1).to_string(),
        },
    };
    let mut rows = Vec::new();
    let mut bad = row.clone();
    bad.event.namespace = "foreign".into();
    rows.push(bad);
    let mut bad = row.clone();
    bad.event.actor = "actor:bob".into();
    rows.push(bad);
    let mut bad = row.clone();
    bad.event.kind = EventKind::FeedbackExplicit;
    rows.push(bad);
    let mut bad = row.clone();
    bad.order_key.created_at_us = 9;
    rows.push(bad);
    let mut bad = row.clone();
    bad.order_key.physical_id = Uuid::from_u128(2).to_string();
    rows.push(bad);
    for bad in rows {
        assert!(validate_window(
            &query(2),
            Some("local"),
            &EventPageWindow {
                rows: vec![bad],
                budget_stop: None
            }
        )
        .is_err());
    }
    let window = EventPageWindow {
        rows: vec![row.clone()],
        budget_stop: None,
    };
    validate_window(&query(2), Some("local"), &window).unwrap();
    let mut excluded = query(2);
    excluded.exclude_namespaces.push("local".into());
    assert!(validate_window(&excluded, Some("local"), &window).is_err());
    let mut after = query(2);
    after.after = Some(row.order_key);
    assert!(validate_window(&after, Some("local"), &window).is_err());
    assert!(validate_window(
        &query(1),
        Some("local"),
        &EventPageWindow {
            rows: vec![window.rows[0].clone(); 2],
            budget_stop: None,
        }
    )
    .is_err());
    let mut budget = ByteBudget(16);
    serde_json::to_writer(&mut budget, &"small").unwrap();
    assert!(budget.0 < 16);
    assert!(serde_json::to_writer(&mut budget, &"too large for what remains").is_err());
}

#[tokio::test]
async fn direct_split_seeks_both_planes_and_keeps_legacy_reads_unchanged() {
    let legacy = StorageBackend::memory()
        .unwrap()
        .events_for_namespace("local")
        .unwrap();
    let lane = StorageBackend::memory()
        .unwrap()
        .events_for_namespace("local")
        .unwrap();
    legacy
        .append_events(vec![event("local", 1, 10), event("local", 3, 10)])
        .await
        .unwrap();
    lane.append_events(vec![event("local", 2, 10), event("local", 4, 10)])
        .await
        .unwrap();
    let split = SplitEventStore::new(legacy, lane);
    let first = split.query_event_page(query(2)).await.unwrap();
    assert_eq!(
        first
            .rows
            .iter()
            .map(|r| r.event.id.as_u128())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let mut seek = query(2);
    seek.after = Some(first.rows[1].order_key.clone());
    let second = split.query_event_page(seek).await.unwrap();
    assert_eq!(
        second
            .rows
            .iter()
            .map(|r| r.event.id.as_u128())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(split.count_events(EventFilter::default()).await.unwrap(), 4);
    let old = split
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 1,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        old.items.iter().map(|e| e.id.as_u128()).collect::<Vec<_>>(),
        vec![3, 2]
    );
    assert!(old.total.is_none());
    let mut excluded = query(2);
    excluded.exclude_namespaces.push("local".into());
    assert!(split
        .query_event_page(excluded)
        .await
        .unwrap()
        .rows
        .is_empty());
}

#[tokio::test]
async fn split_duplicate_key_refuses_before_a_limit_one_page_can_skip_its_twin() {
    let legacy_backend = StorageBackend::memory().unwrap();
    let lane_backend = StorageBackend::memory().unwrap();
    let legacy = legacy_backend.events_for_namespace("local").unwrap();
    let lane = lane_backend.events_for_namespace("local").unwrap();
    legacy.append_event(event("local", 1, 10)).await.unwrap();
    lane.append_event(event("local", 2, 10)).await.unwrap();
    let split = SplitEventStore::new(legacy, lane.clone());
    let valid = split.query_event_page(query(1)).await.unwrap();
    assert_eq!(valid.rows[0].event.id, Uuid::from_u128(1));
    lane.append_event(event("local", 1, 10)).await.unwrap();
    let error = split.query_event_page(query(1)).await.unwrap_err();
    assert!(error.to_string().contains("duplicate ordering key"));
    assert_eq!(split.count_events(EventFilter::default()).await.unwrap(), 3);
    let legacy_page = split
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 3,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        legacy_page.items.len(),
        3,
        "legacy multiset semantics are unchanged"
    );
    let row = valid.rows[0].clone();
    assert!(validate_window(
        &query(2),
        Some("local"),
        &EventPageWindow {
            rows: vec![row.clone(), row],
            budget_stop: None,
        }
    )
    .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn cross_namespace_plane_duplicate_refuses_before_global_page_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let _registry = crate::events_split::TestRegistryGuard::new(dir.path());
    let lane_path = dir.path().join("lane.db");
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(dir.path().join("main.db")),
        events_split: Some(EventsSplitConfig {
            db_path: lane_path.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = token(&["team"]);
    seed(&runtime, &token, "local", vec![event("local", 1, 10)]).await;
    // One process opens the lane with one set of policies: the test runtime's
    // pool carries the test lock directory, so the lane uses it.
    let lane = crate::events_split::direct_backend_with_policies(
        &lane_path,
        false,
        None,
        runtime.backend().pool().config().wal_ceiling,
        Some(runtime.events_disk_guard_policy().unwrap()),
        runtime.events_volume_lock_dir(),
    )
    .unwrap();
    let team = lane.events_for_namespace("team").unwrap();
    team.append_event(event("team", 2, 10)).await.unwrap();
    let mut req = request();
    req.limit = 1;
    req.namespaces = Some(vec!["local".into(), "team".into()]);
    let valid = runtime.page_events(&token, req.clone()).await.unwrap();
    assert!(valid.has_more);
    assert_eq!(valid.events[0].id, Uuid::from_u128(1));
    team.append_event(event("team", 1, 10)).await.unwrap();
    let error = runtime.page_events(&token, req).await.unwrap_err();
    assert!(error.to_string().contains("duplicate ordering key"));
}

#[cfg(unix)]
#[tokio::test]
async fn aggregate_budget_refuses_when_two_planes_fill_sixteen_namespaces() {
    // Arithmetic: 16 namespaces x 2 planes x 500 rows = 16,000 rows, each with a
    // 1,800-byte payload. A plane window admits at most 1 MiB (1,048,576 bytes) of
    // stored text, so the 32 plane windows hold at most 32 MiB raw (33,554,432 bytes).
    // Each namespace's 1,000 rows fit one page at limit 1000 with no leaf stop.
    // Serialized rows add key and ordering overhead: the measured total is
    // 36,521,104 bytes (about 2.3 KB per row), above the 32 MiB bound. The test
    // checks the total before asserting the refusal.
    const NAMESPACES: usize = 16;
    const ROWS_PER_PLANE: usize = 500;
    const PAYLOAD_BYTES: usize = 1_800;
    const SERIALIZED_BOUND: usize = 32 * 1024 * 1024;
    const _: () = assert!(NAMESPACES <= MAX_NAMESPACES);

    let dir = tempfile::tempdir().unwrap();
    let _registry = crate::events_split::TestRegistryGuard::new(dir.path());
    let lane_path = dir.path().join("lane.db");
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(dir.path().join("main.db")),
        events_split: Some(EventsSplitConfig {
            db_path: lane_path.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let names = (0..NAMESPACES)
        .map(|ns| format!("ns{ns:02}"))
        .collect::<Vec<_>>();
    let visible = names.iter().map(String::as_str).collect::<Vec<_>>();
    let token = token(&visible);
    // One process opens the lane with one set of policies: the test runtime's
    // pool carries the test lock directory, so the lane uses it.
    let lane = crate::events_split::direct_backend_with_policies(
        &lane_path,
        false,
        None,
        runtime.backend().pool().config().wal_ceiling,
        Some(runtime.events_disk_guard_policy().unwrap()),
        runtime.events_volume_lock_dir(),
    )
    .unwrap();
    for (index, name) in names.iter().enumerate() {
        let plane_rows = |plane: usize| {
            (0..ROWS_PER_PLANE)
                .map(|row| {
                    let id = (index * 2 + plane) * 1000 + row + 1;
                    let mut event = event(name, id as u128, row as i64 + 1);
                    event.payload = serde_json::json!({ "large": "x".repeat(PAYLOAD_BYTES) });
                    event
                })
                .collect::<Vec<_>>()
        };
        seed(&runtime, &token, name, plane_rows(0)).await;
        lane.events_for_namespace(name)
            .unwrap()
            .append_events(plane_rows(1))
            .await
            .unwrap();
    }

    let mut serialized = 0usize;
    for name in &names {
        let scoped = token.with_namespace(Namespace::parse(name).unwrap());
        let window = runtime
            .events(&scoped)
            .unwrap()
            .query_event_page(query(MAX_LIMIT + 1))
            .await
            .unwrap();
        assert!(window.budget_stop.is_none(), "{name}: a plane leaf stopped");
        assert_eq!(
            window.rows.len(),
            2 * ROWS_PER_PLANE,
            "{name}: rows servable"
        );
        serialized += serde_json::to_vec(&window.rows).unwrap().len();
    }
    assert!(
        serialized > SERIALIZED_BOUND,
        "fixture serializes to {serialized} bytes, not above {SERIALIZED_BOUND}"
    );

    let mut req = request();
    req.limit = MAX_LIMIT;
    req.namespaces = Some(names.clone());
    let error = runtime.page_events(&token, req).await.unwrap_err();
    assert_eq!(error.to_string(), page_budget_exceeded_error().to_string());
    let details = refusal_details(error);
    assert_eq!(details.get("reason"), Some("page_budget_exceeded"));
    assert_eq!(
        details.iter().count(),
        1,
        "no event_id or resume_after detail"
    );
    assert!(details.get("event_id").is_none());
    assert!(details.get("resume_after").is_none());
}

#[cfg(unix)]
mod socket {
    use super::*;
    use crate::daemon::{read_frame, write_frame};
    use crate::events_split::{
        run_events_daemon, EventsRequest, EventsResponse, EventsSplitClient, ForwardingEventStore,
        EVENTS_PROTOCOL_VERSION,
    };
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::net::{UnixListener, UnixStream};

    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn daemon(dir: &tempfile::TempDir) -> (PathBuf, Server) {
        let socket = dir.path().join("events.sock");
        let db = dir.path().join("events.db");
        let path = socket.clone();
        let server = Server(tokio::spawn(async move {
            run_events_daemon(&db, &path).await.unwrap();
        }));
        for _ in 0..100 {
            if UnixStream::connect(&socket).await.is_ok() {
                return (socket, server);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("events daemon did not start");
    }

    async fn wire(path: &std::path::Path, request: &EventsRequest) -> EventsResponse {
        let mut stream = UnixStream::connect(path).await.unwrap();
        write_frame(&mut stream, &serde_json::to_vec(request).unwrap())
            .await
            .unwrap();
        serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn socket_split_matches_direct_order_and_wire_guards_still_refuse() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let (socket, _server) = daemon(&dir).await;
        let forwarded = Arc::new(ForwardingEventStore::new(
            "local",
            EventsSplitClient::new(socket.clone()).unwrap(),
        ));
        forwarded
            .append_events_idempotent(vec![event("local", 2, 10), event("local", 4, 10)])
            .await
            .unwrap();
        let legacy = StorageBackend::memory()
            .unwrap()
            .events_for_namespace("local")
            .unwrap();
        legacy
            .append_events(vec![event("local", 1, 10), event("local", 3, 10)])
            .await
            .unwrap();
        let split = SplitEventStore::new(legacy, forwarded);
        let mut seek = query(1);
        let mut ids = Vec::new();
        loop {
            let page = split.query_event_page(seek.clone()).await.unwrap();
            let Some(row) = page.rows.first() else {
                break;
            };
            ids.push(row.event.id.as_u128());
            seek.after = Some(row.order_key.clone());
        }
        assert_eq!(ids, vec![1, 2, 3, 4]);
        for (version, max_rows) in [
            (EVENTS_PROTOCOL_VERSION + 1, 1),
            (EVENTS_PROTOCOL_VERSION, 4097),
            (EVENTS_PROTOCOL_VERSION, 0),
        ] {
            let result = wire(
                &socket,
                &EventsRequest::QueryEventPage {
                    protocol_version: version,
                    namespace: "local".into(),
                    query: query(max_rows),
                },
            )
            .await;
            assert!(matches!(
                result,
                EventsResponse::Error {
                    retryable: false,
                    ..
                }
            ));
        }
        let valid = wire(
            &socket,
            &EventsRequest::QueryEventPage {
                protocol_version: EVENTS_PROTOCOL_VERSION,
                namespace: "local".into(),
                query: query(2),
            },
        )
        .await;
        assert!(
            matches!(valid, EventsResponse::EventPageWindow { window } if window.rows.len() == 2)
        );
    }

    #[tokio::test]
    async fn old_peer_refusal_never_falls_back_to_the_legacy_query() {
        #[derive(serde::Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum OldRead {
            QueryEvents,
            CountEvents,
        }
        assert!(
            serde_json::from_value::<OldRead>(serde_json::json!({"op":"query_events"})).is_ok()
        );
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join("old.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_frame(&mut stream).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(value["op"], "query_event_page");
            assert_eq!(value["protocol_version"], EVENTS_PROTOCOL_VERSION);
            assert!(serde_json::from_slice::<OldRead>(&request).is_err());
            let response = EventsResponse::Error {
                message:
                    "events daemon could not parse request frame: unknown variant query_event_page"
                        .into(),
                retryable: false,
                writer_task_failure: None,
                sqlite_write_failure: None,
            };
            write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "no legacy-query fallback connection"
            );
        });
        let store = ForwardingEventStore::new("local", EventsSplitClient::new(path).unwrap());
        let error = store.query_event_page(query(1)).await.unwrap_err();
        assert!(!error.is_retryable());
        assert!(error
            .to_string()
            .contains("unknown variant query_event_page"));
        peer.await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn readonly_page_never_creates_missing_lane_and_reads_an_existing_lane() {
    let dir = tempfile::tempdir().unwrap();
    let _registry = crate::events_split::TestRegistryGuard::new(dir.path());
    let main = dir.path().join("main.db");
    let lane = dir.path().join("events.db");
    {
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: Some(main.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        seed(&runtime, &token(&[]), "local", vec![event("local", 1, 10)]).await;
    }
    khive_storage::test_support::freeze_snapshot_sidecars(&main);
    let runtime = KhiveRuntime::new_readonly_for_test(RuntimeConfig {
        db_path: Some(main),
        events_split: Some(EventsSplitConfig {
            db_path: lane.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let page = runtime.page_events(&token(&[]), request()).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert!(!lane.exists());
    {
        let backend = StorageBackend::sqlite_for_test(&lane).unwrap();
        backend
            .events_for_namespace("local")
            .unwrap()
            .append_event(event("local", 2, 10))
            .await
            .unwrap();
    }
    khive_storage::test_support::freeze_snapshot_sidecars(&lane);
    let page = runtime.page_events(&token(&[]), request()).await.unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|e| e.id.as_u128())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

fn big_event(namespace: &str, id: u128, time: i64, bytes: usize) -> Event {
    let mut event = event(namespace, id, time);
    event.payload = serde_json::json!({"large": "x".repeat(bytes)});
    event
}

fn refusal_details(error: RuntimeError) -> Details {
    match error {
        RuntimeError::Khive(error) => error.details().cloned().expect("typed refusal details"),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

fn ids(page: &EventReadPageResult) -> Vec<u128> {
    page.events.iter().map(|e| e.id.as_u128()).collect()
}

fn key(id: u128, time: i64) -> EventOrderKey {
    EventOrderKey {
        created_at_us: time,
        physical_id: Uuid::from_u128(id).to_string(),
    }
}

#[tokio::test]
async fn oversized_event_refuses_with_its_id_and_a_cursor_that_skips_exactly_that_row() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&[]);
    seed(
        &runtime,
        &token,
        "local",
        vec![
            event("local", 1, 10),
            big_event("local", 2, 20, 1_100_000),
            event("local", 3, 30),
        ],
    )
    .await;
    let mut req = request();
    req.limit = 1;
    let first = runtime.page_events(&token, req.clone()).await.unwrap();
    assert_eq!(ids(&first), vec![1]);
    assert!(first.has_more, "the stopped row proves more rows exist");

    req.after = first.next_after.clone();
    let refused = runtime.page_events(&token, req.clone()).await.unwrap_err();
    let details = refusal_details(refused);
    assert_eq!(details.get("reason"), Some("row_exceeds_budget"));
    assert_eq!(
        details.get("event_id"),
        Some(Uuid::from_u128(2).to_string().as_str())
    );
    assert_eq!(details.iter().count(), 3, "no payload or raw key text");
    let resume_after = details.get("resume_after").unwrap().to_owned();
    assert_eq!(
        decode_cursor(&resume_after).unwrap().key,
        key(2, 20),
        "the cursor sits at the oversized row"
    );

    // Control: without the resume cursor the same position refuses again.
    let again = runtime.page_events(&token, req.clone()).await.unwrap_err();
    assert_eq!(
        refusal_details(again).get("event_id"),
        Some(Uuid::from_u128(2).to_string().as_str())
    );
    let mut from_start = request();
    from_start.limit = 1;
    from_start.since_us = 20;
    let again = runtime.page_events(&token, from_start).await.unwrap_err();
    assert_eq!(
        refusal_details(again).get("reason"),
        Some("row_exceeds_budget")
    );

    req.after = Some(resume_after);
    let resumed = runtime.page_events(&token, req).await.unwrap();
    assert_eq!(ids(&resumed), vec![3]);
    assert!(!resumed.has_more);
}

#[tokio::test]
async fn multi_row_page_stopped_mid_page_refuses_without_a_cursor() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&[]);
    let mut rows = (1..=3)
        .map(|id| event("local", id, id as i64 * 10))
        .collect::<Vec<_>>();
    rows.push(big_event("local", 4, 40, 1_100_000));
    rows.push(event("local", 5, 50));
    seed(&runtime, &token, "local", rows).await;
    let mut req = request();
    req.limit = 10;
    let details = refusal_details(runtime.page_events(&token, req.clone()).await.unwrap_err());
    assert_eq!(details.get("reason"), Some("page_budget_exceeded"));
    assert_eq!(details.iter().count(), 1);
    assert!(details.get("resume_after").is_none());

    // A smaller limit reaches the rows before the oversized one without skipping any.
    req.limit = 3;
    let page = runtime.page_events(&token, req).await.unwrap();
    assert_eq!(ids(&page), vec![1, 2, 3]);
    assert!(page.has_more);
}

#[tokio::test]
async fn two_rows_over_the_leaf_budget_are_read_one_per_page() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&[]);
    seed(
        &runtime,
        &token,
        "local",
        vec![
            big_event("local", 1, 10, 600_000),
            big_event("local", 2, 20, 600_000),
        ],
    )
    .await;
    let mut req = request();
    req.limit = 1;
    let first = runtime.page_events(&token, req.clone()).await.unwrap();
    assert_eq!(ids(&first), vec![1]);
    assert!(first.has_more);
    assert!(first.next_after.is_some());
    req.after = first.next_after;
    let second = runtime.page_events(&token, req).await.unwrap();
    assert_eq!(ids(&second), vec![2]);
    assert!(!second.has_more);
    assert!(second.next_after.is_none());
}

#[tokio::test]
async fn single_returned_row_exposes_a_cursor_at_that_row_only() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = token(&[]);
    seed(
        &runtime,
        &token,
        "local",
        vec![event("local", 1, 10), event("local", 2, 20)],
    )
    .await;
    let mut req = request();
    req.limit = 1;
    let one = runtime.page_events(&token, req.clone()).await.unwrap();
    let cursor = one.single_row_cursor.expect("one row returned");
    assert_eq!(decode_cursor(&cursor).unwrap().key, key(1, 10));
    assert_eq!(
        one.next_after.as_deref(),
        Some(cursor.as_str()),
        "both cursors sit at the only returned row"
    );
    req.limit = 2;
    let two = runtime.page_events(&token, req).await.unwrap();
    assert!(two.single_row_cursor.is_none());
}

#[test]
fn window_stop_must_follow_the_cursor_and_the_returned_rows_inside_the_window() {
    let row = EventPageRow {
        event: event("local", 5, 50),
        order_key: key(5, 50),
    };
    let window = |stop: Option<EventOrderKey>, rows: Vec<EventPageRow>| EventPageWindow {
        rows,
        budget_stop: stop,
    };
    let ok = window(Some(key(6, 60)), vec![row.clone()]);
    validate_window(&query(2), Some("local"), &ok).unwrap();
    validate_window(
        &query(2),
        Some("local"),
        &window(Some(key(6, 60)), Vec::new()),
    )
    .unwrap();
    for (name, bad) in [
        (
            "equal to last row",
            window(Some(key(5, 50)), vec![row.clone()]),
        ),
        (
            "before last row",
            window(Some(key(4, 50)), vec![row.clone()]),
        ),
        ("before since", window(Some(key(6, -1)), Vec::new())),
        ("at until", window(Some(key(6, 1000)), Vec::new())),
        (
            "invalid physical id",
            window(
                Some(EventOrderKey {
                    created_at_us: 60,
                    physical_id: "not-a-uuid".into(),
                }),
                Vec::new(),
            ),
        ),
    ] {
        assert!(
            validate_window(&query(2), Some("local"), &bad).is_err(),
            "{name}"
        );
    }
    assert!(
        validate_window(&query(1), Some("local"), &ok).is_err(),
        "a stopped window is shorter than the row bound"
    );
    let mut after = query(2);
    after.after = Some(key(6, 60));
    assert!(
        validate_window(&after, Some("local"), &window(Some(key(6, 60)), Vec::new())).is_err(),
        "a stop must follow the cursor strictly"
    );
}

#[tokio::test]
async fn split_stop_in_one_plane_bounds_the_other_plane_at_the_stop_key() {
    let plane = || {
        StorageBackend::memory()
            .unwrap()
            .events_for_namespace("local")
            .unwrap()
    };
    for stop_in_legacy in [true, false] {
        let (legacy, lane) = (plane(), plane());
        let (stopped, other) = if stop_in_legacy {
            (&legacy, &lane)
        } else {
            (&lane, &legacy)
        };
        stopped
            .append_events(vec![
                event("local", 1, 10),
                big_event("local", 3, 30, 1_100_000),
            ])
            .await
            .unwrap();
        other
            .append_events(vec![
                event("local", 2, 20),
                event("local", 4, 40),
                event("local", 5, 50),
            ])
            .await
            .unwrap();
        let split = SplitEventStore::new(legacy.clone(), lane.clone());

        let window = split.query_event_page(query(4)).await.unwrap();
        assert_eq!(
            window
                .rows
                .iter()
                .map(|r| r.event.id.as_u128())
                .collect::<Vec<_>>(),
            vec![1, 2],
            "rows after the stop key are unknown and are not returned"
        );
        assert_eq!(window.budget_stop, Some(key(3, 30)));

        let full = split.query_event_page(query(2)).await.unwrap();
        assert_eq!(full.rows.len(), 2);
        assert_eq!(full.budget_stop, None, "enough rows precede the stop");
    }
}

#[tokio::test]
async fn split_stop_key_equal_to_a_row_in_the_other_plane_is_an_invariant_error() {
    let plane = || {
        StorageBackend::memory()
            .unwrap()
            .events_for_namespace("local")
            .unwrap()
    };
    let (legacy, lane) = (plane(), plane());
    legacy
        .append_events(vec![big_event("local", 3, 30, 1_100_000)])
        .await
        .unwrap();
    lane.append_events(vec![event("local", 3, 30)])
        .await
        .unwrap();
    let split = SplitEventStore::new(legacy, lane);
    assert!(split.query_event_page(query(4)).await.is_err());
}
