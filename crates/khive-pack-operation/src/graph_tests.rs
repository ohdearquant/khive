use serde_json::json;

use super::*;

fn step_type(value: Value) -> StepType {
    serde_json::from_value(value).expect("a step type")
}

fn operation_type(value: Value) -> OperationType {
    serde_json::from_value(value).expect("an operation type")
}

/// One field produced both about the operation and about a slot is one edge that says both.
#[test]
fn a_field_produced_about_the_operation_and_a_slot_is_one_edge_saying_both() {
    let step = step_type(json!({
        "kind": "step_type", "id": "issue", "version": 1, "name": "issue", "act": "issue",
        "by": "clerk",
        "produces": [{"field": "permit number", "class": "stated"},
                     {"field": "permit number", "slot": "holder", "class": "stated"}]
    }));
    let graph = TypeGraph::of_step_type(&step, "blake3:v1");
    let produces: Vec<_> = graph
        .edges
        .iter()
        .filter(|(_, properties)| properties.get("role") == Some(&json!("produces")))
        .collect();
    assert_eq!(produces.len(), 1, "{produces:?}");
    assert_eq!(produces[0].1["about_operation"], json!(true));
    assert_eq!(produces[0].1["slots"], json!(["holder"]));
}

/// A produces edge about neither the operation nor a slot is refused. No document gives one,
/// since each edge comes from at least one product, so the graph is built by hand.
#[test]
fn a_produces_edge_about_nothing_is_refused() {
    let mut graph = TypeGraph::default();
    graph.edges.insert(
        EdgeKey {
            source: "operation/step_type/issue".to_owned(),
            relation: "links_to",
            target: Target::Field("permit number".to_owned()),
        },
        Map::from_iter([
            ("role".to_owned(), json!("produces")),
            ("about_operation".to_owned(), json!(false)),
            ("slots".to_owned(), json!([])),
        ]),
    );
    let refusal = check_products(&graph).expect_err("about nothing");
    assert_eq!(refusal.code, "invalid_product");
}

#[test]
fn keys_follow_section_2_and_a2_10() {
    assert_eq!(
        definition_key(OPERATION_TYPE, "onboard"),
        "operation/operation_type/onboard"
    );
    assert_eq!(
        definition_key(STEP_TYPE, "onboard/sign"),
        "operation/step_type/onboard/sign"
    );
    assert_eq!(
        slot_key("onboard", "holder"),
        "operation/slot/onboard/holder"
    );
    assert_eq!(
        requirement_key("verify", Some("holder"), "legal name"),
        "operation/requirement/verify/holder/legal name"
    );
    assert_eq!(
        requirement_key("onboard/sign", None, "date"),
        "operation/requirement/onboard/sign//date"
    );
    assert_eq!(
        name_key("tax", SLOT, Some("onboard"), "filer"),
        "operation/tax/slot/onboard/filer"
    );
    assert_eq!(
        name_key("tax", OPERATION_TYPE, None, "intake"),
        "operation/tax/operation_type/intake"
    );
}

#[test]
fn escaping_keeps_the_segment_count() {
    assert_eq!(escape("a/b"), "a%2Fb");
    assert_eq!(escape("50%"), "50%25");
    // `%2F` written literally is escaped too, so it never reads back as a slash.
    assert_eq!(escape("a%2Fb"), "a%252Fb");
    let slash = requirement_key("verify", Some("holder"), "a/b");
    let literal = requirement_key("verify", Some("holder"), "a%2Fb");
    assert_ne!(slash, literal);
    assert_eq!(slash.split('/').count(), 5);
    assert_eq!(literal.split('/').count(), 5);
}

#[test]
fn a_key_over_512_bytes_is_refused_as_invalid_name() {
    let at_limit = "x".repeat(MAX_KEY_BYTES);
    assert!(check_key_length(&at_limit, "slot s").is_ok());
    let over = "x".repeat(MAX_KEY_BYTES + 1);
    let refusal = check_key_length(&over, "slot s").unwrap_err();
    assert_eq!(refusal.code, "invalid_name");
    assert!(refusal.message.contains("513 bytes"), "{}", refusal.message);
}

#[test]
fn a_step_type_shows_its_requirements_and_one_produces_edge_per_field() {
    let step = step_type(json!({
        "kind": "step_type", "id": "verify", "version": 3, "name": "verify",
        "act": "verify", "by": "holder",
        "requires": [{"field": "legal name", "slot": "holder", "value_type": "text"},
                     {"field": "date", "value_type": "date"}],
        "produces": [{"field": "decision", "slot": "holder", "class": "stated"},
                     {"field": "decision", "slot": "agent", "class": "stated"},
                     {"field": "receipt", "class": "read"}]
    }));
    let graph = TypeGraph::of_step_type(&step, "blake3:v3");
    let key = "operation/step_type/verify";
    let node = &graph.nodes[key];
    assert_eq!(node.entity_type, STEP_TYPE);
    assert_eq!(node.properties["version"], json!(3));
    assert_eq!(node.properties["digest"], json!("blake3:v3"));
    assert!(graph
        .nodes
        .contains_key("operation/requirement/verify/holder/legal name"));
    assert!(graph
        .nodes
        .contains_key("operation/requirement/verify//date"));
    assert_eq!(graph.nodes.len(), 3);

    let produces = |field: &str| {
        graph
            .edges
            .get(&EdgeKey {
                source: key.to_owned(),
                relation: "links_to",
                target: Target::Field(field.to_owned()),
            })
            .cloned()
    };
    let decision = produces("decision").expect("one produces edge");
    assert_eq!(decision["slots"], json!(["agent", "holder"]));
    assert_eq!(decision["about_operation"], json!(false));
    let receipt = produces("receipt").expect("about the operation");
    assert_eq!(receipt["slots"], json!([]));
    assert_eq!(receipt["about_operation"], json!(true));
    assert!(check_products(&graph).is_ok());
    // Two contains, two requires and two produces.
    assert_eq!(graph.edges.len(), 6);
    // A requirement holds its slot by name; no edge runs to a slot node.
    assert!(graph
        .edges
        .keys()
        .all(|edge| !matches!(&edge.target, Target::Node(t) if t.starts_with("operation/slot/"))));
}

#[test]
fn two_steps_pinning_one_step_type_are_one_pins_edge_listing_both() {
    let pin = json!({"id": "verify", "version": 1, "digest": "blake3:v1"});
    let type_ = operation_type(json!({
        "kind": "operation_type", "id": "onboard", "version": 2,
        "slots": [{"name": "holder", "repeats": false, "required": true}],
        "steps": [
            {"name": "first", "step_type": pin, "after": [], "outcome": false},
            {"name": "second", "step_type": pin, "after": ["first"], "outcome": true,
             "derived_after": ["first"]}
        ],
        "rules": []
    }));
    let graph = TypeGraph::of_operation_type(&type_, "blake3:t2");
    assert_eq!(
        graph.nodes["operation/operation_type/onboard"].properties["version"],
        json!(2)
    );
    assert!(graph.nodes.contains_key("operation/slot/onboard/holder"));
    let pins = &graph.edges[&EdgeKey {
        source: "operation/operation_type/onboard".to_owned(),
        relation: "contains",
        target: Target::Node("operation/step_type/verify".to_owned()),
    }];
    assert_eq!(pins["role"], json!("pins"));
    assert_eq!(pins["version"], json!(1));
    assert_eq!(pins["digest"], json!("blake3:v1"));
    let steps = pins["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1]["name"], json!("second"));
    assert_eq!(steps[1]["derived_after"], json!(["first"]));
    // One contains to the slot and one pins edge.
    assert_eq!(graph.edges.len(), 2);
}

#[test]
fn the_declared_edge_rules_cover_every_edge_the_graph_builds() {
    let step = step_type(json!({
        "kind": "step_type", "id": "verify", "version": 1, "name": "verify",
        "act": "verify", "by": "holder",
        "requires": [{"field": "legal name", "slot": "holder", "value_type": "text"}],
        "produces": [{"field": "decision", "class": "stated"}]
    }));
    let type_ = operation_type(json!({
        "kind": "operation_type", "id": "onboard", "version": 1,
        "slots": [{"name": "holder", "repeats": false, "required": true}],
        "steps": [{"name": "verify", "step_type": {"id": "verify", "version": 1, "digest": "d"},
                    "after": [], "outcome": true}],
        "rules": []
    }));
    let mut graph = TypeGraph::of_step_type(&step, "d");
    graph.extend(TypeGraph::of_operation_type(&type_, "t"));
    let type_of = |target: &Target| match target {
        Target::Node(key) => graph.nodes[key].entity_type,
        Target::Field(_) => "field",
    };
    for edge in graph.edges.keys() {
        let source = graph.nodes[&edge.source].entity_type;
        let target = type_of(&edge.target);
        let relation = relation_of(edge.relation);
        assert!(
            OPERATION_EDGE_RULES
                .iter()
                .any(|rule| rule.relation == relation
                    && rule.source == typed(source)
                    && rule.target == typed(target)),
            "no rule for {source} {} {target}",
            edge.relation
        );
    }
}
