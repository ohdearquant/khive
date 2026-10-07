use khive_query::{
    compile, parse, parse_auto, CompileOptions, GqlQuery, QueryError, QueryLanguage, ReturnItem,
};

fn public_parses(input: &str) -> [Result<GqlQuery, QueryError>; 3] {
    [
        parse(QueryLanguage::Gql, input),
        parse_auto(input),
        khive_query::parsers::gql::parse(input),
    ]
}

#[test]
fn digit_leading_edge_bindings_fail_at_the_binding_position() {
    for (input, expected_position) in [
        ("MATCH (a)-[123:extends]->(b) RETURN 123.id", 11),
        ("MATCH (a)-[1x]->(b) RETURN 1x", 11),
        ("MATCH (a)-[123:extends*1..3]->(b) RETURN 123.id", 11),
        ("MATCH (a)-[1x*1..3]->(b) RETURN 1x", 11),
        ("MATCH (a)-[1edge:extends]->(b) RETURN 1edge", 11),
        ("MATCH (a)-[9_:extends*1..3]->(b) RETURN 9_.id", 11),
        ("MATCH (a)-[  123:extends]->(b) RETURN 123.id", 13),
        ("MATCH (é)-[123:extends]->(b) RETURN 123.id", 11),
    ] {
        for result in public_parses(input) {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, QueryError::Parse { position, message }
                    if *position == expected_position
                        && message == "edge variable must start with a letter or '_'"),
                "{input}: {error:?}"
            );
        }
    }
}

#[test]
fn valid_edge_binding_spelling_and_projection_are_preserved() {
    for (variable, body) in [
        ("_x", "_x"),
        ("x1", "x1"),
        ("e1", "e1:extends"),
        ("E1", "E1:extends"),
        ("_9", "_9:extends"),
        ("é2", "é2:extends"),
    ] {
        for (hops, source) in [("", "e0.id"), ("*1..3", "t.via_edge")] {
            let input = format!("MATCH (a)-[{body}{hops}]->(b) RETURN {variable}.id");
            for result in public_parses(&input) {
                let query = result.unwrap();
                assert_eq!(
                    query.pattern.edges().next().unwrap().variable.as_deref(),
                    Some(variable)
                );
                assert_eq!(
                    query.return_items,
                    vec![ReturnItem::Property(variable.into(), "id".into())]
                );
                let compiled = compile(&query, &CompileOptions::default()).unwrap();
                assert_eq!(compiled.sql.contains("WITH RECURSIVE"), !hops.is_empty());
                let projection = format!("{source} AS {variable}_id");
                assert_eq!(
                    compiled.sql.matches(&projection).count(),
                    1,
                    "{}",
                    compiled.sql
                );
            }
        }
    }
}

#[test]
fn anonymous_edges_keep_their_fixed_and_recursive_forms() {
    for body in [":extends", "*1..2", ":extends*1..3", "*1..3"] {
        let input = format!("MATCH (a)-[{body}]->(b) RETURN a.id");
        for result in public_parses(&input) {
            let query = result.unwrap();
            assert_eq!(query.pattern.edges().next().unwrap().variable, None);
            let compiled = compile(&query, &CompileOptions::default()).unwrap();
            assert_eq!(compiled.sql.contains("WITH RECURSIVE"), body.contains('*'));
        }
    }
}

#[test]
fn the_binding_guard_does_not_change_relation_or_property_identifier_parsing() {
    for body in [":123", "e1:123"] {
        let input = format!("MATCH (a)-[{body}]->(b) RETURN a.id");
        for result in public_parses(&input) {
            let query = result.unwrap();
            assert_eq!(query.pattern.edges().next().unwrap().relations, vec!["123"]);
            assert!(matches!(
                compile(&query, &CompileOptions::default()),
                Err(QueryError::Validation(_))
            ));
        }
    }
    for result in public_parses("MATCH (a {123: 1})-[:extends]->(b) RETURN a.id") {
        let query = result.unwrap();
        assert!(query
            .pattern
            .nodes()
            .next()
            .unwrap()
            .properties
            .contains_key("123"));
        compile(&query, &CompileOptions::default()).unwrap();
    }
}
