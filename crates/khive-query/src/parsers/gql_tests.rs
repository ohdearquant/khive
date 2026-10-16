use super::*;

include!("gql_tests/patterns_and_paging_tests.rs");

include!("gql_tests/predicates_and_directions_tests.rs");

#[test]
fn comma_separated_match_patterns_name_unsupported_construct() {
    let err = parse("MATCH (a)-[e1]->(b), (a)-[e2]->(b) RETURN a, b").unwrap_err();
    assert!(
        matches!(&err, QueryError::Unsupported(_)),
        "multiple MATCH patterns must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("comma-separated MATCH patterns"),
        "error must name the unsupported construct; got: {msg}"
    );
    assert!(
        msg.contains("e.id") && msg.contains("e.relation"),
        "error must preserve edge identity in its alternative; got: {msg}"
    );
}

#[test]
fn return_aliases_name_unsupported_construct() {
    for query in [
        "MATCH (a)-[e]->(b) RETURN a.name AS src, b.name AS dst LIMIT 5",
        "MATCH (a) RETURN a as node",
        "MATCH (a)-[e]->(b) RETURN a.name, b.name aS dst",
    ] {
        let err = parse(query).unwrap_err();
        assert!(
            matches!(&err, QueryError::Unsupported(_)),
            "RETURN alias must return Unsupported for {query:?}; got {err:?}"
        );
        assert!(
            err.to_string().contains("aliases (`AS`)"),
            "error must name AS for {query:?}; got: {err}"
        );
    }
}

#[test]
fn documented_single_path_parallel_edge_audit_parses() {
    let q = parse("MATCH (a)-[e]->(b) RETURN a.id, e.id, e.relation, b.id").unwrap();
    assert_eq!(
        q.return_items,
        vec![
            ReturnItem::Property("a".into(), "id".into()),
            ReturnItem::Property("e".into(), "id".into()),
            ReturnItem::Property("e".into(), "relation".into()),
            ReturnItem::Property("b".into(), "id".into()),
        ]
    );
}

#[test]
fn gql_dialect_reference_matches_parser_contract() {
    const DOCS: &str = include_str!("../../docs/api/parsing.md");
    const SUPPORTED: &str = "MATCH (a)-[e]->(b) RETURN a.id, e.id, e.relation, b.id";
    const MULTI_PATTERN: &str = "MATCH (a)-[e1]->(b), (a)-[e2]->(b) RETURN a, b";
    const ALIAS: &str = "MATCH (a)-[e]->(b) RETURN a.name AS src, b.name AS dst LIMIT 5";

    for query in [SUPPORTED, MULTI_PATTERN, ALIAS] {
        assert!(
            DOCS.contains(query),
            "dialect reference must include contract example: {query}"
        );
    }

    parse(SUPPORTED).expect("documented single-path alternative must parse");
    for (query, feature) in [
        (MULTI_PATTERN, "comma-separated MATCH patterns"),
        (ALIAS, "aliases (`AS`)"),
    ] {
        let err = parse(query).unwrap_err();
        assert!(
            matches!(&err, QueryError::Unsupported(_)),
            "documented unsupported form must return Unsupported; got {err:?}"
        );
        assert!(
            err.to_string().contains(feature),
            "documented error must name {feature:?}; got: {err}"
        );
    }
}

#[test]
fn node_pattern_entity_type_lifted_from_properties() {
    let q = parse("MATCH (n:document {entity_type: 'paper'}) RETURN n").unwrap();
    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(
        nodes[0].entity_type.as_deref(),
        Some("paper"),
        "entity_type must be lifted into NodePattern.entity_type"
    );
    assert!(
        !nodes[0].properties.contains_key("entity_type"),
        "entity_type must be removed from the properties map after lifting"
    );
}

#[test]
fn gql_create_rejected_with_readonly_message() {
    let err = parse("CREATE (n:concept {name: 'X'}) RETURN n").unwrap_err();
    assert!(
        matches!(err, crate::error::QueryError::Unsupported(_)),
        "GQL CREATE must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("read-only"),
        "error must mention 'read-only'; got: {msg}"
    );
    assert!(
        msg.contains("create") && msg.contains("update") && msg.contains("delete"),
        "error must name the mutation verbs; got: {msg}"
    );
}

#[test]
fn gql_delete_rejected_with_readonly_message() {
    let err = parse("DELETE (n) WHERE n.kind = 'concept'").unwrap_err();
    assert!(
        matches!(err, crate::error::QueryError::Unsupported(_)),
        "GQL DELETE must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn gql_set_rejected_with_readonly_message() {
    let err = parse("SET (n:concept) RETURN n").unwrap_err();
    assert!(
        matches!(err, crate::error::QueryError::Unsupported(_)),
        "GQL SET must return Unsupported; got {err:?}"
    );
}

#[test]
fn gql_merge_rejected_with_readonly_message() {
    let err = parse("MERGE (n:concept {name: 'X'}) RETURN n").unwrap_err();
    assert!(
        matches!(err, crate::error::QueryError::Unsupported(_)),
        "GQL MERGE must return Unsupported; got {err:?}"
    );
}

#[test]
fn gql_match_still_compiles_after_write_guard() {
    let q = parse("MATCH (a:concept)-[:extends]->(b) RETURN a").unwrap();
    assert!(!q.pattern.elements.is_empty(), "valid MATCH must parse");
}

#[test]
fn gql_detach_delete_rejected() {
    let err = parse("DETACH DELETE (n)").unwrap_err();
    assert!(
        matches!(err, crate::error::QueryError::Unsupported(_)),
        "DETACH DELETE must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("read-only"),
        "error must mention 'read-only'; got: {msg}"
    );
    assert!(
        msg.contains("create") && msg.contains("update") && msg.contains("delete"),
        "error must name the mutation verbs; got: {msg}"
    );
}
