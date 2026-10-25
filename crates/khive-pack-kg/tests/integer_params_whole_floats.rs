//! Integer parameters accept whole-number floats (`5.0`), as some MCP clients encode every JSON
//! number with a decimal point, and still refuse fractional ones (`5.5`).

use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

const ACTOR: &str = "kg-integer-params-fixture";

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        packs: vec!["kg".into()],
        actor_id: Some(ACTOR.into()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("KG registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn seed(registry: &VerbRegistry, count: usize) {
    for i in 0..count {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": format!("lighthouse keeper {i}"),
                    "description": "A lighthouse keeper record used to page through search and list results.",
                    "skip_dedup_check": true
                }),
            )
            .await
            .expect("seed create");
    }
}

fn hit_ids(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("search hit array: {value}"))
        .iter()
        .map(|hit| hit["id"].clone())
        .collect()
}

fn items(value: &Value) -> usize {
    value["items"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_else(|| panic!("list items: {value}"))
}

fn refused_as_bad_params(result: Result<Value, RuntimeError>, what: &str) {
    match result {
        Err(RuntimeError::InvalidInput(message)) => {
            assert!(message.contains("bad params"), "{what}: {message}");
            assert!(message.contains("2.5"), "{what}: {message}");
        }
        other => panic!("{what}: expected a bad params refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn search_takes_a_whole_float_limit_as_the_integer() {
    let registry = fixture();
    seed(&registry, 4).await;
    let with_integer = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "lighthouse keeper", "limit": 2}),
        )
        .await
        .expect("integer limit");
    let with_float = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "lighthouse keeper", "limit": 2.0}),
        )
        .await
        .expect("whole-float limit");
    assert_eq!(hit_ids(&with_integer).len(), 2, "{with_integer}");
    assert_eq!(hit_ids(&with_float), hit_ids(&with_integer));
}

#[tokio::test]
async fn list_takes_a_whole_float_limit_as_the_integer() {
    let registry = fixture();
    seed(&registry, 3).await;
    let listed = registry
        .dispatch("list", json!({"kind": "concept", "limit": 2.0}))
        .await
        .expect("whole-float limit");
    assert_eq!(items(&listed), 2, "{listed}");
}

#[tokio::test]
async fn the_validated_search_request_takes_a_whole_float_limit() {
    let registry = fixture();
    let request = ValidatedSearchRequest::from_value(
        json!({"kind": "entity", "query": "lighthouse keeper", "limit": 2.0}),
        &registry,
    )
    .expect("whole-float limit");
    assert_eq!(request.limit(), 2);
    refused_as_bad_params(
        ValidatedSearchRequest::from_value(
            json!({"kind": "entity", "query": "lighthouse keeper", "limit": 2.5}),
            &registry,
        )
        .map(|_| Value::Null),
        "validated search request",
    );
}

#[tokio::test]
async fn a_fractional_limit_is_still_refused() {
    let registry = fixture();
    seed(&registry, 1).await;
    refused_as_bad_params(
        registry
            .dispatch(
                "search",
                json!({"kind": "entity", "query": "lighthouse keeper", "limit": 2.5}),
            )
            .await,
        "search",
    );
    refused_as_bad_params(
        registry
            .dispatch("list", json!({"kind": "concept", "limit": 2.5}))
            .await,
        "list",
    );
}
