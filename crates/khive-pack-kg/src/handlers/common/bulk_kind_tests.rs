use super::*;
use async_trait::async_trait;
use khive_runtime::{PackRuntime, VerbRegistryBuilder};
use khive_types::{EntityTypeDef, HandlerDef, Pack};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

struct BulkTypesPack;

impl Pack for BulkTypesPack {
    const NAME: &'static str = "bulk_types_fixture";
    const NOTE_KINDS: &'static [&'static str] = &["bulk_note", "bulk_report_note"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &[
        EntityTypeDef {
            kind: EntityKind::Document,
            type_name: "bulk_report",
            aliases: &["bulk_rep", "bulk_shared"],
        },
        EntityTypeDef {
            kind: EntityKind::Concept,
            type_name: "bulk_concept",
            aliases: &["bulk_shared"],
        },
        EntityTypeDef {
            kind: EntityKind::Document,
            type_name: "bulk_same_type",
            aliases: &["bulk_same_alias"],
        },
        EntityTypeDef {
            kind: EntityKind::Concept,
            type_name: "bulk_same_type",
            aliases: &["bulk_same_alias"],
        },
    ];
}

#[async_trait]
impl PackRuntime for BulkTypesPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn entity_types(&self) -> &'static [EntityTypeDef] {
        Self::ENTITY_TYPES
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "fixture does not handle {verb:?}"
        )))
    }
}

fn registry(extras: bool) -> VerbRegistry {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(runtime));
    if extras {
        builder.register(BulkTypesPack);
    }
    builder.build().unwrap()
}

#[tokio::test]
async fn bulk_fixture_registry_builds_before_request_observation() {
    let loaded = registry(true);
    assert!(loaded.all_note_kinds().contains(&"bulk_note"));
    assert!(loaded.all_note_kinds().contains(&"bulk_report_note"));
    assert!(!loaded.all_note_kinds().contains(&"bulk_report"));
    assert!(loaded
        .all_entity_types()
        .iter()
        .any(|definition| definition.type_name == "bulk_report"));
}

#[tokio::test]
async fn bulk_handler_future_is_send_for_both_commit_modes() {
    fn require_send<T: Future + Send>(future: T) -> T {
        future
    }
    let runtime = KhiveRuntime::memory().unwrap();
    let pack = crate::KgPack::new(runtime.clone());
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(runtime));
    builder.register(BulkTypesPack);
    let registry = builder.build().unwrap();
    for atomic in [true, false] {
        let response = require_send(pack.handle_create(
            &token,
            json!({"atomic":atomic,"items":[{"kind":"bulk_report","entity_type":"bulk_rep","name":"send witness"}]}),
            &registry,
        )).await.unwrap();
        assert_eq!(response["created"], 1);
    }
}

#[tokio::test]
async fn bulk_registered_note_keeps_subtype_probe_before_note_fallback() {
    for atomic in [true, false] {
        let registry = registry(true);
        let (result, sites) = observed(registry.dispatch(
            "create",
            json!({"atomic":atomic,"items":[{"kind":"bulk_note","content":"valid distinct vocabulary"}]}),
        )).await;
        let response = result.unwrap();
        assert_eq!(response["created"], 1);
        assert_eq!(response["results"][0]["result"]["kind"], "bulk_note");
        assert_builds(&sites, &["subtype_kind_351"]);
    }
}

async fn observed<T>(future: impl Future<Output = T>) -> (T, Vec<&'static str>) {
    let builds = Arc::new(Mutex::new(Vec::new()));
    let result = REGISTRY_BUILDS.scope(builds.clone(), future).await;
    let sites = builds.lock().unwrap().clone();
    (result, sites)
}

fn assert_builds(sites: &[&'static str], expected: &[&'static str]) {
    assert_eq!(
        sites, expected,
        "actual composition provenance and request total"
    );
    assert_eq!(sites.len(), expected.len(), "request composition total");
}

#[tokio::test]
async fn bulk_subtypes_explicit_types_build_once_at_subtype_site() {
    for atomic in [true, false] {
        let registry = registry(true);
        let items: Vec<_> = (0..1000).map(|i| json!({"kind":"bulk_report", "entity_type":"bulk_rep", "name":format!("item {i}")})).collect();
        let (result, sites) =
            observed(registry.dispatch("create", json!({"items":items,"atomic":atomic}))).await;
        let response = result.unwrap();
        assert_eq!(response["created"], 1000);
        assert_eq!(response["failed"], 0);
        assert_builds(&sites, &["subtype_kind_351"]);
        for index in [0, 999] {
            assert_eq!(response["results"][index]["index"], index);
            assert_eq!(
                response["entity_type_normalized"][index]["stored"],
                "bulk_report"
            );
            let row = registry
                .dispatch(
                    "get",
                    json!({"id":response["results"][index]["result"]["id"]}),
                )
                .await
                .unwrap();
            assert_eq!(row["entity_type"], "bulk_report");
            assert_eq!(row["kind"], "document");
            assert_eq!(row["name"], format!("item {index}"));
        }
    }
}

#[tokio::test]
async fn bulk_base_kinds_explicit_aliases_build_once_at_pinned_site() {
    let registry = registry(true);
    let items: Vec<_> = (0..1000)
        .map(|i| json!({"kind":"document", "entity_type":"bulk_rep", "name":format!("base {i}")}))
        .collect();
    let (result, sites) = observed(registry.dispatch("create", json!({"items":items}))).await;
    assert_eq!(result.unwrap()["created"], 1000);
    assert_builds(&sites, &["pinned_type_104"]);
}

#[tokio::test]
async fn bulk_empty_and_base_without_type_do_not_compose() {
    let registry = registry(true);
    for params in [
        json!({"items":[]}),
        json!({"items":[{"kind":"document","name":"plain"},{"kind":"note","content":"plain note"}]}),
    ] {
        let (result, sites) = observed(registry.dispatch("create", params)).await;
        assert!(result.is_ok());
        assert_builds(&sites, &[]);
    }
}

#[tokio::test]
async fn bulk_unknown_kind_reuses_error_enumeration_and_preserves_indexed_results() {
    for atomic in [true, false] {
        let registry = registry(true);
        let params = json!({"atomic":atomic,"items":[
            {"kind":"bulk_report","name":"first"},
            {"kind":"zz_bulk_unknown","name":"second"},
            {"kind":"zz_bulk_unknown","name":"third"},
            {"kind":"document","entity_type":"bulk_rep","name":"fourth"}
        ]});
        let (result, sites) = observed(registry.dispatch("create", params)).await;
        assert_builds(&sites, &["subtype_kind_351"]);
        if atomic {
            let message = result.unwrap_err().to_string();
            assert!(message.contains("items[1].kind"), "{message}");
            assert!(message.contains("bulk_report"), "{message}");
            let listed = registry
                .dispatch("list", json!({"kind":"entity","limit":100}))
                .await
                .unwrap();
            assert!(listed["items"].as_array().unwrap().is_empty());
        } else {
            let response = result.unwrap();
            assert_eq!(response["created"], 2);
            assert_eq!(response["failed"], 2);
            for index in 0..4 {
                assert_eq!(response["results"][index]["index"], index);
            }
            assert_eq!(response["results"][0]["ok"], true);
            assert_eq!(response["results"][1]["ok"], false);
            assert_eq!(response["results"][2]["ok"], false);
            assert_eq!(response["results"][3]["ok"], true);
            let encoded = serde_json::to_string(&response).unwrap();
            assert!(encoded.contains("bulk_report"));
            assert!(encoded.contains("items[1].kind"));
        }
    }
}

#[tokio::test]
async fn unpinned_type_filter_shared_context_counts_actual_compositions() {
    // Bulk entities always pin a canonical base kind. This observes the real
    // shared helper's unpinned arm, not a claim that bulk dispatch reaches it.
    let registry = registry(true);
    let context = BulkKindContext::default();
    let ((), sites) = observed(async {
        for _ in 0..1000 {
            assert_eq!(
                validate_entity_type_filter_with_context(
                    None,
                    Some("bulk_rep"),
                    &registry,
                    Some(&context)
                )
                .unwrap(),
                Some("bulk_report".into())
            );
        }
        assert!(validate_entity_type_filter_with_context(
            None,
            Some("bulk_shared"),
            &registry,
            Some(&context)
        )
        .unwrap_err()
        .to_string()
        .contains("ambiguous entity_type"));
        assert_eq!(
            validate_entity_type_filter_with_context(
                None,
                Some("bulk_same_alias"),
                &registry,
                Some(&context)
            )
            .unwrap(),
            Some("bulk_same_type".into())
        );
        assert_eq!(
            validate_entity_type_filter_with_context(
                Some("concept"),
                Some("bulk_shared"),
                &registry,
                Some(&context)
            )
            .unwrap(),
            Some("bulk_concept".into())
        );
    })
    .await;
    assert_builds(&sites, &["unpinned_filter_124"]);
}

#[tokio::test]
async fn bulk_registry_scope_subtypes_notes_and_conflicts_are_preserved() {
    let loaded = registry(true);
    let unloaded = registry(false);
    for registry in [&loaded, &unloaded, &loaded] {
        let (result, sites) = observed(registry.dispatch(
            "create",
            json!({"items":[{"kind":"bulk_report","entity_type":"bulk_rep","name":"precedence"}]}),
        ))
        .await;
        if std::ptr::eq(registry, &unloaded) {
            assert!(result.unwrap_err().to_string().contains("unknown kind"));
            assert_builds(&sites, &["subtype_kind_351"]);
        } else {
            let response = result.unwrap();
            assert_eq!(response["results"][0]["result"]["kind"], "document");
            assert_builds(&sites, &["subtype_kind_351"]);
        }
    }
    let (result, sites) = observed(loaded.dispatch(
        "create",
        json!({"items":[
            {"kind":"bulk_report","entity_type":"report","name":"conflict"}
        ]}),
    ))
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("contradicts entity_type"));
    assert_builds(&sites, &["subtype_kind_351"]);
    let (result, sites) = observed(loaded.dispatch(
        "create",
        json!({"atomic":false,"items":[
            {"kind":"bulk_note","content":"note body"},
            {"kind":"document","name":"plain entity"},
            {"kind":"bulk_same_type","name":"ambiguous subtype"}
        ]}),
    ))
    .await;
    let response = result.unwrap();
    assert_eq!(response["created"], 2);
    assert_eq!(response["failed"], 1);
    assert_eq!(response["results"][0]["result"]["kind"], "bulk_note");
    assert!(serde_json::to_string(&response)
        .unwrap()
        .contains("ambiguous kind"));
    assert_builds(&sites, &["subtype_kind_351"]);
}

// Real dispatch wire envelopes exported for owner original/candidate parity.
// UUIDs and timestamps are replaced explicitly because each create allocates
// a new identity and clock value. All other success fields remain byte-exact;
// error envelopes are emitted without normalization.
const GENERATED_SUCCESS_PATHS: [&str; 3] =
    ["/result/id", "/result/created_at", "/result/updated_at"];

fn stable_success(value: &mut Value, started: i64, completed: i64) {
    let id = value
        .pointer("/id")
        .and_then(Value::as_str)
        .expect("/result/id present");
    let parsed = Uuid::parse_str(id).expect("/result/id is a UUID");
    assert_eq!(
        parsed.to_string(),
        id,
        "canonical UUID before normalization"
    );
    for path in ["/created_at", "/updated_at"] {
        let raw = value
            .pointer(path)
            .and_then(Value::as_str)
            .expect("generated timestamp present");
        let micros = chrono::DateTime::parse_from_rfc3339(raw)
            .expect("generated timestamp well-formed")
            .timestamp_micros();
        assert!(
            started <= micros && micros <= completed,
            "{path}={raw} outside captured wall-clock interval {started}..={completed}"
        );
    }
    value["id"] = json!("<generated_uuid>");
    value["created_at"] = json!("<generated_timestamp>");
    value["updated_at"] = json!("<generated_timestamp>");
}

#[tokio::test]
async fn singleton_wire_parity_receipts() {
    let registry = registry(true);
    for (label, params) in [
        (
            "subtype_success",
            json!({"kind":"bulk_report","entity_type":"bulk_rep","name":"single subtype","skip_dedup_check":true}),
        ),
        (
            "base_success",
            json!({"kind":"document","entity_type":"bulk_rep","name":"single base","skip_dedup_check":true}),
        ),
        (
            "note_success",
            json!({"kind":"bulk_note","content":"single note"}),
        ),
        (
            "conflict_error",
            json!({"kind":"bulk_report","entity_type":"report","name":"single conflict","skip_dedup_check":true}),
        ),
        (
            "unknown_error",
            json!({"kind":"zz_bulk_unknown","name":"single unknown","skip_dedup_check":true}),
        ),
        (
            "malformed_error",
            json!({"kind":"document","name":"single malformed","entity_kind":7}),
        ),
    ] {
        let started = chrono::Utc::now().timestamp_micros();
        let dispatched = registry.dispatch("create", params).await;
        let completed = chrono::Utc::now().timestamp_micros();
        let response = match dispatched {
            Ok(mut value) => {
                stable_success(&mut value, started, completed);
                json!({"ok":true,"result":value})
            }
            Err(error) => {
                json!({"ok":false,"error":khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::NotCommitted)})
            }
        };
        if label.ends_with("success") {
            assert_eq!(response["ok"], true);
        } else {
            assert_eq!(response["ok"], false);
        }
        if label.ends_with("success") {
            println!(
                "3720_NORMALIZED_PATHS {label} {}",
                serde_json::to_string(&GENERATED_SUCCESS_PATHS).unwrap()
            );
        }
        println!(
            "3720_SINGLETON_PARITY {label} {}",
            serde_json::to_string(&response).unwrap()
        );
    }
}
