use khive_query::{
    compile, parse, parse_auto, validate, validate_with_warnings, CompileOptions, QueryError,
    QueryLanguage, ReturnItem,
};

fn assert_collision(error: QueryError) {
    assert!(
        matches!(error, QueryError::Validation(ref message)
            if message == "variable 'x' cannot bind both a node and an edge"),
        "{error:?}"
    );
}

#[test]
fn node_edge_name_reuse_is_rejected_by_validation_and_compilation() {
    let options = CompileOptions::default();
    for input in [
        "MATCH (x)-[x:extends]->(b) RETURN x.id",
        "MATCH (a)-[x:extends]->(x) RETURN x.id",
        "MATCH (x)-[x:extends*1..3]->(b) RETURN x.id",
        "MATCH (a)-[x:extends*1..3]->(x) RETURN x.id",
        "MATCH (x)-[e:extends]->(b)-[x:extends]->(c) RETURN x.id",
        "MATCH (a)-[x:extends]->(b)-[e:extends]->(x) RETURN x.id",
    ] {
        for parsed in [parse(QueryLanguage::Gql, input), parse_auto(input)] {
            let query = parsed.expect("binding validity is checked after syntax parsing");
            assert_collision(validate(&mut query.clone()).unwrap_err());
            assert_collision(validate_with_warnings(&mut query.clone()).unwrap_err());
            assert_collision(compile(&query, &options).unwrap_err());
        }
    }
}

#[test]
fn distinct_and_case_distinct_bindings_keep_their_projection_sources() {
    let options = CompileOptions::default();
    for (range, aliases) in [
        ("", ["n0.id", "e0.id", "n1.id"]),
        ("*1..3", ["s.id", "t.via_edge", "r.id"]),
    ] {
        for (node, edge) in [("a", "e"), ("x", "X")] {
            let input = format!(
                "MATCH ({node})-[{edge}:extends{range}]->(b) RETURN {node}.id, {edge}.id, b.id"
            );
            for parsed in [parse(QueryLanguage::Gql, &input), parse_auto(&input)] {
                let mut query = parsed.unwrap();
                validate(&mut query).unwrap();
                assert!(validate_with_warnings(&mut query).unwrap().is_empty());
                let compiled = compile(&query, &options).unwrap();
                assert_eq!(compiled.sql.contains("WITH RECURSIVE"), !range.is_empty());
                assert_eq!(
                    compiled.return_vars,
                    vec![
                        ReturnItem::Property(node.into(), "id".into()),
                        ReturnItem::Property(edge.into(), "id".into()),
                        ReturnItem::Property("b".into(), "id".into()),
                    ]
                );
                for (alias, variable) in aliases.into_iter().zip([node, edge, "b"]) {
                    let projection = format!("{alias} AS \"{variable}_id\"");
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
}

#[test]
fn anonymous_bindings_do_not_collide() {
    for (range, start_alias, end_alias) in [("", "n0.id", "n1.id"), ("*1..3", "s.id", "r.id")] {
        for (pattern, variable, alias) in [
            (format!("(a)-[:extends{range}]->()"), "a", start_alias),
            (format!("()-[:extends{range}]->(b)"), "b", end_alias),
        ] {
            let input = format!("MATCH {pattern} RETURN {variable}.id");
            for parsed in [parse(QueryLanguage::Gql, &input), parse_auto(&input)] {
                let mut query = parsed.unwrap();
                validate(&mut query).unwrap();
                assert!(validate_with_warnings(&mut query).unwrap().is_empty());
                let compiled = compile(&query, &CompileOptions::default()).unwrap();
                assert_eq!(compiled.sql.contains("WITH RECURSIVE"), !range.is_empty());
                assert_eq!(
                    compiled.return_vars,
                    vec![ReturnItem::Property(variable.into(), "id".into())]
                );
                let projection = format!("{alias} AS \"{variable}_id\"");
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
fn same_kind_reuse_keeps_existing_diagnostics() {
    for (input, message) in [
        (
            "MATCH (a)-[:extends]->(a) RETURN a.id",
            "repeated node variable 'a' (cycle / self-reachability requires alias-equality predicates not yet implemented)",
        ),
        (
            "MATCH (a)-[e:extends]->(b)-[e:extends]->(c) RETURN a.id",
            "repeated edge variable 'e' not supported",
        ),
    ] {
        for parsed in [parse(QueryLanguage::Gql, input), parse_auto(input)] {
            let query = parsed.unwrap();
            for error in [
                validate(&mut query.clone()).unwrap_err(),
                validate_with_warnings(&mut query.clone()).unwrap_err(),
                compile(&query, &CompileOptions::default()).unwrap_err(),
            ] {
                assert!(matches!(error, QueryError::Unsupported(ref text) if text == message), "{error:?}");
            }
        }
    }
}
