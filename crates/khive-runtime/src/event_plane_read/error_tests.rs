use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::event::{EventGroupBy, EventStore};
use khive_storage::{BatchWriteSummary, Event, EventFilter, Page, PageRequest, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

use super::counts::{fetch_event_counts_window, fold_event_counts_window_exhaustive};
use super::grouping::EventCountsAccumulator;

const CALLER: &str = "lambda:reader";
const SINCE: i64 = 1_000_000;

fn fixture() -> KhiveRuntime {
    let mut config = RuntimeConfig {
        db_path: None,
        brain_profile: None,
        actor_id: Some(CALLER.into()),
        visible_namespaces: vec![Namespace::parse("lambda:visible").unwrap()],
        ..RuntimeConfig::no_embeddings()
    };
    config.brain.fleet_readers = vec![CALLER.into()];
    KhiveRuntime::new(config).unwrap()
}

fn audit(outcome: EventOutcome, payload: Value, at: i64) -> Event {
    let mut event = Event::new(
        "local",
        "test.audit",
        EventKind::Audit,
        SubstrateKind::Note,
        CALLER,
    )
    .with_outcome(outcome)
    .with_payload(payload);
    event.created_at = at;
    event
}

async fn seed(runtime: &KhiveRuntime, event: Event) -> Uuid {
    let id = event.id;
    // Preserve fixture actor and historical payload rather than restamping it.
    runtime
        .backend()
        .events_for_namespace(&event.namespace)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();
    id
}

// Reproduce a live-window shortfall: admission observes one more audit row than
// the following real store walk. This is a stale count, not a synthetic payload.
struct StaleTotal {
    inner: Arc<dyn EventStore>,
    queries: AtomicUsize,
}

#[async_trait::async_trait]
impl EventStore for StaleTotal {
    async fn append_event(&self, _: Event) -> StorageResult<()> {
        panic!("read-only wrapper")
    }
    async fn append_events(&self, _: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        panic!("read-only wrapper")
    }
    async fn get_event(&self, _: Uuid) -> StorageResult<Option<Event>> {
        panic!("walk must query pages")
    }
    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.query_events(filter, page).await
    }
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(filter).await
    }
    async fn count_events_grouped(
        &self,
        filter: EventFilter,
        group_by: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        assert!(matches!(group_by, EventGroupBy::Kind));
        let mut counts = self.inner.count_events_grouped(filter, group_by).await?;
        *counts.entry("audit".into()).or_default() += 1;
        Ok(counts)
    }
}

#[tokio::test]
async fn bounded_and_exhaustive_shortfalls_use_only_page_scoped_error_keys() {
    let runtime = fixture();
    for (outcome, payload, at) in [
        (
            EventOutcome::Error,
            json!({"error_class": "storage"}),
            SINCE + 3,
        ),
        (
            EventOutcome::Denied,
            json!({"error_class": "future_class"}),
            SINCE + 2,
        ),
        (
            EventOutcome::Success,
            json!({"error_class": "internal"}),
            SINCE + 1,
        ),
    ] {
        seed(&runtime, audit(outcome, payload, at)).await;
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let filter = EventFilter {
        kinds: vec![EventKind::Audit],
        ..EventFilter::default()
    };
    let (items, total, truncated) = fetch_event_counts_window(store.as_ref(), &filter, false, 1)
        .await
        .unwrap();
    assert_eq!((items.len(), total, truncated), (1, 3, true));
    let mut counts = EventCountsAccumulator::new(None, None);
    for event in &items {
        counts.observe(event);
    }
    let mut bounded = json!({});
    counts.add_to_result(&mut bounded, truncated);
    assert_eq!(
        bounded["counts_by_error_class_page_scoped"],
        json!({"storage": 1})
    );
    assert_eq!(bounded["unclassified_error_count_page_scoped"], 0);
    assert!(bounded.get("counts_by_error_class").is_none());
    assert!(bounded.get("unclassified_error_count").is_none());

    let stale = StaleTotal {
        inner: store.clone(),
        queries: AtomicUsize::new(0),
    };
    let mut counts = EventCountsAccumulator::new(None, None);
    let result = fold_event_counts_window_exhaustive(&stale, &filter, 2, 10, &mut counts)
        .await
        .unwrap();
    assert_eq!(result, (3, 4, true));
    assert!(
        stale.queries.load(Ordering::Relaxed) >= 2,
        "must fold more than one real page"
    );
    let mut folded = json!({});
    counts.add_to_result(&mut folded, result.2);
    assert_eq!(
        folded["counts_by_error_class_page_scoped"],
        json!({"storage": 1})
    );
    assert_eq!(folded["unclassified_error_count_page_scoped"], 1);
    assert!(folded.get("counts_by_error_class").is_none());
    assert!(folded.get("unclassified_error_count").is_none());
}
