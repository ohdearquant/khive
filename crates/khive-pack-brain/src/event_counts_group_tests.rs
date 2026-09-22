use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};

use crate::event_counts_grouping::EventCountGroupBy;
use crate::handlers::{fetch_event_counts_window, fetch_event_counts_window_exhaustive};
use crate::BrainPack;

const CALLER: &str = "lambda:reader";
const SINCE: i64 = 1_000_000;
const UNTIL: i64 = 2_000_000;

fn fixture(fleet_reader: bool) -> (KhiveRuntime, VerbRegistry) {
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
    let runtime = KhiveRuntime::new(config).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(CALLER.into()));
    builder.with_visible_namespaces(runtime.config().visible_namespaces.clone());
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

fn window() -> Value {
    json!({"since": khive_runtime::micros_to_iso(SINCE), "until": khive_runtime::micros_to_iso(UNTIL)})
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

fn cross_sum(value: &Value) -> u64 {
    value
        .as_object()
        .unwrap()
        .values()
        .flat_map(|actors| actors.as_object().unwrap().values())
        .map(|count| count.as_u64().unwrap())
        .sum()
}

#[tokio::test]
async fn requested_cross_keeps_joint_identity_when_marginals_are_equal() {
    let (runtime, registry) = fixture(true);
    for (verb, actor, count) in [
        ("gtd.tasks", "actor:lambda:a", 3),
        ("gtd.tasks", "agent:worker:z", 1),
        ("memory.recall", "actor:lambda:a", 1),
        ("memory.recall", "agent:worker:z", 3),
    ] {
        for offset in 0..count {
            seed(
                &runtime,
                "local",
                verb,
                actor,
                EventKind::Audit,
                SINCE + offset,
            )
            .await;
        }
    }
    seed(
        &runtime,
        "local",
        "outside.before",
        "actor:lambda:a",
        EventKind::Audit,
        SINCE - 1,
    )
    .await;
    seed(
        &runtime,
        "local",
        "outside.until",
        "actor:lambda:a",
        EventKind::Audit,
        UNTIL,
    )
    .await;
    seed(
        &runtime,
        "local",
        "other.kind",
        "actor:lambda:a",
        EventKind::SearchExecuted,
        SINCE,
    )
    .await;
    seed(
        &runtime,
        "elsewhere",
        "other.namespace",
        "actor:lambda:a",
        EventKind::Audit,
        SINCE,
    )
    .await;
    let mut args = window();
    args["all_actors"] = json!(true);
    args["kind"] = json!("audit");
    let ungrouped = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert!(ungrouped.get("counts_by_verb_and_actor").is_none());
    assert!(ungrouped
        .get("counts_by_verb_and_actor_page_scoped")
        .is_none());
    args["group_by"] = json!(["verb", "actor"]);
    let mut grouped = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(
        grouped["counts_by_verb"],
        json!({"gtd.tasks": 4, "memory.recall": 4})
    );
    assert_eq!(
        grouped["counts_by_actor"],
        json!({"actor:lambda:a": 4, "agent:worker:z": 4})
    );
    assert_eq!(
        grouped["counts_by_verb_and_actor"],
        json!({
            "gtd.tasks": {"actor:lambda:a": 3, "agent:worker:z": 1},
            "memory.recall": {"actor:lambda:a": 1, "agent:worker:z": 3},
        })
    );
    assert_eq!(cross_sum(&grouped["counts_by_verb_and_actor"]), 8);
    assert_eq!(grouped["window_event_total"], 8);
    grouped
        .as_object_mut()
        .unwrap()
        .remove("counts_by_verb_and_actor");
    assert_eq!(
        grouped, ungrouped,
        "grouping must not change any existing response field"
    );
}

#[tokio::test]
async fn cross_inherits_default_alias_coalescing_and_explicit_actor_scope() {
    let (runtime, registry) = fixture(false);
    for actor in [
        CALLER,
        "actor:lambda:reader",
        "actor:lambda:visible",
        "actor:lambda:hidden",
    ] {
        seed(
            &runtime,
            "local",
            "pack.verb",
            actor,
            EventKind::Audit,
            SINCE,
        )
        .await;
    }
    let mut args = window();
    args["group_by"] = json!(["verb", "actor"]);
    let own = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(
        own["counts_by_verb_and_actor"],
        json!({"pack.verb": {CALLER: 2}})
    );
    assert_eq!(own["counts_by_actor"], json!({CALLER: 2}));
    args["actor"] = json!(CALLER);
    let explicit = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(
        explicit["counts_by_verb_and_actor"],
        json!({"pack.verb": {CALLER: 1, "actor:lambda:reader": 1}})
    );
    args["actor"] = json!("lambda:visible");
    let visible = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(
        visible["counts_by_verb_and_actor"],
        json!({"pack.verb": {"actor:lambda:visible": 1}})
    );
    args["actor"] = json!("lambda:hidden");
    let hidden = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap_err();
    assert!(matches!(hidden, RuntimeError::InvalidInput(_)));
    args.as_object_mut().unwrap().remove("actor");
    args["all_actors"] = json!(true);
    let all = registry
        .dispatch("brain.event_counts", args)
        .await
        .unwrap_err();
    assert!(all.to_string().contains("not a configured fleet reader"));
}

#[tokio::test]
async fn group_by_is_optional_and_accepts_only_the_ordered_pair() {
    let (_, registry) = fixture(true);
    let base = registry
        .dispatch("brain.event_counts", window())
        .await
        .unwrap();
    let mut args = window();
    args["group_by"] = Value::Null;
    assert_eq!(
        registry
            .dispatch("brain.event_counts", args.clone())
            .await
            .unwrap(),
        base
    );
    args["group_by"] = json!(["verb", "actor"]);
    let empty = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(empty["counts_by_verb_and_actor"], json!({}));
    for invalid in [
        json!([]),
        json!(["verb"]),
        json!(["verb", "actor", "kind"]),
        json!(["actor", "verb"]),
        json!(["verb", "verb"]),
        json!(["actor", "actor"]),
        json!(["verb", "kind"]),
        json!(["actor", "kind"]),
        json!(["Verb", "actor"]),
        json!("verb,actor"),
        json!({"verb": "actor"}),
        json!(["verb", null]),
        json!([1, "actor"]),
    ] {
        args["group_by"] = invalid.clone();
        let error = registry
            .dispatch("brain.event_counts", args.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{invalid}: {error:?}"
        );
    }
    args["group_by"] = json!(["verb", "actor"]);
    args["actor"] = json!(CALLER);
    args["all_actors"] = json!(true);
    assert!(registry
        .dispatch("brain.event_counts", args)
        .await
        .unwrap_err()
        .to_string()
        .contains("cannot be combined"));
}

#[tokio::test]
async fn cross_uses_the_sampled_and_exhaustive_event_windows_including_audit_segregation() {
    let (runtime, _) = fixture(true);
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
async fn more_cross_cells_than_marginal_keys_do_not_create_another_cap() {
    let (runtime, registry) = fixture(true);
    for verb in ["v.one", "v.two"] {
        for actor in ["actor:a", "actor:b"] {
            seed(&runtime, "local", verb, actor, EventKind::Audit, SINCE).await;
        }
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let (items, total, truncated) = fetch_event_counts_window(
        store.as_ref(),
        &EventFilter {
            kinds: vec![EventKind::Audit],
            ..Default::default()
        },
        false,
        4,
    )
    .await
    .unwrap();
    assert_eq!(total, 4);
    assert_eq!(items.len(), 4);
    assert!(!truncated);
    let mut args = window();
    args["all_actors"] = json!(true);
    args["kind"] = json!("audit");
    args["group_by"] = json!(["verb", "actor"]);
    let result = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(result["counts_by_verb"].as_object().unwrap().len(), 2);
    assert_eq!(result["counts_by_actor"].as_object().unwrap().len(), 2);
    assert_eq!(result["truncated"], false);
    assert_eq!(
        result["counts_by_verb_and_actor"],
        json!({"v.one": {"actor:a": 1, "actor:b": 1}, "v.two": {"actor:a": 1, "actor:b": 1}})
    );
}
