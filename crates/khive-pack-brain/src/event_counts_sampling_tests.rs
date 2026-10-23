use std::collections::BTreeMap;

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::event_counts_grouping::{EventCountGroupBy, EventCountsAccumulator};
use crate::handlers::fetch_event_counts_window;
use crate::BrainPack;

const CALLER: &str = "reader:primary";
const PRIMARY: &str = "actor:reader:primary";
const SECONDARY: &str = "actor:reader:secondary";
const SINCE: i64 = 1_000_000;
const UNTIL: i64 = 2_000_000;

fn sampling(every: u64, seq: u64, selected_by: &str) -> Value {
    json!({"every": every, "seq": seq, "selected_by": selected_by})
}

fn row(
    verb: &str,
    kind: EventKind,
    outcome: EventOutcome,
    object: Option<Value>,
    at: i64,
) -> Event {
    let mut resource = json!({"cost_unit": 1, "work_class": "interactive"});
    if let Some(object) = object {
        resource["sampling"] = object;
    }
    let mut event = Event::new("local", verb, kind, SubstrateKind::Event, PRIMARY)
        .with_outcome(outcome)
        .with_payload(json!({"resource": resource}))
        .with_duration_us(7);
    event.created_at = at;
    event.op_index = None;
    event.ref_resolution = None;
    if kind == EventKind::SearchExecuted {
        event.payload["result_kind"] = json!("note");
    }
    event
}

fn fixture_rows() -> Vec<Event> {
    let mut rows = Vec::new();
    for i in 0..100 {
        rows.push(row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            None,
            SINCE + i,
        ));
    }
    for (i, seq) in [0, 2, 8, 12, 34, 47, 77, 90].into_iter().enumerate() {
        rows.push(row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            Some(sampling(10, seq, "sample")),
            SINCE + 200 + i as i64,
        ));
    }
    for i in 0..2 {
        rows.push(row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            Some(sampling(10, i, "forced")),
            SINCE + 210 + i as i64,
        ));
    }
    for (i, object) in [
        Value::Null,
        json!({"every":20,"selected_by":"sample"}),
        sampling(10, 0, "other"),
        sampling(0, 0, "sample"),
    ]
    .into_iter()
    .enumerate()
    {
        rows.push(row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            Some(object),
            SINCE + 220 + i as i64,
        ));
    }
    for (i, object) in [
        None,
        Some(sampling(10, 0, "sample")),
        Some(sampling(10, 1, "forced")),
    ]
    .into_iter()
    .enumerate()
    {
        rows.push(row(
            "comm.cleanup_expired_quarantine",
            EventKind::Audit,
            EventOutcome::Success,
            object,
            SINCE + 300 + i as i64,
        ));
    }
    for (verb, kind, outcome) in [
        ("comm.heartbeat", EventKind::Audit, EventOutcome::Error),
        (
            "comm.cleanup_expired_quarantine",
            EventKind::Audit,
            EventOutcome::Denied,
        ),
        (
            "comm.heartbeat",
            EventKind::SearchExecuted,
            EventOutcome::Success,
        ),
        ("stats", EventKind::Audit, EventOutcome::Success),
    ] {
        let mut event = row(
            verb,
            kind,
            outcome,
            Some(sampling(10, 0, "sample")),
            SINCE + 400,
        );
        if outcome == EventOutcome::Error {
            event.payload["error_class"] = json!("storage");
        }
        if outcome == EventOutcome::Denied {
            event.payload["error_class"] = json!("gate_refusal");
        }
        rows.push(event);
    }
    let mut feedback = row(
        "brain.feedback",
        EventKind::FeedbackExplicit,
        EventOutcome::Success,
        None,
        SINCE + 401,
    );
    feedback.payload = json!({"resource":{"cost_unit":3,"work_class":"warm"},"originating_verb":"search","served_by_profile_id":"profile-a","signal":"useful"});
    rows.push(feedback);
    for (verb, selected) in [
        ("comm.heartbeat", "sample"),
        ("comm.cleanup_expired_quarantine", "forced"),
    ] {
        let mut event = row(
            verb,
            EventKind::Audit,
            EventOutcome::Success,
            Some(sampling(20, 0, selected)),
            SINCE + 500,
        );
        event.actor = SECONDARY.into();
        rows.push(event);
    }
    for (verb, actor, every) in [
        ("comm.heartbeat", PRIMARY, 10),
        ("comm.cleanup_expired_quarantine", SECONDARY, 20),
    ] {
        let mut event = row(
            verb,
            EventKind::Audit,
            EventOutcome::Success,
            Some(sampling(every, 0, "sample")),
            SINCE + 600,
        );
        event.namespace = "research".into();
        event.actor = actor.into();
        rows.push(event);
    }
    for at in [SINCE - 1, UNTIL] {
        rows.push(row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            Some(sampling(99, 0, "sample")),
            at,
        ));
    }
    for (i, event) in rows.iter_mut().enumerate() {
        event.id = Uuid::from_u128(i as u128 + 1);
    }
    rows
}

fn window(extra: Value) -> Value {
    let mut value = json!({"since":khive_runtime::micros_to_iso(SINCE),"until":khive_runtime::micros_to_iso(UNTIL),"group_by":["verb","actor"]});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    value
}

async fn capture_fixture() -> Value {
    let mut config = RuntimeConfig {
        db_path: None,
        brain_profile: None,
        actor_id: Some(CALLER.into()),
        visible_namespaces: vec![
            Namespace::parse("research").unwrap(),
            Namespace::parse("reader:secondary").unwrap(),
        ],
        ..RuntimeConfig::no_embeddings()
    };
    config.brain.fleet_readers = vec![CALLER.into()];
    let runtime = KhiveRuntime::new(config).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder
        .with_actor_id(Some(CALLER.into()))
        .with_visible_namespaces(runtime.config().visible_namespaces.clone());
    builder
        .register(KgPack::new(runtime.clone()))
        .register(BrainPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    let rows = fixture_rows();
    for event in &rows {
        runtime
            .backend()
            .events_for_namespace(&event.namespace)
            .unwrap()
            .append_event(event.clone())
            .await
            .unwrap();
    }
    let mut results = BTreeMap::new();
    for (name, extra) in [
        ("own", json!({})),
        ("exhaustive", json!({"exhaustive":true})),
        ("all", json!({"all_actors":true})),
        ("secondary", json!({"actor":"reader:secondary"})),
        ("research", json!({"namespace":"research"})),
        (
            "research_all",
            json!({"namespace":"research","all_actors":true}),
        ),
        ("audit", json!({"kind":"audit"})),
        ("search", json!({"kind":"search_executed"})),
        (
            "narrow",
            json!({"since":khive_runtime::micros_to_iso(SINCE+200),"until":khive_runtime::micros_to_iso(SINCE+210)}),
        ),
    ] {
        results.insert(
            name,
            registry
                .dispatch("brain.event_counts", window(extra))
                .await
                .unwrap(),
        );
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let filter = EventFilter {
        actors: vec![PRIMARY.into()],
        kinds: vec![EventKind::Audit],
        after: Some(SINCE + 299),
        before: Some(SINCE + 303),
        ..EventFilter::default()
    };
    let (items, total, truncated) = fetch_event_counts_window(store.as_ref(), &filter, false, 2)
        .await
        .unwrap();
    assert_eq!((items.len(), total, truncated), (2, 3, true));
    let mut counts = EventCountsAccumulator::new(None, Some(EventCountGroupBy::VerbActor));
    for event in &items {
        counts.observe(event);
    }
    let mut prefix =
        json!({"total_page_scoped":items.len(),"window_event_total":total,"truncated":truncated});
    counts.add_to_result(&mut prefix, truncated);
    results.insert("prefix", prefix);
    for event in &rows {
        let stored = runtime
            .backend()
            .events_for_namespace(&event.namespace)
            .unwrap()
            .get_event(event.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(stored).unwrap(),
            serde_json::to_value(event).unwrap(),
            "reader preserves every stored field and id"
        );
    }
    json!({"fixture":rows,"results":results})
}

#[tokio::test]
async fn event_counts_sampling_capture_compatibility_fixture() {
    let capture = capture_fixture().await;
    let own = &capture["results"]["own"];
    assert_eq!(capture["fixture"].as_array().unwrap().len(), 128);
    assert_eq!(own["total"], 122);
    assert_eq!(own["total_cost_unit"], 124);
    assert_eq!(
        own["counts_by_error_class"],
        json!({"storage":1,"gate_refusal":1})
    );
    assert_eq!(own["unclassified_error_count"], 0);
    println!("EVENT_COUNTS_COMPAT_CAPTURE={capture}");
}

fn expected(rows: u64, sample: u64, forced: u64, unsampled: u64, every: Option<u64>) -> Value {
    json!({"rows":rows,"sample_rows":sample,"forced_rows":forced,"unsampled_rows":unsampled,"every":every})
}

#[tokio::test]
async fn event_counts_sampling_metadata_respects_authorized_filters_and_fetched_prefix() {
    let capture = capture_fixture().await;
    let result = &capture["results"];
    let own = json!({"comm.heartbeat":expected(14,8,2,100,Some(10)),"comm.cleanup_expired_quarantine":expected(2,1,1,1,Some(10))});
    for case in ["own", "exhaustive", "audit"] {
        assert_eq!(result[case]["sampled_success_rows"], own, "{case}");
    }
    let heartbeat = &own["comm.heartbeat"];
    assert_eq!(
        heartbeat["unsampled_rows"].as_u64().unwrap()
            + heartbeat["every"].as_u64().unwrap() * heartbeat["sample_rows"].as_u64().unwrap(),
        180
    );
    assert_eq!(
        result["all"]["sampled_success_rows"],
        json!({"comm.heartbeat":expected(15,9,2,100,None),"comm.cleanup_expired_quarantine":expected(3,1,2,1,None)})
    );
    assert_eq!(
        result["secondary"]["sampled_success_rows"],
        json!({"comm.heartbeat":expected(1,1,0,0,Some(20)),"comm.cleanup_expired_quarantine":expected(1,0,1,0,Some(20))})
    );
    assert_eq!(
        result["research"]["sampled_success_rows"],
        json!({"comm.heartbeat":expected(1,1,0,0,Some(10))})
    );
    assert_eq!(
        result["research_all"]["sampled_success_rows"],
        json!({"comm.heartbeat":expected(1,1,0,0,Some(10)),"comm.cleanup_expired_quarantine":expected(1,1,0,0,Some(20))})
    );
    assert_eq!(result["search"]["sampled_success_rows"], json!({}));
    assert_eq!(
        result["narrow"]["sampled_success_rows"],
        json!({"comm.heartbeat":expected(8,8,0,0,Some(10))})
    );
    assert_eq!(
        result["prefix"]["sampled_success_rows_page_scoped"],
        json!({"comm.cleanup_expired_quarantine":expected(2,1,1,0,Some(10))})
    );
    assert!(result["prefix"].get("sampled_success_rows").is_none());
    assert!(result["own"]
        .get("sampled_success_rows_page_scoped")
        .is_none());
}

fn aggregate(events: &[Event]) -> Value {
    let mut counts = EventCountsAccumulator::new(None, None);
    for event in events {
        counts.observe(event);
    }
    let mut result = json!({});
    counts.add_to_result(&mut result, false);
    result
}

#[test]
fn event_counts_sampling_validates_objects_and_rate_windows() {
    for malformed in [
        Value::Null,
        json!([]),
        json!(false),
        json!(1),
        json!({}),
        json!({"seq":0,"selected_by":"sample"}),
        json!({"every":10,"selected_by":"sample"}),
        json!({"every":10,"seq":0}),
        sampling(0, 0, "sample"),
        json!({"every":10.0,"seq":0,"selected_by":"sample"}),
        json!({"every":"10","seq":0,"selected_by":"sample"}),
        json!({"every":10,"seq":-1,"selected_by":"sample"}),
        json!({"every":10,"seq":0.0,"selected_by":"sample"}),
        sampling(10, 0, "other"),
    ] {
        let event = row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            Some(malformed.clone()),
            SINCE,
        );
        assert_eq!(
            aggregate(&[event])["sampled_success_rows"]["comm.heartbeat"],
            expected(1, 0, 0, 0, None),
            "{malformed}"
        );
    }
    let event = |object| {
        row(
            "comm.heartbeat",
            EventKind::Audit,
            EventOutcome::Success,
            object,
            SINCE,
        )
    };
    assert_eq!(
        aggregate(&[event(None)])["sampled_success_rows"]["comm.heartbeat"],
        expected(0, 0, 0, 1, None)
    );
    assert_eq!(
        aggregate(&[
            event(Some(sampling(10, 0, "sample"))),
            event(Some(sampling(20, 1, "forced"))),
            event(Some(sampling(10, 2, "sample")))
        ])["sampled_success_rows"]["comm.heartbeat"],
        expected(3, 2, 1, 0, None)
    );
    assert_eq!(
        aggregate(&[
            event(Some(sampling(10, 0, "sample"))),
            event(Some(json!({"every":20,"selected_by":"sample"})))
        ])["sampled_success_rows"]["comm.heartbeat"],
        expected(2, 1, 0, 0, Some(10))
    );
    let mut extra = sampling(20, 0, "sample");
    extra["future"] = json!(true);
    assert_eq!(
        aggregate(&[event(Some(extra))])["sampled_success_rows"]["comm.heartbeat"],
        expected(1, 1, 0, 0, Some(20))
    );
}

#[test]
fn event_counts_sampling_population_excludes_counterfeit_sampling() {
    for (verb, kind, outcome) in [
        ("comm.heartbeat", EventKind::Audit, EventOutcome::Error),
        (
            "comm.cleanup_expired_quarantine",
            EventKind::Audit,
            EventOutcome::Denied,
        ),
        (
            "comm.heartbeat",
            EventKind::SearchExecuted,
            EventOutcome::Success,
        ),
        ("stats", EventKind::Audit, EventOutcome::Success),
    ] {
        let result = aggregate(&[row(
            verb,
            kind,
            outcome,
            Some(sampling(10, 0, "sample")),
            SINCE,
        )]);
        assert_eq!(
            result["sampled_success_rows"],
            json!({}),
            "{verb} {kind:?} {outcome:?}"
        );
        assert_eq!(result["counts_by_verb"][verb], 1);
        assert_eq!(result["total_cost_unit"], 1);
    }
}
