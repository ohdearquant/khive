use super::*;
use crate::parsers::gql;

#[test]
fn node_kind_passes_through_unchanged() {
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
    let mut q = gql::parse("MATCH (a:gizmo)-[:extends]->(b) RETURN a").unwrap();
    validate(&mut q).unwrap();
}

#[test]
fn rejects_depth_above_max() {
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
fn allows_explicit_json_property_named_namespace() {
    let mut q = gql::parse(
        "MATCH (a:concept)-[:extends]->(b) \
         WHERE a.properties.namespace = 'legacy' RETURN a",
    )
    .unwrap();
    validate(&mut q).unwrap();
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
fn where_relation_in_validates_every_list_member() {
    let mut valid =
        gql::parse("MATCH (a)-[e]->(b) WHERE e.relation IN ['extends', 'variant_of'] RETURN a")
            .unwrap();
    assert!(validate(&mut valid).is_ok());

    let mut invalid =
        gql::parse("MATCH (a)-[e]->(b) WHERE e.relation IN ['extends', 'not_real'] RETURN a")
            .unwrap();
    let err = validate(&mut invalid).unwrap_err();
    assert!(err.to_string().contains("not_real"), "msg: {err}");

    let mut non_string =
        gql::parse("MATCH (a)-[e]->(b) WHERE e.relation IN ['extends', 1] RETURN a").unwrap();
    let err = validate(&mut non_string).unwrap_err();
    assert!(err.to_string().contains("must be strings"), "msg: {err}");
}

#[test]
fn where_relation_contains_is_not_exact_relation_validation() {
    let mut q =
        gql::parse("MATCH (a)-[e]->(b) WHERE e.relation CONTAINS 'extend' RETURN a").unwrap();
    assert!(validate(&mut q).is_ok());
}

#[test]
fn query_edge_relation_bang_rejected() {
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
    let mut q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'gizmo' RETURN a").unwrap();
    validate(&mut q).unwrap();
    assert_eq!(first_condition_string_value(&q), "gizmo");
}

#[test]
fn kind_in_where_passes_through_unchanged() {
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
fn accepts_zero_hop_range_gql_wide() {
    let mut q = gql::parse("MATCH (a)-[:extends*0..3]->(b) RETURN b").unwrap();
    validate(&mut q).unwrap();
    let edge = q.pattern.edges().next().unwrap();
    assert_eq!((edge.min_hops, edge.max_hops), (0, 3));
    assert!(q.pattern.has_variable_length());
}

#[test]
fn accepts_zero_hop_range_gql_narrow_and_at_depth_cap() {
    for max in [0, 1, 10] {
        let mut q = gql::parse(&format!("MATCH (a)-[:extends*0..{max}]->(b) RETURN b")).unwrap();
        validate_with_warnings(&mut q).unwrap();
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!((edge.min_hops, edge.max_hops), (0, max));
        assert!(
            q.pattern.has_variable_length(),
            "0..{max} needs a zero seed"
        );
    }
}

#[test]
fn accepts_zero_hop_sparql_explicit_range() {
    use crate::parsers::sparql;
    for max in [0, 1, 3] {
        let mut q = sparql::parse(&format!(
            "SELECT ?a ?b WHERE {{ ?a :extends{{0,{max}}} ?b . }}"
        ))
        .unwrap();
        validate(&mut q).unwrap();
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!((edge.min_hops, edge.max_hops), (0, max));
        assert!(q.pattern.has_variable_length());
    }
}

#[test]
fn direct_ast_zero_hop_ranges_retain_depth_and_inversion_guards() {
    let with_range = |min_hops, max_hops| {
        let mut q = gql::parse("MATCH (a)-[:extends]->(b) RETURN b").unwrap();
        let crate::ast::PatternElement::Edge(edge) = &mut q.pattern.elements[1] else {
            panic!("expected parsed edge");
        };
        edge.min_hops = min_hops;
        edge.max_hops = max_hops;
        q
    };
    for (min, max) in [(0, 0), (0, 1), (0, 10), (1, 1)] {
        let mut q = with_range(min, max);
        validate(&mut q).unwrap();
        assert_eq!(q.pattern.has_variable_length(), (min, max) != (1, 1));
    }
    let err = validate(&mut with_range(0, 11)).unwrap_err();
    assert!(matches!(err, QueryError::InvalidInput(_)), "{err:?}");
    let err = validate(&mut with_range(1, 0)).unwrap_err();
    assert!(matches!(err, QueryError::Validation(_)), "{err:?}");
    for (range, inverted) in [("0..11", false), ("1..0", true)] {
        let mut q = gql::parse(&format!("MATCH (a)-[:extends*{range}]->(b) RETURN b")).unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(
                (&err, inverted),
                (QueryError::Validation(_), true) | (QueryError::InvalidInput(_), false)
            ),
            "{range}: {err:?}"
        );
    }
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
    let mut q = gql::parse("MATCH (a)-[:extends*3..1]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "expected Validation error, got {err:?}"
    );
}

#[test]
fn rejects_min_hops_above_depth_cap() {
    let mut q = gql::parse("MATCH (a)-[:extends*50..100]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn rejects_max_above_depth_cap_with_satisfiable_min() {
    let mut q = gql::parse("MATCH (a)-[:extends*2..50]->(b) RETURN b").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

#[test]
fn rejects_unknown_synthetic_relation() {
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

#[test]
fn validate_pattern_shape_rejects_even_element_count() {
    use crate::ast::{EdgeDirection, EdgePattern, PatternElement};
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
    let elements = vec![make_node(), make_node(), make_node()];
    let err = validate_pattern_shape(&elements).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "expected Validation error for Node at odd index, got {err:?}"
    );
    let elements2 = vec![make_edge(), make_node(), make_edge()];
    let err2 = validate_pattern_shape(&elements2).unwrap_err();
    assert!(
        matches!(err2, QueryError::Validation(_)),
        "expected Validation error for Edge at even index, got {err2:?}"
    );
    let elements3 = vec![make_node(), make_edge(), make_node()];
    validate_pattern_shape(&elements3).expect("Node, Edge, Node must be valid");
}

#[test]
fn node_property_named_relation_allowed() {
    let mut q =
        gql::parse("MATCH (a)-[:extends]->(b) WHERE a.properties.relation = 'external' RETURN a")
            .unwrap();
    validate(&mut q).unwrap();
    assert_eq!(first_condition_string_value(&q), "external");
}

#[test]
fn edge_relation_still_validated() {
    let mut q =
        gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'not_real' RETURN a").unwrap();
    let err = validate(&mut q).unwrap_err();
    assert!(err.to_string().contains("not_real"), "msg: {err}");
}

#[test]
fn not_exists_direct_ast_validates_inner_ranges_and_constraints() {
    use crate::ast::{PatternElement, WhereExpr};
    let base =
        || gql::parse("MATCH (r) WHERE NOT EXISTS { (f)-[:Supports]->(r) } RETURN r.id").unwrap();
    let mut normalized = base();
    validate(&mut normalized).unwrap();
    let WhereExpr::NotExists(pattern) = &normalized.where_clause else {
        panic!("absence")
    };
    assert_eq!(pattern.edges().next().unwrap().relations, ["supports"]);
    for (min, max) in [(0, 0), (0, 1), (1, 2), (2, 2), (1, 11), (2, 1)] {
        let mut query = base();
        let WhereExpr::NotExists(pattern) = &mut query.where_clause else {
            unreachable!()
        };
        let PatternElement::Edge(edge) = &mut pattern.elements[1] else {
            unreachable!()
        };
        edge.min_hops = min;
        edge.max_hops = max;
        assert!(validate(&mut query).is_err(), "inner range {min}..{max}");
    }
    for source in [
        "MATCH (r) WHERE NOT EXISTS { (f {namespace: 'foreign'})-[:supports]->(r) } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:not_a_relation]->(r) } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:observed_as_selected]->(r) } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:supports|observed_as_selected]->(r) } RETURN r",
    ] {
        let mut query = gql::parse(source).unwrap();
        assert!(
            validate(&mut query).is_err(),
            "must validate the scoped pattern: {source}"
        );
    }
    let mut empty = base();
    empty.where_clause = WhereExpr::NotExists(crate::ast::MatchPattern { elements: vec![] });
    assert!(crate::compile(&empty, &crate::CompileOptions::default()).is_err());
    let mut malformed = base();
    let WhereExpr::NotExists(pattern) = &mut malformed.where_clause else {
        unreachable!()
    };
    pattern.elements.pop();
    assert!(crate::compile(&malformed, &crate::CompileOptions::default()).is_err());
}
#[test]
fn mixed_segment_cumulative_maxima_validate_parsed_and_direct_ast() {
    for (second_max, accepted) in [(6, true), (7, false)] {
        let text = format!(
            "MATCH (a)-[:extends*1..4]->(b)-[:implements*1..{second_max}]->(c) RETURN c.id"
        );
        let parsed = gql::parse(&text).unwrap();
        let mut direct =
            gql::parse("MATCH (a)-[:extends]->(b)-[:implements]->(c) RETURN c.id").unwrap();
        for (index, max) in [(1, 4), (3, second_max)] {
            let PatternElement::Edge(edge) = &mut direct.pattern.elements[index] else {
                unreachable!()
            };
            edge.max_hops = max;
        }
        for mut query in [parsed, direct] {
            let result = validate(&mut query);
            if accepted {
                result.unwrap();
                crate::compile(&query, &crate::CompileOptions::default()).unwrap();
            } else {
                assert!(
                    matches!(result, Err(QueryError::InvalidInput(_))),
                    "{result:?}"
                );
                assert!(matches!(
                    crate::compile(&query, &crate::CompileOptions::default()),
                    Err(QueryError::InvalidInput(_))
                ));
            }
        }
    }
    for source in [
        "MATCH (a)-[:contains]->(b)-[:extends*1..10]->(c)-[:implements]->(d) RETURN d.id",
        "MATCH (a)-[:extends*0..0]->(b)-[:implements*1..10]->(c) RETURN c.id",
        "MATCH (a)-[:extends*0..0]->(b)-[:implements*0..0]->(c) RETURN c.id",
    ] {
        let mut query = gql::parse(source).unwrap();
        validate(&mut query).unwrap();
        crate::compile(&query, &crate::CompileOptions::default()).unwrap();
    }
    let mut outputs = Vec::new();
    for fixed in [":contains", ":contains*1", ":contains*1..1"] {
        let mut query = gql::parse(&format!(
            "MATCH (a)-[{fixed}]->(b)-[:extends*1..10]->(c) RETURN c.id"
        ))
        .unwrap();
        validate(&mut query).unwrap();
        let edge = query.pattern.edges().next().unwrap();
        assert_eq!((edge.min_hops, edge.max_hops), (1, 1));
        let result = crate::compile(&query, &crate::CompileOptions::default()).unwrap();
        outputs.push((result.sql, format!("{:?}", result.params)));
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0], outputs[2]);

    for (min, max) in [(2, 1), (1, 11)] {
        let mut query =
            gql::parse("MATCH (a)-[:extends]->(b)-[:implements]->(c) RETURN c.id").unwrap();
        let PatternElement::Edge(edge) = &mut query.pattern.elements[1] else {
            unreachable!()
        };
        edge.min_hops = min;
        edge.max_hops = max;
        let error = validate(&mut query).unwrap_err();
        if min > max {
            assert!(matches!(error, QueryError::Validation(_)), "{error:?}");
            assert!(error.to_string().contains("min 2 > max 1"));
        } else {
            assert!(matches!(error, QueryError::InvalidInput(_)), "{error:?}");
            assert!(error.to_string().contains("11"));
        }
    }
    let mut comma =
        gql::parse("MATCH (a)-[:extends*1..2]->(b), (b)-[:implements]->(c) RETURN c.id").unwrap();
    assert!(matches!(
        validate(&mut comma),
        Err(QueryError::Unsupported(_))
    ));
}
