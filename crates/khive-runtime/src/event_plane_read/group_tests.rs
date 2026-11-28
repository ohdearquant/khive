use super::counts::{fetch_event_counts_window, fetch_event_counts_window_exhaustive};
use super::grouping::EventCountGroupBy;
use super::test_support::cross_sum;
use crate::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::json;

const CALLER: &str = "lambda:reader";
const SINCE: i64 = 1_000_000;
const UNTIL: i64 = 2_000_000;

fn fixture(fleet_reader: bool) -> KhiveRuntime {
    let mut config = RuntimeConfig {
        db_path: None,
        brain_profile: None,
        actor_id: Some(CALLER.into()),
        visible_namespaces: vec![Namespace::parse("lambda:visible").unwrap()],
        ..RuntimeConfig::no_embeddings()
    };
    if fleet_reader {
        config.brain.fleet_readers = vec![CALLER.into()];
    }
    KhiveRuntime::new(config).unwrap()
}

async fn seed(
    runtime: &KhiveRuntime,
    namespace: &str,
    verb: &str,
    actor: &str,
    kind: EventKind,
    time: i64,
) {
    let mut event = Event::new(namespace, verb, kind, SubstrateKind::Note, actor);
    event.created_at = time;
    if kind == EventKind::SearchExecuted {
        event.payload = json!({"result_kind": "note"});
    }
    // Trusted fixture insertion preserves historical actor labels exactly.
    runtime
        .backend()
        .events_for_namespace(namespace)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();
}

/// Default grouped refusal must be propagated before any fallback or row read.
struct UngroupedEvents;

#[async_trait::async_trait]
impl khive_storage::event::EventStore for UngroupedEvents {
    async fn append_event(&self, _: Event) -> khive_storage::StorageResult<()> {
        panic!("read-only refusal fixture")
    }
    async fn append_events(
        &self,
        _: Vec<Event>,
    ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
        panic!("read-only refusal fixture")
    }
    async fn get_event(&self, _: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
        panic!("grouped refusal must precede row reads")
    }
    async fn query_events(
        &self,
        _: EventFilter,
        _: khive_storage::PageRequest,
    ) -> khive_storage::StorageResult<khive_storage::Page<Event>> {
        panic!("grouped refusal must precede row reads")
    }
    async fn count_events(&self, _: EventFilter) -> khive_storage::StorageResult<u64> {
        panic!("grouped refusal must not fall back to scalar counts")
    }
}

#[tokio::test]
async fn cross_uses_the_sampled_and_exhaustive_event_windows_including_audit_segregation() {
    let runtime = fixture(true);
    for offset in 0..5 {
        seed(
            &runtime,
            "local",
            "audit.verb",
            &format!("actor:sample:{offset}"),
            EventKind::Audit,
            SINCE + 10 + offset,
        )
        .await;
    }
    for offset in 0..2 {
        seed(
            &runtime,
            "local",
            "quiet.search",
            "actor:quiet",
            EventKind::SearchExecuted,
            SINCE + offset,
        )
        .await;
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let filter = EventFilter {
        after: Some(SINCE - 1),
        before: Some(UNTIL),
        ..Default::default()
    };
    let (sample, total, truncated) = fetch_event_counts_window(store.as_ref(), &filter, true, 2)
        .await
        .unwrap();
    assert_eq!(total, 7);
    assert!(truncated);
    let mut result = json!({});
    EventCountGroupBy::VerbActor.add_to_result(&mut result, &sample, None, truncated);
    assert!(result.get("counts_by_verb_and_actor").is_none());
    assert_eq!(
        result["counts_by_verb_and_actor_page_scoped"],
        json!({
            "audit.verb": {"actor:sample:4": 1, "actor:sample:3": 1},
            "quiet.search": {"actor:quiet": 2},
        })
    );
    assert_eq!(
        cross_sum(&result["counts_by_verb_and_actor_page_scoped"]),
        sample.len() as u64
    );
    let cells: usize = result["counts_by_verb_and_actor_page_scoped"]
        .as_object()
        .unwrap()
        .values()
        .map(|actors| actors.as_object().unwrap().len())
        .sum();
    assert!(
        cells <= sample.len(),
        "a cross cannot invent unobserved cells"
    );

    let audit_filter = EventFilter {
        kinds: vec![EventKind::Audit],
        ..filter.clone()
    };
    let (audit, total, truncated) =
        fetch_event_counts_window(store.as_ref(), &audit_filter, false, 2)
            .await
            .unwrap();
    assert_eq!(total, 5);
    assert!(truncated);
    let mut audit_result = json!({});
    EventCountGroupBy::VerbActor.add_to_result(&mut audit_result, &audit, None, truncated);
    assert!(audit_result["counts_by_verb_and_actor_page_scoped"]
        .get("quiet.search")
        .is_none());
    assert_eq!(
        cross_sum(&audit_result["counts_by_verb_and_actor_page_scoped"]),
        2
    );

    let (all, total, truncated) =
        fetch_event_counts_window_exhaustive(store.as_ref(), &filter, 2, 20)
            .await
            .unwrap();
    assert_eq!(total, 7);
    assert!(!truncated);
    let mut complete = json!({});
    EventCountGroupBy::VerbActor.add_to_result(&mut complete, &all, None, truncated);
    assert!(complete
        .get("counts_by_verb_and_actor_page_scoped")
        .is_none());
    assert_eq!(cross_sum(&complete["counts_by_verb_and_actor"]), 7);
    assert_eq!(
        complete["counts_by_verb_and_actor"]["quiet.search"]["actor:quiet"],
        2
    );
}

#[tokio::test]
async fn unsupported_grouped_admission_refuses_without_scalar_or_row_fallback() {
    let expected = "unsupported operation for Events: count_events_grouped (this EventStore backend does not implement grouped counts)";
    for unfiltered in [false, true] {
        let error =
            fetch_event_counts_window(&UngroupedEvents, &EventFilter::default(), unfiltered, 1)
                .await
                .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(message) if message == expected));
    }
    let error =
        fetch_event_counts_window_exhaustive(&UngroupedEvents, &EventFilter::default(), 1, 1)
            .await
            .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(message) if message == expected));
}
