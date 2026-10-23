use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::event::{EventGroupBy, EventStore};
use khive_storage::{BatchWriteSummary, Event, EventFilter, Page, PageRequest, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::event_counts_grouping::EventCountsAccumulator;
use crate::handlers::{fetch_event_counts_window, fold_event_counts_window_exhaustive};
use crate::BrainPack;

const CALLER: &str = "lambda:reader";
const SINCE: i64 = 1_000_000;
const UNTIL: i64 = 2_000_000;
const CLASSES: [&str; 12] = [
    "policy_refusal",
    "gate_refusal",
    "invalid_params",
    "not_found_or_not_addressed",
    "deadline_exceeded",
    "admission_exhausted",
    "conflict",
    "unsupported",
    "configuration",
    "storage",
    "unavailable",
    "internal",
];

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let mut config = RuntimeConfig {
        db_path: None,
        brain_profile: None,
        actor_id: Some(CALLER.into()),
        visible_namespaces: vec![Namespace::parse("lambda:visible").unwrap()],
        ..RuntimeConfig::no_embeddings()
    };
    config.brain.fleet_readers = vec![CALLER.into()];
    let runtime = KhiveRuntime::new(config).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(CALLER.into()));
    builder.with_visible_namespaces(runtime.config().visible_namespaces.clone());
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

fn window() -> Value {
    json!({"since": khive_runtime::micros_to_iso(SINCE),
        "until": khive_runtime::micros_to_iso(UNTIL)})
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

#[tokio::test]
async fn failed_audit_classes_exclude_success_and_other_kinds_without_relabeling_history() {
    let (runtime, registry) = fixture();
    for (i, class) in CLASSES.iter().enumerate() {
        let outcome = if i % 2 == 0 {
            EventOutcome::Error
        } else {
            EventOutcome::Denied
        };
        seed(
            &runtime,
            audit(outcome, json!({"error_class": class}), SINCE + i as i64),
        )
        .await;
    }
    let malformed = [
        json!({}),
        Value::Null,
        json!({"error_class": null}),
        json!({"error_class": 1}),
        json!({"error_class": {"name": "internal"}}),
        json!({"error_class": "future_class"}),
        json!({"error_class": "STORAGE"}),
        json!({"error_class": " storage "}),
    ];
    let mut preserved = Vec::new();
    for (i, payload) in malformed.into_iter().enumerate() {
        let id = seed(
            &runtime,
            audit(EventOutcome::Error, payload.clone(), SINCE + 20 + i as i64),
        )
        .await;
        preserved.push((id, payload));
    }
    for payload in [json!({"error_class": "storage"}), json!({"error_class": 3})] {
        seed(&runtime, audit(EventOutcome::Success, payload, SINCE + 40)).await;
    }
    let mut non_audit = audit(
        EventOutcome::Error,
        json!({"error_class": "internal", "result_kind": "note"}),
        SINCE + 50,
    );
    non_audit.kind = EventKind::SearchExecuted;
    seed(&runtime, non_audit).await;
    let expected: BTreeMap<_, _> = CLASSES.into_iter().map(|class| (class, 1)).collect();
    for exhaustive in [false, true] {
        let mut args = window();
        args["exhaustive"] = json!(exhaustive);
        let result = registry.dispatch("brain.event_counts", args).await.unwrap();
        assert_eq!(result["counts_by_error_class"], json!(expected));
        assert_eq!(result["unclassified_error_count"], 8);
        assert_eq!(
            result["counts_by_kind"],
            json!({"audit": 22, "search_executed": 1})
        );
        assert_eq!(result["window_event_total"], 23);
        assert_eq!(result["total"], 23);
        assert_eq!(result["truncated"], false);
        assert!(result.get("counts_by_error_class_page_scoped").is_none());
        assert!(result.get("unclassified_error_count_page_scoped").is_none());
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    for (id, payload) in preserved {
        assert_eq!(store.get_event(id).await.unwrap().unwrap().payload, payload);
    }
}

#[tokio::test]
async fn failed_audit_classes_keep_namespace_actor_alias_and_window_scope() {
    let (runtime, registry) = fixture();
    for (namespace, actor, class, at) in [
        ("local", CALLER, "storage", SINCE),
        ("local", "actor:lambda:reader", "conflict", SINCE + 1),
        ("local", "actor:lambda:visible", "gate_refusal", SINCE + 2),
        ("local", "actor:lambda:hidden", "invalid_params", SINCE + 3),
        ("lambda:visible", CALLER, "unavailable", SINCE + 4),
        ("local", CALLER, "internal", UNTIL),
    ] {
        let mut event = audit(EventOutcome::Error, json!({"error_class": class}), at);
        event.namespace = namespace.into();
        event.actor = actor.into();
        seed(&runtime, event).await;
    }
    let own = registry
        .dispatch("brain.event_counts", window())
        .await
        .unwrap();
    assert_eq!(
        own["counts_by_error_class"],
        json!({"storage": 1, "conflict": 1})
    );
    assert_eq!(own["counts_by_actor"], json!({CALLER: 2}));
    assert_eq!(own["unclassified_error_count"], 0);
    let mut args = window();
    args["actor"] = json!("lambda:visible");
    let other_actor = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(
        other_actor["counts_by_error_class"],
        json!({"gate_refusal": 1})
    );
    let mut args = window();
    args["namespace"] = json!("lambda:visible");
    let other_namespace = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(other_namespace["scope"]["namespace"], "lambda:visible");
    assert_eq!(
        other_namespace["counts_by_error_class"],
        json!({"unavailable": 1})
    );
    let mut args = window();
    args["all_actors"] = json!(true);
    let all = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(
        all["counts_by_error_class"],
        json!({"storage": 1, "conflict": 1, "gate_refusal": 1, "invalid_params": 1})
    );
    assert_eq!(all["total"], 4);
}

#[tokio::test]
async fn error_count_keys_are_absent_without_failed_audits() {
    let (runtime, registry) = fixture();
    for populated in [false, true] {
        if populated {
            seed(
                &runtime,
                audit(
                    EventOutcome::Success,
                    json!({"error_class": "internal"}),
                    SINCE,
                ),
            )
            .await;
            let mut non_audit = audit(
                EventOutcome::Error,
                json!({"error_class": "storage", "result_kind": "note"}),
                SINCE + 1,
            );
            non_audit.kind = EventKind::SearchExecuted;
            seed(&runtime, non_audit).await;
        }
        let result = registry
            .dispatch("brain.event_counts", window())
            .await
            .unwrap();
        assert_eq!(result["total"], if populated { 2 } else { 0 });
        for key in [
            "counts_by_error_class",
            "unclassified_error_count",
            "counts_by_error_class_page_scoped",
            "unclassified_error_count_page_scoped",
        ] {
            assert!(result.get(key).is_none(), "unexpected {key} in {result}");
        }
    }
    seed(&runtime, audit(EventOutcome::Denied, json!({}), SINCE + 2)).await;
    let historical = registry
        .dispatch("brain.event_counts", window())
        .await
        .unwrap();
    assert_eq!(historical["counts_by_error_class"], json!({}));
    assert_eq!(historical["unclassified_error_count"], 1);
    assert_eq!(historical["total"], 3);
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
    let (runtime, _) = fixture();
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
