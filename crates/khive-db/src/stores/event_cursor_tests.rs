use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use khive_storage::event::{Event, EventFilter};
use khive_storage::{BatchWriteSummary, EventStore, Page, PageRequest};
use khive_types::{EventKind, SubstrateKind};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use serde_json::json;

use super::*;
use crate::pool::{ConnectionPool, PoolConfig};

fn store() -> SqlEventStore {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(super::super::EVENTS_DDL)
        .unwrap();
    assert_eq!(
        pool.max_readers(),
        0,
        "fixture uses the shared in-memory reader"
    );
    SqlEventStore::new_scoped(pool, false, "alpha")
}

fn event(id: u128, at: i64, namespace: &str, kind: EventKind, actor: &str) -> Event {
    let mut event = Event::new(
        namespace,
        "fixture.event",
        kind,
        SubstrateKind::Event,
        actor,
    )
    .with_payload(json!({"ordinal": id.to_string(), "nested": [true, null, "kept"]}))
    .with_profile_state_version(7)
    .with_duration_us(23);
    event.id = Uuid::from_u128(id);
    event.created_at = at;
    event
}

fn query(max_rows: u32) -> EventPageQuery {
    EventPageQuery {
        since_us: 0,
        until_us: 100,
        kinds: vec![],
        actors: vec![],
        exclude_namespaces: vec![],
        after: None,
        max_rows,
    }
}

async fn walk(store: &SqlEventStore, mut query: EventPageQuery) -> Vec<EventPageRow> {
    let mut found: Vec<EventPageRow> = Vec::new();
    loop {
        let window = store.query_event_page(query.clone()).await.unwrap();
        assert!(window.rows.len() <= query.max_rows as usize);
        if window.rows.is_empty() {
            return found;
        }
        for row in &window.rows {
            if let Some(previous) = found.last() {
                assert!(
                    (
                        previous.order_key.created_at_us,
                        previous.order_key.physical_id.as_bytes()
                    ) < (
                        row.order_key.created_at_us,
                        row.order_key.physical_id.as_bytes()
                    )
                );
            }
            found.push(row.clone());
        }
        query.after = found.last().map(|row| row.order_key.clone());
        assert!(found.len() <= 5000, "seek did not advance");
    }
}

#[tokio::test]
async fn page_filters_before_limit_and_retains_complete_rows() {
    let store = store();
    let expected: Vec<_> = (0..5)
        .map(|index| {
            event(
                1000 + index,
                if index == 0 { 0 } else { 20 + index as i64 },
                "alpha",
                EventKind::Audit,
                "chosen",
            )
        })
        .collect();
    let mut seeded = expected.clone();
    for index in 1..=150 {
        seeded.push(event(
            index,
            1,
            "alpha",
            EventKind::SearchExecuted,
            "chosen",
        ));
        seeded.push(event(200 + index, 2, "alpha", EventKind::Audit, "other"));
        seeded.push(event(400 + index, 3, "beta", EventKind::Audit, "chosen"));
    }
    seeded.push(event(900, -1, "alpha", EventKind::Audit, "chosen"));
    seeded.push(event(901, 100, "alpha", EventKind::Audit, "chosen"));
    store.append_events(seeded).await.unwrap();
    let mut selected = query(2);
    selected.kinds = vec![EventKind::Audit];
    selected.actors = vec!["chosen".into()];
    selected.exclude_namespaces = vec!["beta".into()];
    let actual = walk(&store, selected.clone()).await;
    assert_eq!(
        actual
            .iter()
            .map(|row| row.event.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        store
            .count_events(EventFilter {
                kinds: selected.kinds.clone(),
                actors: selected.actors.clone(),
                after: Some(-1),
                before: Some(100),
                ..EventFilter::default()
            })
            .await
            .unwrap(),
        actual.len() as u64
    );
    selected.exclude_namespaces.push("alpha".into());
    assert!(store
        .query_event_page(selected)
        .await
        .unwrap()
        .rows
        .is_empty());
}

#[tokio::test]
async fn page_walks_more_than_4096_equal_time_rows() {
    let store = store();
    let expected: Vec<_> = (1..=4105)
        .map(|id| event(id, 10, "alpha", EventKind::Audit, "chosen"))
        .collect();
    store.append_events(expected.clone()).await.unwrap();
    let rows = walk(&store, query(127)).await;
    assert_eq!(
        rows.iter().map(|row| row.event.clone()).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        store.count_events(EventFilter::default()).await.unwrap(),
        4105
    );
}

#[tokio::test]
async fn page_preserves_binary_physical_uuid_order() {
    let store = store();
    let physical = [
        "AAAAAAAA-0000-0000-0000-000000000001",
        "bbbbbbbb000000000000000000000002",
        "{cccccccc-0000-0000-0000-000000000003}",
        "urn:uuid:dddddddd-0000-0000-0000-000000000004",
        "eeeeeeee-0000-0000-0000-000000000005",
    ];
    let mut expected = Vec::new();
    for text in physical {
        let mut value = event(1, 10, "alpha", EventKind::Audit, "chosen");
        value.id = Uuid::parse_str(text).unwrap();
        store.append_event(value.clone()).await.unwrap();
        store
            .pool
            .writer()
            .unwrap()
            .conn()
            .execute(
                "UPDATE events SET id = ?1 WHERE id = ?2",
                rusqlite::params![text, value.id.to_string()],
            )
            .unwrap();
        expected.push((text.to_string(), value));
    }
    expected.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let actual = walk(&store, query(1)).await;
    assert_eq!(actual.len(), expected.len());
    for (row, (physical_id, event)) in actual.into_iter().zip(expected) {
        assert_eq!(row.order_key.physical_id, physical_id);
        assert_eq!(row.event, event);
    }
}

#[tokio::test]
async fn page_observes_later_keys_without_claiming_a_snapshot() {
    let store = store();
    store
        .append_events(vec![
            event(10, 10, "alpha", EventKind::Audit, "chosen"),
            event(30, 30, "alpha", EventKind::Audit, "chosen"),
        ])
        .await
        .unwrap();
    let first = store.query_event_page(query(1)).await.unwrap();
    assert_eq!(first.rows[0].event.id, Uuid::from_u128(10));
    store
        .append_events(vec![
            event(20, 20, "alpha", EventKind::Audit, "chosen"),
            event(9, 9, "alpha", EventKind::Audit, "chosen"),
            event(1, 10, "alpha", EventKind::Audit, "chosen"),
            event(100, 100, "alpha", EventKind::Audit, "chosen"),
        ])
        .await
        .unwrap();
    let mut resumed = query(1);
    resumed.after = Some(first.rows[0].order_key.clone());
    assert_eq!(
        walk(&store, resumed)
            .await
            .iter()
            .map(|row| row.event.id)
            .collect::<Vec<_>>(),
        vec![Uuid::from_u128(20), Uuid::from_u128(30)]
    );
}

#[tokio::test]
async fn invalid_page_bounds_refuse_before_any_table_read() {
    let store = store();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    store
        .pool
        .writer()
        .unwrap()
        .conn()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Read { .. }) {
                observed.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let mut cases = vec![query(0), query(4097)];
    let mut reversed = query(1);
    reversed.since_us = reversed.until_us;
    cases.push(reversed);
    let mut exclusions = query(1);
    exclusions.exclude_namespaces = vec!["excluded".into(); 33];
    cases.push(exclusions);
    for (time, id) in [
        (-1, Uuid::from_u128(1).to_string()),
        (100, Uuid::from_u128(1).to_string()),
        (1, "short".into()),
    ] {
        let mut cursor = query(1);
        cursor.after = Some(EventOrderKey {
            created_at_us: time,
            physical_id: id,
        });
        cases.push(cursor);
    }
    for request in cases {
        let error = store.query_event_page(request).await.unwrap_err();
        assert!(
            matches!(error, StorageError::InvalidInput { .. }),
            "{error}"
        );
        assert!(!error.is_retryable());
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(store.query_event_page(query(1)).await.is_err());
    assert!(
        reads.load(Ordering::SeqCst) > 0,
        "read-denial control was not active"
    );
}

fn stop_key(id: u128, at: i64) -> EventOrderKey {
    EventOrderKey {
        created_at_us: at,
        physical_id: Uuid::from_u128(id).to_string(),
    }
}

async fn drop_payload_index(store: &SqlEventStore) {
    // Admit a deliberately corrupt payload without the unrelated JSON expression index.
    store
        .pool
        .writer()
        .unwrap()
        .conn()
        .execute_batch("DROP INDEX idx_events_payload_proposal_id")
        .unwrap();
}

#[tokio::test]
async fn page_stops_before_an_oversized_row_without_decoding_it() {
    let store = store();
    let first = event(1, 1, "alpha", EventKind::Audit, "chosen");
    store
        .append_events(vec![
            first.clone(),
            event(2, 2, "alpha", EventKind::Audit, "chosen"),
        ])
        .await
        .unwrap();
    drop_payload_index(&store).await;
    store
        .pool
        .writer()
        .unwrap()
        .conn()
        .execute(
            "UPDATE events SET payload = ?1 WHERE id = ?2",
            rusqlite::params![
                "!".repeat(MAX_RAW_TEXT_BYTES + 1),
                Uuid::from_u128(2).to_string()
            ],
        )
        .unwrap();
    let window = store.query_event_page(query(2)).await.unwrap();
    assert_eq!(window.rows.len(), 1);
    assert_eq!(window.rows[0].event, first);
    assert_eq!(window.budget_stop, Some(stop_key(2, 2)));

    let mut resumed = query(1);
    resumed.after = Some(window.rows[0].order_key.clone());
    let window = store.query_event_page(resumed).await.unwrap();
    assert!(window.rows.is_empty());
    assert_eq!(window.budget_stop, Some(stop_key(2, 2)));

    store
        .pool
        .writer()
        .unwrap()
        .conn()
        .execute(
            "UPDATE events SET payload = '{}', actor = ?1 WHERE id = ?2",
            rusqlite::params![
                "a".repeat(MAX_RAW_TEXT_BYTES + 1),
                Uuid::from_u128(2).to_string()
            ],
        )
        .unwrap();
    let window = store.query_event_page(query(2)).await.unwrap();
    assert_eq!(window.rows.len(), 1);
    assert_eq!(window.budget_stop, Some(stop_key(2, 2)));
}

#[tokio::test]
async fn page_without_a_stop_reports_none() {
    let store = store();
    store
        .append_events(vec![event(1, 1, "alpha", EventKind::Audit, "chosen")])
        .await
        .unwrap();
    let window = store.query_event_page(query(4)).await.unwrap();
    assert_eq!(window.rows.len(), 1);
    assert_eq!(window.budget_stop, None);
}

#[tokio::test]
async fn page_source_byte_budget_is_cumulative_and_names_the_blocking_row() {
    let store = store();
    let mut values = vec![
        event(1, 1, "alpha", EventKind::Audit, "chosen"),
        event(2, 2, "alpha", EventKind::Audit, "chosen"),
    ];
    for value in &mut values {
        value.payload = json!({"large": "x".repeat(600_000)});
    }
    store.append_events(values.clone()).await.unwrap();
    let first = store.query_event_page(query(1)).await.unwrap();
    assert_eq!(first.rows[0].event, values[0]);
    assert_eq!(
        first.budget_stop, None,
        "a full window is not a stopped one"
    );
    let both = store.query_event_page(query(2)).await.unwrap();
    assert_eq!(both.rows.len(), 1);
    assert_eq!(both.rows[0].event, values[0]);
    assert_eq!(both.budget_stop, Some(stop_key(2, 2)));
    let mut resumed = query(1);
    resumed.after = Some(first.rows[0].order_key.clone());
    let second = store.query_event_page(resumed).await.unwrap();
    assert_eq!(second.rows[0].event, values[1]);
    assert_eq!(second.budget_stop, None);
}

#[tokio::test]
async fn page_refuses_an_oversized_row_whose_ordering_key_is_invalid() {
    let store = store();
    store
        .append_events(vec![event(1, 1, "alpha", EventKind::Audit, "chosen")])
        .await
        .unwrap();
    drop_payload_index(&store).await;
    store
        .pool
        .writer()
        .unwrap()
        .conn()
        .execute(
            "UPDATE events SET payload = ?1, id = 'not-a-uuid' WHERE id = ?2",
            rusqlite::params![
                "!".repeat(MAX_RAW_TEXT_BYTES + 1),
                Uuid::from_u128(1).to_string()
            ],
        )
        .unwrap();
    let error = store.query_event_page(query(2)).await.unwrap_err();
    assert!(matches!(error, StorageError::InvalidInput { message, .. }
        if message == "invalid stored event page ordering key"));
}

#[tokio::test]
async fn page_uses_no_count_or_offset_and_does_not_write() {
    let store = store();
    store
        .append_event(event(1, 1, "alpha", EventKind::Audit, "chosen"))
        .await
        .unwrap();
    let counts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&counts);
    let writes = Arc::new(AtomicUsize::new(0));
    let observed_writes = Arc::clone(&writes);
    store
        .pool
        .writer()
        .unwrap()
        .conn()
        .authorizer(Some(move |context: AuthContext<'_>| match context.action {
            AuthAction::Function { function_name }
                if function_name.eq_ignore_ascii_case("count") =>
            {
                observed.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            }
            AuthAction::Insert { .. } | AuthAction::Update { .. } | AuthAction::Delete { .. } => {
                observed_writes.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }))
        .unwrap();
    let page = store.query_event_page(query(1)).await.unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(counts.load(Ordering::SeqCst), 0);
    assert!(store.count_events(EventFilter::default()).await.is_err());
    assert_eq!(
        counts.load(Ordering::SeqCst),
        1,
        "COUNT-denial control was not active"
    );
    let guard = store.pool.writer().unwrap();
    let error = guard
        .conn()
        .execute(
            "DELETE FROM events WHERE id = ?1",
            rusqlite::params![Uuid::from_u128(1).to_string()],
        )
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::AuthorizationForStatementDenied)
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "write-denial control was not active"
    );
    let mut program = guard.conn().prepare(&format!("EXPLAIN {QUERY}")).unwrap();
    let opcodes: Vec<String> = program
        .query_map(
            rusqlite::params![
                "alpha",
                0_i64,
                100_i64,
                "[]",
                "[]",
                "[]",
                Option::<i64>::None,
                Option::<String>::None,
                1_i64
            ],
            |row| row.get(1),
        )
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!opcodes.iter().any(|opcode| opcode == "OffsetLimit"));
    assert!(opcodes.iter().any(|opcode| opcode == "ResultRow"));
}

struct LegacyStore;

#[async_trait]
impl EventStore for LegacyStore {
    async fn append_event(&self, _: Event) -> Result<(), StorageError> {
        unreachable!()
    }
    async fn append_events(&self, _: Vec<Event>) -> Result<BatchWriteSummary, StorageError> {
        unreachable!()
    }
    async fn get_event(&self, _: Uuid) -> Result<Option<Event>, StorageError> {
        unreachable!()
    }
    async fn query_events(
        &self,
        _: EventFilter,
        _: PageRequest,
    ) -> Result<Page<Event>, StorageError> {
        panic!("page must never downgrade to offset query")
    }
    async fn count_events(&self, _: EventFilter) -> Result<u64, StorageError> {
        panic!("page must never count")
    }
}

#[tokio::test]
async fn legacy_backend_refuses_page_without_a_query_fallback() {
    let error = LegacyStore.query_event_page(query(1)).await.unwrap_err();
    assert!(matches!(
        error,
        StorageError::Unsupported { capability: StorageCapability::Events, operation, .. }
            if operation == OPERATION
    ));
}
