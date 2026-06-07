use super::*;
use crate::pool::PoolConfig;
use serde_json::json;

fn setup_memory_store() -> SqlEventStore {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }

    SqlEventStore::new_scoped(pool, false, "default")
}

fn make_event(namespace: &str) -> Event {
    Event::new(
        namespace,
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    )
}

#[tokio::test]
async fn test_append_and_get_event() {
    let store = setup_memory_store();

    let event = make_event("default");
    let id = event.id;

    store.append_event(event).await.unwrap();

    let fetched = store.get_event(id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.verb, "search");
    assert_eq!(fetched.substrate, SubstrateKind::Note);
    assert_eq!(fetched.actor, "agent:test");
    assert_eq!(fetched.outcome, EventOutcome::Success);
}

#[tokio::test]
async fn test_append_events_batch() {
    let store = setup_memory_store();

    let events: Vec<Event> = (0..3).map(|_| make_event("default")).collect();
    let summary = store.append_events(events).await.unwrap();
    assert_eq!(summary.attempted, 3);
    assert_eq!(summary.affected, 3);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn test_count_events() {
    let store = setup_memory_store();

    for _ in 0..3 {
        store.append_event(make_event("default")).await.unwrap();
    }

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn test_query_events_filter_by_verb() {
    let store = setup_memory_store();

    store.append_event(make_event("default")).await.unwrap();

    let mut create_event = make_event("default");
    create_event.verb = "create".to_string();
    store.append_event(create_event).await.unwrap();

    let filter = EventFilter {
        verbs: vec!["search".to_string()],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].verb, "search");
}

#[tokio::test]
async fn test_query_events_filter_by_substrate() {
    let store = setup_memory_store();

    store.append_event(make_event("default")).await.unwrap();

    let mut entity_event = make_event("default");
    entity_event.substrate = SubstrateKind::Entity;
    store.append_event(entity_event).await.unwrap();

    let filter = EventFilter {
        substrates: vec![SubstrateKind::Entity],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].substrate, SubstrateKind::Entity);
}

#[tokio::test]
async fn test_outcome_roundtrip() {
    let store = setup_memory_store();

    let mut denied = make_event("default");
    denied.outcome = EventOutcome::Denied;
    let denied_id = denied.id;
    store.append_event(denied).await.unwrap();

    let fetched = store.get_event(denied_id).await.unwrap().unwrap();
    assert_eq!(fetched.outcome, EventOutcome::Denied);
}

#[tokio::test]
async fn append_event_writes_observations_atomically() {
    let store = setup_memory_store();
    let candidate = Uuid::new_v4();
    let selected = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [candidate.to_string()],
        "selected": [selected.to_string()],
        "served_by_profile_id": "profile-a"
    });
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    // Verify event was inserted.
    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_some());

    // Verify observations were written.
    let pool = Arc::clone(&store.pool);
    let event_id_str = event_id.to_string();
    let (candidate_count, selected_count) = tokio::task::spawn_blocking(move || {
            let guard = pool.reader().unwrap();
            let conn = guard.conn();
            let c: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM event_observations WHERE event_id = ?1 AND role = 'candidate'",
                    [&event_id_str],
                    |r| r.get(0),
                )
                .unwrap();
            let s: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM event_observations WHERE event_id = ?1 AND role = 'selected'",
                    [&event_id_str],
                    |r| r.get(0),
                )
                .unwrap();
            (c, s)
        })
        .await
        .unwrap();

    assert_eq!(candidate_count, 1, "expected one candidate observation row");
    assert_eq!(selected_count, 1, "expected one selected observation row");
}

#[tokio::test]
async fn invalid_projection_payload_aborts_event_insert() {
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    // "candidates" must be an array of UUID strings, not a plain string.
    event.payload = json!({ "candidates": "not-array" });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "invalid payload must return Err");

    // The event row must not exist — transaction was rolled back.
    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn query_events_orders_by_created_at_then_id_desc() {
    let store = setup_memory_store();

    let ts = chrono::Utc::now().timestamp_micros();
    let id_low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let id_high = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();

    // Insert both events with identical created_at via direct SQL to bypass UUID generation.
    let pool = Arc::clone(&store.pool);
    tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().unwrap();
            let conn = guard.conn();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            for id in [id_low, id_high] {
                conn.execute(
                    "INSERT INTO events \
                     (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                      payload_schema_version, duration_us, created_at) \
                     VALUES (?1, 'default', 'search', 'note', 'test', 'audit', 'success', '{}', 1, 0, ?2)",
                    rusqlite::params![id.to_string(), ts],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        })
        .await
        .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 2);
    assert_eq!(
        page.items[0].id, id_high,
        "higher UUID must come first (id DESC tiebreaker)"
    );
    assert_eq!(page.items[1].id, id_low);
}

#[tokio::test]
async fn query_events_filters_by_kind() {
    let store = setup_memory_store();
    store.append_event(make_event("default")).await.unwrap();
    let mut recall_event = make_event("default");
    recall_event.kind = EventKind::RecallExecuted;
    store.append_event(recall_event).await.unwrap();

    let filter = EventFilter {
        kinds: vec![EventKind::RecallExecuted],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].kind, EventKind::RecallExecuted);
}

#[tokio::test]
async fn query_events_filters_by_session_id() {
    let store = setup_memory_store();
    let session = Uuid::new_v4();
    let mut event = make_event("default");
    event.session_id = Some(session);
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        session_id: Some(session),
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].session_id, Some(session));
}

#[tokio::test]
async fn query_events_filters_by_observed() {
    let store = setup_memory_store();
    let entity_id = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [entity_id.to_string()],
        "selected": []
    });
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        observed: vec![entity_id],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn query_events_filters_by_selected() {
    let store = setup_memory_store();
    let entity_id = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "selected": [entity_id.to_string()]
    });
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        selected: vec![entity_id],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn query_events_filters_by_payload_proposal_id() {
    let store = setup_memory_store();
    let proposal_id = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::ProposalCreated;
    event.payload = json!({ "proposal_id": proposal_id.to_string() });
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        payload_proposal_id: Some(proposal_id),
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn query_events_observed_filter_missing_projection_returns_clean_error() {
    // Set up a legacy-schema store (no event_observations table).
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        // Create only the events table, without event_observations.
        writer.conn().execute_batch(
                "CREATE TABLE IF NOT EXISTS events (\
                     id TEXT PRIMARY KEY, namespace TEXT NOT NULL, verb TEXT NOT NULL,\
                     substrate TEXT NOT NULL, actor TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'audit',\
                     outcome TEXT NOT NULL, payload TEXT NOT NULL DEFAULT '{}',\
                     payload_schema_version INTEGER NOT NULL DEFAULT 1,\
                     duration_us INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL\
                 );"
            ).unwrap();
    }
    let store = SqlEventStore::new_scoped(pool, false, "default");

    let filter = EventFilter {
        observed: vec![Uuid::new_v4()],
        ..EventFilter::default()
    };
    let result = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("event_observations") && err_msg.contains("run migrations"),
        "error should mention event_observations and run migrations, got: {err_msg}"
    );
}
