use super::*;
use crate::parsers::gql;

#[test]
fn node_kind_passes_through_unchanged() {
    // Entity kinds are pack-agnostic strings -- no normalization at the query layer.
    let mut q = gql::parse("MATCH (a:paper)-[:introduced_by]->(b:concept) RETURN a").unwrap();
    validate(&mut q).unwrap();
    let kinds: Vec<_> = q
        .pattern
        .nodes()
        .map(|n| n.kind.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(kinds, vec!["paper", "concept"]);
}

#[test]
fn normalises_relation_case_and_hyphens() {
    let mut q = gql::parse("MATCH (a)-[:Introduced_By]->(b) RETURN a").unwrap();
    validate(&mut q).unwrap();
    let rels: Vec<_> = q
        .pattern
        .edges()
        .flat_map(|e| e.relations.iter().cloned())
        .collect();
    assert_eq!(rels, vec!["introduced_by".to_string()]);
}

#[test]
fn rejects_unknown_relation() {
    let mut q = gql::parse("MATCH (a)-[:not_a_relation]->(b) RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not_a_relation"), "msg: {msg}");
}

#[test]
fn unknown_kind_passes_through() {
    // Entity kinds are pack-agnostic strings -- any string is accepted at the query layer.
    let mut q = gql::parse("MATCH (a:gizmo)-[:extends]->(b) RETURN a").unwrap();
    validate(&mut q).unwrap();
}

#[test]
fn rejects_depth_above_max() {
    // Exceeding MAX_DEPTH is an InvalidInput error, not a silent clamp.
    let mut q = gql::parse("MATCH (a)-[:extends*1..50]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
    assert!(
        err.to_string().contains("50"),
        "error should mention requested depth: {err}"
    );
}

#[test]
fn rejects_depth_above_max_warnings_path() {
    // validate_with_warnings must also reject (not clamp + warn).
    let mut q = gql::parse("MATCH (a)-[:extends*1..50]->(b) RETURN b").unwrap();
    let err = validate_with_warnings(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

#[test]
fn multi_relation_all_normalised() {
    let mut q = gql::parse("MATCH (a)-[:Extends|VARIANT_OF]->(b) RETURN a").unwrap();
    validate(&mut q).unwrap();
    let edge = q.pattern.edges().next().unwrap();
    assert_eq!(
        edge.relations,
        vec!["extends".to_string(), "variant_of".to_string()]
    );
}

#[test]
fn rejects_namespace_in_where() {
    let mut q =
        gql::parse("MATCH (a:concept)-[:extends]->(b) WHERE a.namespace = 'other' RETURN a")
            .unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(err.to_string().contains("namespace"), "msg: {err}");
}

#[test]
fn rejects_namespace_in_node_properties() {
    let mut q =
        gql::parse("MATCH (a:concept {namespace: 'other'})-[:extends]->(b) RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(err.to_string().contains("namespace"), "msg: {err}");
}

#[test]
fn rejects_unknown_relation_in_where() {
    let mut q =
        gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'related_to' RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(err.to_string().contains("related_to"), "msg: {err}");
}

#[test]
fn query_edge_relation_bang_rejected() {
    // Regression for #471: relation filters with punctuation must be
    // rejected as Validation errors, not silently normalised into a
    // canonical relation.
    for bad in ["supports!", "part/of", "depends.on", "competes with"] {
        let mut q = gql::parse(&format!(
            "MATCH (a)-[e:extends]->(b) WHERE e.relation = '{bad}' RETURN a"
        ))
        .unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Validation(_)),
            "relation {bad:?} must be QueryError::Validation, got: {err:?}"
        );
    }
}

fn first_condition_string_value(q: &GqlQuery) -> String {
    match q.where_clause.conditions().next().unwrap().value {
        ConditionValue::String(ref s) => s.clone(),
        _ => panic!("expected string condition value"),
    }
}

#[test]
fn unknown_kind_in_where_passes_through() {
    // Entity kinds are pack-agnostic strings -- any kind string is accepted.
    let mut q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'gizmo' RETURN a").unwrap();
    validate(&mut q).unwrap();
    assert_eq!(first_condition_string_value(&q), "gizmo");
}

#[test]
fn kind_in_where_passes_through_unchanged() {
    // Pack-agnostic: 'paper' is not normalized to 'document'; strings pass through as-is.
    let mut q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'paper' RETURN a").unwrap();
    validate(&mut q).unwrap();
    assert_eq!(first_condition_string_value(&q), "paper");
}

#[test]
fn normalises_relation_alias_in_where() {
    let mut q =
        gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'Introduced_By' RETURN a")
            .unwrap();
    validate(&mut q).unwrap();
    assert_eq!(first_condition_string_value(&q), "introduced_by");
}

#[test]
fn rejects_zero_hop_range_gql_wide() {
    let mut q = gql::parse("MATCH (a)-[:extends*0..3]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_zero_hop_range_gql_narrow() {
    // *0..1 has max_hops=1 so has_variable_length() is false, but the
    // fixed-length compiler also can't produce zero-hop rows -- reject at
    // validation regardless of compile path.
    let mut q = gql::parse("MATCH (a)-[:extends*0..1]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_zero_hop_sparql_explicit_range() {
    use crate::parsers::sparql;
    let mut q = sparql::parse("SELECT ?a ?b WHERE { ?a :extends{0,3} ?b . }").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_repeated_node_var_cycle_gql() {
    let mut q = gql::parse("MATCH (a)-[:extends]->(b)-[:variant_of]->(a) RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_repeated_node_var_self_reach_variable_length() {
    let mut q = gql::parse("MATCH (a)-[:extends*1..3]->(a) RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_repeated_node_var_cycle_sparql() {
    use crate::parsers::sparql;
    let mut q = sparql::parse("SELECT ?a WHERE { ?a :extends ?b . ?b :variant_of ?a . }").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_repeated_edge_var() {
    let mut q = gql::parse("MATCH (a)-[e:extends]->(b)-[e:variant_of]->(c) RETURN c").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_inverted_range() {
    // *3..1 is an inverted range -- must error, not silently rewrite to *1..1.
    let mut q = gql::parse("MATCH (a)-[:extends*3..1]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "expected Validation error, got {err:?}"
    );
}

#[test]
fn rejects_min_hops_above_depth_cap() {
    // min=50, max=100 -- the lower bound exceeds MAX_DEPTH so the query
    // can never produce results within our cap.
    let mut q = gql::parse("MATCH (a)-[:extends*50..100]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_max_above_depth_cap_with_satisfiable_min() {
    // *2..50 -- min 2 is satisfiable but max 50 exceeds MAX_DEPTH; must error.
    let mut q = gql::parse("MATCH (a)-[:extends*2..50]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

// --- Regression: observed_as_* bypass fix ---

#[test]
fn rejects_unknown_synthetic_relation() {
    // observed_as_bogus is not in SYNTHETIC_RELATIONS -- must be rejected, not
    // silently compiled as a graph_edges query (closed-ontology bypass fix).
    let mut q = gql::parse("MATCH (a)-[:observed_as_bogus]->(b) RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "expected Validation error for unknown synthetic relation, got {err:?}"
    );
    assert!(
        err.to_string().contains("observed_as_bogus"),
        "error must name the unknown relation: {err}"
    );
}

#[test]
fn accepts_known_synthetic_relation() {
    // All four known observed_as_* relations must pass validation.
    for rel in &[
        "observed_as_candidate",
        "observed_as_selected",
        "observed_as_target",
        "observed_as_signal",
    ] {
        let input = format!("MATCH (ev)-[:{rel}]->(m) RETURN ev, m");
        let mut q = gql::parse(&input).unwrap();
        validate(&mut q)
            .unwrap_or_else(|_| panic!("known synthetic relation '{rel}' must pass validation"));
    }
}

// --- Regression: public AST pattern shape fix ---

#[test]
fn validate_pattern_shape_rejects_even_element_count() {
    use crate::ast::{EdgeDirection, EdgePattern, PatternElement};
    // A hand-constructed AST with only an Edge element (no surrounding nodes) is malformed.
    let elements = vec![PatternElement::Edge(EdgePattern {
        variable: None,
        relations: vec!["extends".to_string()],
        direction: EdgeDirection::Out,
        min_hops: 1,
        max_hops: 1,
    })];
    let err = validate_pattern_shape(&elements).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "expected Validation error for even element count, got {err:?}"
    );
}

#[test]
fn validate_pattern_shape_rejects_wrong_type_at_position() {
    use crate::ast::{EdgeDirection, EdgePattern, NodePattern, PatternElement};
    use std::collections::HashMap;
    // Edge, Node, Edge -- wrong: index 0 must be Node, index 2 must be Node.
    let make_node = || {
        PatternElement::Node(NodePattern {
            variable: None,
            kind: None,
            entity_type: None,
            properties: HashMap::new(),
        })
    };
    let make_edge = || {
        PatternElement::Edge(EdgePattern {
            variable: None,
            relations: vec!["extends".to_string()],
            direction: EdgeDirection::Out,
            min_hops: 1,
            max_hops: 1,
        })
    };
    // Node, Node, Node -- two nodes in a row at odd index is wrong
    let elements = vec![make_node(), make_node(), make_node()];
    let err = validate_pattern_shape(&elements).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "expected Validation error for Node at odd index, got {err:?}"
    );
    // Edge, Node, Edge -- edge at even index is wrong
    let elements2 = vec![make_edge(), make_node(), make_edge()];
    let err2 = validate_pattern_shape(&elements2).unwrap_err();
    assert!(
        matches!(err2, QueryError::Validation(_)),
        "expected Validation error for Edge at even index, got {err2:?}"
    );
    // Valid: Node, Edge, Node
    let elements3 = vec![make_node(), make_edge(), make_node()];
    validate_pattern_shape(&elements3).expect("Node, Edge, Node must be valid");
}

#[test]
fn node_property_named_relation_allowed() {
    // `relation` on a node variable is a free-form JSON property, not the
    // edge relation column -- taxonomy enforcement should not apply.
    let mut q =
        gql::parse("MATCH (a)-[:extends]->(b) WHERE a.relation = 'external' RETURN a").unwrap();
    validate(&mut q).unwrap();
    assert_eq!(first_condition_string_value(&q), "external");
}

#[test]
fn edge_relation_still_validated() {
    // `relation` on an edge variable must still go through EdgeRelation
    // taxonomy validation.
    let mut q =
        gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'not_real' RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(err.to_string().contains("not_real"), "msg: {err}");
}
