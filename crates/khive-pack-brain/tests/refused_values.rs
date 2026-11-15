//! Refused enum values retain useful diagnostics without echoing credentials.
use std::collections::HashMap;

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Event, EventFilter, PageRequest};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};

fn fixture() -> VerbRegistry {
    fixture_with_runtime().1
}

fn fixture_with_runtime() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "brain".into()],
        actor_id: Some("brain-refusal-fixture".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("brain-refusal-fixture".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn refused_values() -> [(String, &'static str); 2] {
    [
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

async fn assert_refusal(
    registry: &VerbRegistry,
    verb: &str,
    params: Value,
    raw: &str,
    expected: String,
) {
    let error = registry
        .dispatch(verb, params)
        .await
        .expect_err("invalid enum");
    let RuntimeError::InvalidInput(ref message) = error else {
        panic!("{verb} changed its error kind: {error:?}");
    };
    assert_eq!(message, &expected, "{verb}");
    let displayed = error.to_string();
    let projected = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(projected["kind"], "runtime_error");
    assert_eq!(projected["message"], displayed);
    if expected.contains("***MASKED***") {
        assert!(!projected.to_string().contains(raw), "credential leaked");
        assert!(
            !projected.to_string().contains(&raw.to_ascii_lowercase()),
            "normalized credential leaked"
        );
        assert!(displayed.contains("***MASKED***"));
    }
}

#[tokio::test]
async fn profile_lifecycle_refusals_mask_credentials_and_preserve_filters() {
    let registry = fixture();
    let before = registry
        .dispatch("brain.profiles", json!({}))
        .await
        .unwrap();
    for (raw, shown) in refused_values() {
        assert_refusal(
            &registry,
            "brain.profiles",
            json!({"lifecycle": raw}),
            &raw,
            format!(
                "invalid lifecycle {shown:?}; expected one of 'active', 'inactive', 'archived'"
            ),
        )
        .await;
    }
    for lifecycle in ["active", "inactive", "archived"] {
        let result = registry
            .dispatch("brain.profiles", json!({"lifecycle": lifecycle}))
            .await
            .expect("valid lifecycle");
        assert!(result["profiles"].is_array());
    }
    assert_eq!(
        registry
            .dispatch("brain.profiles", json!({}))
            .await
            .unwrap(),
        before
    );
}

async fn feedback_refusals(verb: &str) {
    let registry = fixture();
    let target = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "refusal target"}),
        )
        .await
        .expect("target")["id"]
        .clone();
    registry
        .dispatch("brain.create_profile", json!({"name": "refusal-profile"}))
        .await
        .expect("profile");
    let params = |signal: &str| {
        let mut args = json!({
            "target_id": target,
            "signal": signal,
            "served_by_profile_id": "refusal-profile",
        });
        if verb == "brain.auto_feedback" {
            args["query"] = json!("retrieval quality");
            args["results"] = json!([{"id": target}]);
        }
        args
    };
    for (raw, shown) in refused_values() {
        assert_refusal(
            &registry,
            verb,
            params(&raw),
            &raw,
            format!(
                "unknown signal {shown:?}; valid: useful | not_useful | wrong | \
                 explicit_positive | explicit_negative | implicit_positive | \
                 implicit_negative | correction"
            ),
        )
        .await;
    }
    let accepted = registry
        .dispatch(verb, params("useful"))
        .await
        .expect("valid feedback");
    assert_eq!(accepted["emitted"], true);
    assert_eq!(accepted["signal"], "useful");
}

#[tokio::test]
async fn feedback_signal_refusals_mask_credentials_and_preserve_valid_feedback() {
    feedback_refusals("brain.feedback").await;
}

#[tokio::test]
async fn auto_feedback_signal_refusals_share_masked_diagnostics() {
    feedback_refusals("brain.auto_feedback").await;
}

const EVENT_ACTOR: &str = "brain-refusal-fixture";
const EVENT_START: i64 = 1_000_000;

fn event_window() -> Value {
    json!({
        "since": khive_runtime::micros_to_iso(EVENT_START),
        "until": khive_runtime::micros_to_iso(EVENT_START + 100),
    })
}

fn event_credentials() -> [String; 2] {
    let values = [
        format!("sk-proj-{}", "A".repeat(80)),
        "AKIAFAKEKEY1234567890".into(),
    ];
    for raw in &values {
        assert_eq!(
            mask_for_redaction_surface(RedactionSurface::GateProbe, raw),
            "***MASKED***",
            "synthetic credential must exercise the existing masker"
        );
    }
    values
}

async fn event_fixture() -> (KhiveRuntime, VerbRegistry, Vec<Event>) {
    let (runtime, registry) = fixture_with_runtime();
    registry
        .dispatch("brain.profiles", json!({}))
        .await
        .expect("load Brain before recording event state");
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let mut seeded = Vec::new();
    for (offset, kind) in [EventKind::PhaseStarted, EventKind::PhaseCompleted]
        .into_iter()
        .enumerate()
    {
        let mut event = Event::new(
            "local",
            "fixture.work",
            kind,
            SubstrateKind::Note,
            EVENT_ACTOR,
        );
        event.created_at = EVENT_START + offset as i64;
        store.append_event(event.clone()).await.unwrap();
        seeded.push(event);
    }
    for event in &seeded {
        let found = store.get_event(event.id).await.unwrap().unwrap();
        assert_eq!(found.created_at, event.created_at);
        assert_eq!(found.actor, event.actor);
    }
    (runtime, registry, seeded)
}

async fn stored_events(runtime: &KhiveRuntime) -> Value {
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.count_events(EventFilter::default()).await.unwrap(),
        page.items.len() as u64,
        "complete event snapshot"
    );
    let mut events = page.items;
    events.sort_by_key(|event| event.id);
    serde_json::to_value(events).unwrap()
}

#[tokio::test]
async fn event_kind_refusals_mask_original_credentials_and_preserve_filters() {
    let (runtime, registry, seeded) = event_fixture().await;
    let before = stored_events(&runtime).await;
    let mut cases = vec![("bogus".to_string(), "bogus"), (" BoGuS ".into(), "bogus")];
    cases.extend(event_credentials().map(|raw| (raw, "***MASKED***")));
    for (raw, shown) in cases {
        let expected = format!(
            "invalid `kind`: unknown event_kind: {shown:?}. Valid: {}",
            "bogus".parse::<EventKind>().unwrap_err().valid.join(" | ")
        );
        for (verb, field) in [
            ("brain.event_counts", "kind"),
            ("brain.event_page", "kind"),
            ("brain.event_page", "kinds"),
        ] {
            let mut params = event_window();
            params[field] = if field == "kinds" {
                json!(["phase_completed", raw])
            } else {
                json!(raw)
            };
            assert_refusal(&registry, verb, params, &raw, expected.clone()).await;
        }
    }
    let all = registry
        .dispatch("brain.event_counts", event_window())
        .await
        .unwrap();
    assert_eq!(
        all["counts_by_kind"],
        json!({"phase_started": 1, "phase_completed": 1})
    );
    let mut filtered = event_window();
    filtered["kind"] = json!(" PHASE_STARTED ");
    let counts = registry
        .dispatch("brain.event_counts", filtered.clone())
        .await
        .unwrap();
    assert_eq!(counts["counts_by_kind"], json!({"phase_started": 1}));
    for params in [filtered, {
        let mut params = event_window();
        params["kinds"] = json!(["phase_started", " PHASE_STARTED "]);
        params
    }] {
        let page = registry.dispatch("brain.event_page", params).await.unwrap();
        assert_eq!(page["count"], 1);
        assert_eq!(page["events"][0]["id"], json!(seeded[0].id));
        assert_eq!(page["events"][0]["kind"], "phase_started");
    }
    assert_eq!(stored_events(&runtime).await, before);
}

#[tokio::test]
async fn event_count_group_by_refusals_remain_masked_and_valid_grouping_works() {
    let (runtime, registry, _) = event_fixture().await;
    let before = stored_events(&runtime).await;
    let mut cases = vec![("bogus".to_string(), "bogus")];
    cases.extend(event_credentials().map(|raw| (raw, "***MASKED***")));
    for (raw, shown) in cases {
        for position in 0..2 {
            let mut params = event_window();
            let mut group_by = json!(["verb", "actor"]);
            group_by[position] = json!(raw);
            params["group_by"] = group_by;
            assert_refusal(
                &registry,
                "brain.event_counts",
                params,
                &raw,
                format!("unknown variant `{shown}`, expected `verb` or `actor`"),
            )
            .await;
        }
    }
    let base = registry
        .dispatch("brain.event_counts", event_window())
        .await
        .unwrap();
    assert_eq!(
        base["counts_by_kind"],
        json!({"phase_started": 1, "phase_completed": 1})
    );
    assert!(base.get("counts_by_verb_and_actor").is_none());
    let mut params = event_window();
    params["group_by"] = Value::Null;
    assert_eq!(
        registry
            .dispatch("brain.event_counts", params.clone())
            .await
            .unwrap(),
        base
    );
    params["group_by"] = json!(["verb", "actor"]);
    let grouped = registry
        .dispatch("brain.event_counts", params)
        .await
        .unwrap();
    assert_eq!(
        grouped["counts_by_verb_and_actor"],
        json!({"fixture.work": {"brain-refusal-fixture": 2}})
    );
    assert_eq!(grouped["counts_by_kind"], base["counts_by_kind"]);
    assert_eq!(stored_events(&runtime).await, before);
}
