use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use super::{NeighborProjection, NeighborsParams};
use crate::handler_defs::KG_HANDLERS;

const NODE_ID: &str = "11111111-1111-4111-8111-111111111111";

fn neighbors_wire_fixture() -> [(&'static str, Value); 9] {
    [
        ("id", json!(NODE_ID)),
        ("direction", json!("both")),
        ("relations", json!(["derived_from"])),
        ("min_weight", json!(0.25)),
        ("limit", json!(10)),
        ("after", json!("")),
        ("neighbor_kinds", json!(["concept"])),
        ("projection", json!("summary")),
        ("include_entity_type", json!(true)),
    ]
}

#[test]
fn neighbors_advertised_parameters_match_typed_wire_fixture() {
    let fixtures = neighbors_wire_fixture();
    let advertised = KG_HANDLERS
        .iter()
        .find(|handler| handler.name == "neighbors")
        .expect("neighbors must remain an advertised handler");
    let advertised_names: BTreeSet<_> = advertised.params.iter().map(|param| param.name).collect();
    let fixture_names: BTreeSet<_> = fixtures.iter().map(|(name, _)| *name).collect();

    assert_eq!(advertised.params.len(), fixtures.len());
    assert_eq!(advertised_names, fixture_names);

    let input: Map<String, Value> = fixtures
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
    let params: NeighborsParams = serde_json::from_value(Value::Object(input))
        .unwrap_or_else(|error| panic!("advertised neighbors wire fixture must decode: {error}"));

    assert_eq!(params.id, NODE_ID);
    assert_eq!(params.direction.as_deref(), Some("both"));
    assert_eq!(params.relations, Some(vec!["derived_from".to_owned()]));
    assert_eq!(params.min_weight, Some(0.25));
    assert_eq!(params.limit, Some(10));
    assert_eq!(params.after.as_deref(), Some(""));
    assert_eq!(params.neighbor_kinds, Some(vec!["concept".to_owned()]));
    assert!(matches!(
        params.projection,
        Some(NeighborProjection::Summary)
    ));
    assert_eq!(params.include_entity_type, Some(true));
}

#[test]
fn neighbors_endpoint_wire_spellings_remain_accepted() {
    for name in ["node_id", "id"] {
        let mut input = Map::new();
        input.insert(name.to_owned(), json!(NODE_ID));
        let params: NeighborsParams = serde_json::from_value(Value::Object(input))
            .unwrap_or_else(|error| panic!("neighbors endpoint spelling {name:?}: {error}"));
        assert_eq!(params.id, NODE_ID);
    }
}

#[test]
fn neighbors_unknown_wire_parameter_is_refused() {
    let mut input: Map<String, Value> = neighbors_wire_fixture()
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
    input.insert("neighbors_fixture_unknown".into(), json!(true));
    let error = match serde_json::from_value::<NeighborsParams>(Value::Object(input)) {
        Ok(_) => panic!("unknown neighbors wire parameter must be refused"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("unknown field `neighbors_fixture_unknown`"),
        "expected the specific unknown-field refusal: {error}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn neighbors_public_dispatch_preserves_node_id_alias() {
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};

    use crate::KgPack;

    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        events_split: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("KG registry");

    let source = registry
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "entity_kind": "concept",
                "name": "Neighbors canonical source",
                "description": "This source concept supports a private graph test that compares both accepted neighbor identifier spellings against the same stored directed edge and target."
            }),
        )
        .await
        .expect("create source concept");
    let target = registry
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "entity_kind": "concept",
                "name": "Neighbors canonical target",
                "description": "This target concept supports a private graph test that compares both accepted neighbor identifier spellings against the same stored directed edge and source."
            }),
        )
        .await
        .expect("create target concept");
    let source_id = source["id"].as_str().expect("source UUID").to_owned();
    let target_id = target["id"].as_str().expect("target UUID").to_owned();
    registry
        .dispatch(
            "link",
            json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "contains",
                "weight": 1.0
            }),
        )
        .await
        .expect("link the two concepts");

    let canonical = registry
        .dispatch("neighbors", json!({"id": source_id, "direction": "out"}))
        .await
        .expect("canonical id spelling");
    let hits = canonical.as_array().expect("unlimited neighbors array");
    assert_eq!(hits.len(), 1, "the graph must contain the known neighbor");
    let hit = &hits[0];
    assert_eq!(hit["id"], json!(target_id));
    assert_eq!(hit["origin_id"], json!(source_id));
    assert_eq!(hit["source_id"], json!(source_id));
    assert_eq!(hit["target_id"], json!(target_id));
    assert_eq!(hit["relation"], json!("contains"));
    assert_eq!(hit["weight"], json!(1.0));
    let edge_id: uuid::Uuid = hit["edge_id"]
        .as_str()
        .expect("stored edge UUID")
        .parse()
        .expect("canonical edge UUID");
    let edge = runtime
        .get_edge(&token, edge_id)
        .await
        .expect("read stored edge")
        .expect("edge must exist");
    assert_eq!(edge.source_id.to_string(), source_id);
    assert_eq!(edge.target_id.to_string(), target_id);

    let legacy = registry
        .dispatch(
            "neighbors",
            json!({"node_id": source_id, "direction": "out"}),
        )
        .await
        .expect("legacy node_id spelling");
    assert_eq!(
        legacy, canonical,
        "both spellings must return the full same neighbor records"
    );
}
