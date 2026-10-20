use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use super::{NeighborProjection, NeighborsParams};
use crate::handler_defs::KG_HANDLERS;

const NODE_ID: &str = "11111111-1111-4111-8111-111111111111";

fn neighbors_wire_fixture() -> [(&'static str, Value); 9] {
    [
        ("node_id", json!(NODE_ID)),
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
