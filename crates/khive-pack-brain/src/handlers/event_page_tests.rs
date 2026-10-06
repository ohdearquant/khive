use super::*;

use khive_runtime::{KhiveConfig, KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::event::Event;
use khive_types::SubstrateKind;
use uuid::Uuid;

fn fixture(
    actor: Option<&str>,
    visible: &[&str],
    fleet_readers: &[&str],
) -> (BrainPack, KhiveRuntime, NamespaceToken) {
    let mut config = KhiveConfig::default();
    config.actor.id = actor.map(str::to_owned);
    config.actor.visible_namespaces = Some(visible.iter().map(|s| s.to_string()).collect());
    config.brain.fleet_readers = fleet_readers.iter().map(|s| s.to_string()).collect();
    let runtime = KhiveRuntime::new(khive_runtime::runtime_config_from_khive_config(
        &config,
        RuntimeConfig {
            db_path: None,
            actor_id: None,
            packs: vec!["kg".into()],
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        },
    ))
    .unwrap();
    let token = runtime
        .authorize_with_visibility(Namespace::local(), runtime.visible_namespaces().to_vec())
        .unwrap();
    (BrainPack::new(runtime.clone()), runtime, token)
}

async fn seed(runtime: &KhiveRuntime, namespace: &str, actor: &str, at: i64) -> Uuid {
    let mut event = Event::new(
        namespace,
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        actor,
    );
    event.created_at = at;
    event.payload = json!({"result_kind": "note", "marker": at});
    let id = event.id;
    runtime
        .backend()
        .events_for_namespace(namespace)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();
    id
}

fn params() -> Value {
    json!({
        "since": micros_to_iso(1_000_000),
        "until": micros_to_iso(2_000_000),
        "kind": "search_executed",
        "limit": 1,
    })
}

fn ids(value: &Value) -> Vec<Uuid> {
    value["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().parse().unwrap())
        .collect()
}

fn invalid_message(error: RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(message) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[tokio::test]
async fn page_stitches_stored_rows_and_matches_quiescent_alias_counts() {
    let (pack, runtime, token) = fixture(Some("caller"), &[], &[]);
    let first = seed(&runtime, "local", "caller", 1_000_001).await;
    let second = seed(&runtime, "local", "actor:caller", 1_000_002).await;
    seed(&runtime, "local", "other", 1_000_003).await;
    let request = params();
    let page = pack
        .handle_event_page(&token, request.clone())
        .await
        .unwrap();
    assert_eq!(ids(&page), vec![first]);
    assert_eq!(page["count"], 1);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["events"][0]["payload"]["marker"], 1_000_001);
    assert_eq!(page["events"][0]["created_at"], micros_to_iso(1_000_001));
    assert_eq!(page["scope"]["namespaces"], json!(["local"]));
    assert_eq!(page["consistency"], "live_ordered_window");

    let mut next = request.clone();
    next["after"] = page["next_after"].clone();
    next["limit"] = json!(1000);
    let tail = pack.handle_event_page(&token, next).await.unwrap();
    assert_eq!(ids(&tail), vec![second]);
    assert_eq!(tail["has_more"], false);
    assert!(tail["next_after"].is_null());
    assert_eq!(tail["until"], page["until"]);

    let counts = pack
        .handle_event_counts(&token, request_without_page_args())
        .await
        .unwrap();
    assert_eq!(counts["truncated"], false);
    assert_eq!(counts["window_event_total"], 2);
    assert_eq!(counts["counts_by_actor"], json!({"caller": 2}));
    assert_eq!(
        page["count"].as_u64().unwrap() + tail["count"].as_u64().unwrap(),
        2
    );
}

fn request_without_page_args() -> Value {
    let mut request = params();
    request.as_object_mut().unwrap().remove("limit");
    request
}

#[tokio::test]
async fn namespace_exclusions_are_applied_before_page_peek() {
    let (pack, runtime, token) = fixture(Some("caller"), &["visible"], &[]);
    for at in 1_000_000..1_000_010 {
        seed(&runtime, "local", "caller", at).await;
    }
    let first = seed(&runtime, "visible", "caller", 1_000_011).await;
    let second = seed(&runtime, "visible", "caller", 1_000_012).await;
    seed(&runtime, "hidden", "caller", 1_000_013).await;
    let mut request = params();
    request["namespaces"] = json!(["local", "visible", "hidden"]);
    request["exclude_namespaces"] = json!(["local"]);
    let page = pack
        .handle_event_page(&token, request.clone())
        .await
        .unwrap();
    assert_eq!(ids(&page), vec![first]);
    assert_eq!(page["scope"]["namespaces"], json!(["visible"]));
    assert_eq!(page["has_more"], true);
    request["after"] = page["next_after"].clone();
    let tail = pack.handle_event_page(&token, request).await.unwrap();
    assert_eq!(ids(&tail), vec![second]);
    assert_eq!(tail["has_more"], false);
}

#[tokio::test]
async fn invisible_and_empty_namespace_selections_return_same_empty_page() {
    let (pack, runtime, token) = fixture(Some("caller"), &[], &[]);
    seed(&runtime, "hidden", "caller", 1_000_001).await;
    for selection in [json!([]), json!(["hidden"]), json!(["absent"])] {
        let mut request = params();
        request["namespaces"] = selection;
        let page = pack.handle_event_page(&token, request).await.unwrap();
        assert_eq!(page["count"], 0);
        assert_eq!(page["has_more"], false);
        assert_eq!(page["scope"]["namespaces"], json!([]));
        assert!(page["next_after"].is_null());
    }
}

#[tokio::test]
async fn serving_fleet_allowlist_controls_all_actors_without_widening_namespaces() {
    let (unlisted, _, _) = fixture(Some("serving"), &[], &[]);
    let (_, _, caller) = fixture(Some("caller"), &["visible"], &["caller"]);
    let mut request = params();
    request["all_actors"] = json!(true);
    let error = unlisted
        .handle_event_page(&caller, request.clone())
        .await
        .unwrap_err();
    assert_eq!(
        invalid_message(error),
        "actor \"caller\" is not a configured fleet reader"
    );

    let (listed, runtime, _) = fixture(Some("serving"), &[], &["caller"]);
    let own = seed(&runtime, "local", "foreign", 1_000_001).await;
    seed(&runtime, "visible", "foreign", 1_000_002).await;
    let page = listed.handle_event_page(&caller, request).await.unwrap();
    assert_eq!(ids(&page), vec![own]);
    assert_eq!(page["scope"]["namespaces"], json!(["local"]));
    assert_eq!(page["has_more"], false);
}

#[tokio::test]
async fn actor_refusals_precede_missing_time_or_invalid_kind() {
    let (pack, _, token) = fixture(Some("caller"), &[], &[]);
    let cases = [
        (
            json!({"actor": "foreign", "kind": "invalid"}),
            "actor \"foreign\" is not visible to this caller",
        ),
        (
            json!({"actor": "caller", "all_actors": true}),
            "all_actors=true cannot be combined with actor",
        ),
    ];
    for (request, expected) in cases {
        let error = pack
            .handle_event_page(&token, request.clone())
            .await
            .unwrap_err();
        assert_eq!(invalid_message(error), expected);
        let old_error = pack.handle_event_counts(&token, request).await.unwrap_err();
        assert_eq!(invalid_message(old_error), expected);
    }
    let missing = pack.handle_event_page(&token, json!({})).await.unwrap_err();
    assert!(invalid_message(missing).starts_with("missing `since`:"));
}

#[tokio::test]
async fn prefixed_and_anonymous_callers_keep_count_actor_aliases() {
    for actor in [Some("actor:caller"), None] {
        let (pack, runtime, token) = fixture(actor, &[], &[]);
        let scope = event_actor_read_scope(&runtime, &token, None, false).unwrap();
        assert_eq!(scope.actors.len(), 1);
        let expected = seed(&runtime, "local", &scope.actors[0], 1_000_001).await;
        seed(&runtime, "local", "caller", 1_000_002).await;
        let page = pack.handle_event_page(&token, params()).await.unwrap();
        assert_eq!(ids(&page), vec![expected]);
        let counts = pack
            .handle_event_counts(&token, request_without_page_args())
            .await
            .unwrap();
        assert_eq!(counts["window_event_total"], 1);
    }
}

#[tokio::test]
async fn page_limits_and_cursor_scope_mismatches_refuse() {
    let (pack, runtime, token) = fixture(Some("caller"), &[], &[]);
    seed(&runtime, "local", "caller", 1_000_001).await;
    seed(&runtime, "local", "caller", 1_000_002).await;
    for limit in [0, 1001] {
        let mut request = params();
        request["limit"] = json!(limit);
        assert!(pack.handle_event_page(&token, request).await.is_err());
    }
    let page = pack.handle_event_page(&token, params()).await.unwrap();
    let mut request = params();
    request["after"] = page["next_after"].clone();
    request["actor"] = json!("actor:caller");
    assert!(pack.handle_event_page(&token, request).await.is_err());
    let mut request = params();
    request["after"] = json!("invalid-cursor-sentinel");
    let error = pack.handle_event_page(&token, request).await.unwrap_err();
    assert!(!error.to_string().contains("invalid-cursor-sentinel"));
}

#[tokio::test]
async fn duplicate_kind_selectors_do_not_duplicate_events() {
    let (pack, runtime, token) = fixture(Some("caller"), &[], &[]);
    let expected = seed(&runtime, "local", "caller", 1_000_001).await;
    let mut request = params();
    request["kinds"] = json!(["search_executed", "search_executed"]);
    let page = pack.handle_event_page(&token, request).await.unwrap();
    assert_eq!(ids(&page), vec![expected]);
    assert_eq!(page["has_more"], false);
}

#[test]
fn response_preserves_every_stored_payload_row_and_microsecond_time() {
    let mut event = Event::new(
        "local",
        "gtd.transition",
        EventKind::Audit,
        SubstrateKind::Note,
        "caller",
    );
    event.created_at = 1_000_123;
    event.payload = json!({"event_id": "payload-only-id", "marker": "stored"});
    let id = event.id;
    let result = EventReadPageResult {
        events: vec![event.clone(), event],
        has_more: false,
        next_after: None,
        since_us: 1_000_000,
        until_us: 2_000_000,
        namespaces: vec!["local".into()],
    };
    let page = event_page_json(result).unwrap();
    assert_eq!(page["count"], 2);
    for row in page["events"].as_array().unwrap() {
        assert_eq!(row["id"], id.to_string());
        assert_eq!(row["payload"]["event_id"], "payload-only-id");
        assert_eq!(row["created_at"], micros_to_iso(1_000_123));
        assert!(row["payload"].get("task_id").is_none());
        assert!(row["payload"].get("from").is_none());
        assert!(row["payload"].get("to").is_none());
    }
}

#[test]
fn response_budget_counts_actual_json_escaping_and_envelope_bytes() {
    let result = |payload: Value| {
        let mut event = Event::new(
            "local",
            "search",
            EventKind::SearchExecuted,
            SubstrateKind::Note,
            "caller",
        );
        event.created_at = 1_000_001;
        event.payload = payload;
        EventReadPageResult {
            events: vec![event],
            has_more: false,
            next_after: None,
            since_us: 1_000_000,
            until_us: 2_000_000,
            namespaces: vec!["local".into()],
        }
    };
    let small = event_page_json(result(json!({"marker": "small"}))).unwrap();
    assert!(serde_json::to_vec(&small).unwrap().len() < MAX_RESPONSE_BYTES);
    let payload = "\u{1}".repeat(MAX_RESPONSE_BYTES / 2);
    assert!(payload.len() < MAX_RESPONSE_BYTES);
    let error = event_page_json(result(json!({"marker": payload}))).unwrap_err();
    assert_eq!(
        invalid_message(error),
        "event page response exceeds the 4194304-byte limit"
    );
}

#[test]
fn page_metadata_is_public_and_legacy_debug_metadata_is_unchanged() {
    assert_eq!(HANDLER.visibility, Visibility::Verb);
    assert_eq!(HANDLER.category, VerbCategory::Assertive);
    let names: Vec<_> = HANDLER.params.iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        [
            "since",
            "until",
            "kind",
            "kinds",
            "namespaces",
            "exclude_namespaces",
            "actor",
            "all_actors",
            "limit",
            "after"
        ]
    );
    let debug = super::super::BRAIN_HANDLERS
        .iter()
        .find(|h| h.name == "brain.events")
        .unwrap();
    assert_eq!(debug.visibility, Visibility::Subhandler);
    assert_eq!(debug.params.len(), 1);
    assert_eq!(debug.params[0].name, "limit");
}
