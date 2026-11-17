use super::grouping::EventCountsAccumulator;
use khive_storage::Event;
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};

const PRIMARY: &str = "actor:reader:primary";
const SINCE: i64 = 1_000_000;

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

fn expected(rows: u64, sample: u64, forced: u64, unsampled: u64, every: Option<u64>) -> Value {
    json!({"rows":rows,"sample_rows":sample,"forced_rows":forced,"unsampled_rows":unsampled,"every":every})
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
