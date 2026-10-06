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
            &EventPageWindow { rows: vec![bad] }
        )
        .is_err());
    }
    let window = EventPageWindow {
        rows: vec![row.clone()],
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
            rows: vec![window.rows[0].clone(); 2]
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
            rows: vec![row.clone(), row]
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
        events_split: Some(EventsSplitConfig {
            db_path: lane_path.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = token(&["team"]);
    seed(&runtime, &token, "local", vec![event("local", 1, 10)]).await;
    let lane = crate::events_split::direct_backend_for(&lane_path).unwrap();
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
