use std::sync::Mutex;

use async_trait::async_trait;
use khive_runtime::{visit_events_cursor_walk, EventCursorWalkError};
use khive_storage::event::{Event, EventFilter, EventStore};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest, StorageResult};
use khive_storage::StorageError;
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use uuid::Uuid;

const CAP: u32 = khive_runtime::events_split::MAX_QUERY_EVENTS_PAGE_ROWS;

struct Fixture {
    events: Vec<Event>,
    base: EventFilter,
    queries: Mutex<Vec<(Option<i64>, u32)>>,
    counts: Mutex<Vec<Option<i64>>>,
    fail_query: bool,
    fail_count: bool,
}

impl Fixture {
    fn new(timestamps: &[i64], base: EventFilter) -> Self {
        assert!(timestamps.windows(2).all(|pair| pair[0] >= pair[1]));
        Self {
            events: timestamps
                .iter()
                .enumerate()
                .map(|(index, timestamp)| Event {
                    id: Uuid::from_u128(index as u128 + 1),
                    namespace: "local".to_string(),
                    verb: "cursor.fixture".to_string(),
                    substrate: SubstrateKind::Entity,
                    actor: "lambda:cursor-fixture".to_string(),
                    kind: EventKind::FeedbackExplicit,
                    outcome: EventOutcome::Success,
                    payload: serde_json::json!({"position": index}),
                    payload_schema_version: 1,
                    profile_state_version: None,
                    duration_us: 0,
                    target_id: None,
                    session_id: None,
                    aggregate_kind: None,
                    aggregate_id: None,
                    created_at: *timestamp,
                    op_index: None,
                    ref_resolution: None,
                })
                .collect(),
            base,
            queries: Mutex::new(Vec::new()),
            counts: Mutex::new(Vec::new()),
            fail_query: false,
            fail_count: false,
        }
    }

    fn matching(&self, filter: &EventFilter) -> impl Iterator<Item = &Event> {
        let after = filter.after;
        let before = filter.before;
        self.events.iter().filter(move |event| {
            after.is_none_or(|bound| event.created_at > bound)
                && before.is_none_or(|bound| event.created_at < bound)
        })
    }
}

#[async_trait]
impl EventStore for Fixture {
    async fn append_event(&self, _: Event) -> StorageResult<()> {
        panic!("cursor walks must not append")
    }

    async fn append_events(&self, _: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        panic!("cursor walks must not append")
    }

    async fn get_event(&self, _: Uuid) -> StorageResult<Option<Event>> {
        panic!("cursor walks must use the filtered pages")
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        assert_eq!(page.offset, 0);
        assert!((1..=CAP).contains(&page.limit));
        let mut expected = self.base.clone();
        expected.before = filter.before;
        assert_eq!(
            serde_json::to_value(&filter).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        {
            let mut queries = self.queries.lock().unwrap();
            assert!(
                queries.len() < 32,
                "a cursor regression must fail without looping"
            );
            queries.push((filter.before, page.limit));
        }
        if self.fail_query {
            return Err(StorageError::Timeout {
                operation: "query-fixture".into(),
            });
        }
        Ok(Page {
            items: self
                .matching(&filter)
                .take(page.limit as usize)
                .cloned()
                .collect(),
            total: None,
        })
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        let mut expected = self.base.clone();
        expected.after = filter.after;
        assert_eq!(
            serde_json::to_value(&filter).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        self.counts.lock().unwrap().push(filter.after);
        if self.fail_count {
            return Err(StorageError::Timeout {
                operation: "count-fixture".into(),
            });
        }
        Ok(self.matching(&filter).count() as u64)
    }
}

#[tokio::test]
async fn shared_cursor_preserves_boundaries_order_budgets_and_tie_refusals() {
    let base = EventFilter {
        actors: vec!["lambda:cursor-fixture".to_string()],
        verbs: vec!["cursor.fixture".to_string()],
        kinds: vec![EventKind::FeedbackExplicit],
        after: Some(3),
        before: Some(10),
        ..EventFilter::default()
    };
    for budget in [0, 1, 4, 20] {
        let store = Fixture::new(&[9, 8, 8, 8, 5, 4], base.clone());
        let mut ids = Vec::new();
        let admitted = visit_events_cursor_walk(&store, &base, 2, budget, |event| {
            ids.push(event.id.as_u128());
        })
        .await
        .unwrap();
        let expected: Vec<u128> = (1..=6).take(budget as usize).collect();
        assert_eq!(ids, expected);
        assert_eq!(admitted as usize, expected.len());
        let all_queries = [
            (Some(10), 2),
            (Some(9), 2),
            (Some(9), 2),
            (Some(9), 4),
            (Some(6), 4),
        ];
        let reads = match budget {
            0 => 0,
            1 => 1,
            4 => 4,
            _ => 5,
        };
        assert_eq!(*store.queries.lock().unwrap(), all_queries[..reads]);
        assert!(store.counts.lock().unwrap().is_empty());
    }

    let base = EventFilter::default();
    let store = Fixture::new(&[5, 4], base.clone());
    let mut ids = Vec::new();
    assert_eq!(
        visit_events_cursor_walk(&store, &base, 0, 2, |event| {
            ids.push(event.id.as_u128());
        })
        .await
        .unwrap(),
        2
    );
    assert_eq!(ids, [1, 2]);
    assert_eq!(
        *store.queries.lock().unwrap(),
        [(None, 1), (Some(6), 1), (Some(6), 2)]
    );

    for boundary in [i64::MIN, i64::MAX] {
        let mut times = vec![boundary; CAP as usize];
        if boundary == i64::MAX {
            times.push(i64::MIN);
        }
        let store = Fixture::new(&times, base.clone());
        let mut ids = Vec::new();
        let admitted = visit_events_cursor_walk(&store, &base, u32::MAX, u64::MAX, |event| {
            ids.push(event.id.as_u128());
        })
        .await
        .unwrap();
        assert_eq!(admitted as usize, times.len());
        assert_eq!(ids, (1..=times.len() as u128).collect::<Vec<_>>());
        let repeated_before = if boundary == i64::MAX {
            None
        } else {
            Some(i64::MIN + 1)
        };
        assert_eq!(
            *store.queries.lock().unwrap(),
            [(None, CAP), (repeated_before, CAP), (Some(boundary), CAP)]
        );
        let counted_after = if boundary == i64::MAX {
            Some(i64::MAX - 1)
        } else {
            None
        };
        assert_eq!(*store.counts.lock().unwrap(), [counted_after]);
    }

    let store = Fixture::new(&vec![7; CAP as usize + 1], base.clone());
    let mut ids = Vec::new();
    let error = visit_events_cursor_walk(&store, &base, CAP, u64::MAX, |event| {
        ids.push(event.id.as_u128());
    })
    .await
    .unwrap_err();
    assert!(
        matches!(error, EventCursorWalkError::DenseTimestampTie { page_limit } if page_limit == CAP)
    );
    assert_eq!(ids, (1..=u128::from(CAP)).collect::<Vec<_>>());
    assert_eq!(
        *store.queries.lock().unwrap(),
        [(None, CAP), (Some(8), CAP)]
    );
    assert_eq!(*store.counts.lock().unwrap(), [Some(6)]);

    for fail_query in [true, false] {
        let mut store = Fixture::new(&vec![7; CAP as usize], base.clone());
        store.fail_query = fail_query;
        store.fail_count = !fail_query;
        let mut visited = 0;
        let error = visit_events_cursor_walk(&store, &base, CAP, u64::MAX, |_| {
            visited += 1;
        })
        .await
        .unwrap_err();
        let expected_operation = if fail_query {
            "query-fixture"
        } else {
            "count-fixture"
        };
        assert!(
            matches!(error, EventCursorWalkError::Storage(StorageError::Timeout { operation }) if operation == expected_operation)
        );
        assert_eq!(visited, if fail_query { 0 } else { CAP });
        assert_eq!(
            store.queries.lock().unwrap().len(),
            if fail_query { 1 } else { 2 }
        );
        assert_eq!(store.counts.lock().unwrap().len(), usize::from(!fail_query));
    }
}
