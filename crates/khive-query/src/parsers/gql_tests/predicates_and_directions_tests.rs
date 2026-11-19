#[test]
fn where_clause() {
    let q = parse(
            "MATCH (a)-[e:implements]->(b:project) WHERE b.name = 'lattice-inference' RETURN a LIMIT 10"
        ).unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 1);
    assert_eq!(conds[0].variable, "b");
    assert_eq!(conds[0].property, PropertyRef::Field("name".into()));
}

#[test]
fn where_json_property_path_is_explicit_in_ast() {
    let q = parse("MATCH (n) WHERE n.properties.finding.severity = 'high' RETURN n").unwrap();
    let cond = q.where_clause.conditions().next().unwrap();
    assert_eq!(
        cond.property,
        PropertyRef::JsonPath(vec!["finding".into(), "severity".into()])
    );
}

#[test]
fn where_nested_path_rejects_non_properties_root() {
    let err = parse("MATCH (n) WHERE n.finding.severity = 'high' RETURN n").unwrap_err();
    assert!(
        err.to_string()
            .contains("invalid WHERE property path root 'finding'"),
        "got: {err}"
    );
}

#[test]
fn where_clause_and() {
    let q = parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' AND b.kind = 'concept' RETURN a, b"
        ).unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 2, "AND should produce two leaf conditions");
    assert!(
        matches!(&q.where_clause, WhereExpr::And(_, _)),
        "should be And node"
    );
}

#[test]
fn where_clause_or() {
    let q = parse(
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a",
    )
    .unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 2, "OR should produce two leaf conditions");
    assert!(
        matches!(&q.where_clause, WhereExpr::Or(_, _)),
        "should be Or node"
    );
}

#[test]
fn where_clause_and_or() {
    let q = parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a"
        ).unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 3);
    assert!(
        matches!(&q.where_clause, WhereExpr::Or(_, _)),
        "top-level should be Or"
    );
}

#[test]
fn where_clause_extended_operators() {
    let q = parse(
        "MATCH (n:entity) WHERE n.name CONTAINS '%_' AND n.name STARTS WITH 'pre' \
             AND n.kind IN ['concept', 'document'] AND n.properties.domain IS NOT NULL RETURN n",
    )
    .unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 4);
    assert_eq!(conds[0].op, CompareOp::Contains);
    assert_eq!(conds[0].value, ConditionValue::String("%_".into()));
    assert_eq!(conds[1].op, CompareOp::StartsWith);
    assert_eq!(conds[2].op, CompareOp::In);
    assert_eq!(
        conds[2].value,
        ConditionValue::List(vec![
            ConditionValue::String("concept".into()),
            ConditionValue::String("document".into()),
        ])
    );
    assert_eq!(conds[3].op, CompareOp::IsNotNull);
    assert_eq!(conds[3].value, ConditionValue::Null);
}

#[test]
fn where_clause_is_null() {
    let q = parse("MATCH (n:entity) WHERE n.properties.domain IS NULL RETURN n").unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 1);
    assert_eq!(conds[0].op, CompareOp::IsNull);
    assert_eq!(conds[0].value, ConditionValue::Null);
}

#[test]
fn where_clause_is_null_and_is_not_null_together() {
    let q =
        parse("MATCH (n:entity) WHERE n.name IS NOT NULL AND n.properties.domain IS NULL RETURN n")
            .unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(conds.len(), 2);
    assert_eq!(conds[0].op, CompareOp::IsNotNull);
    assert_eq!(conds[1].op, CompareOp::IsNull);
}

#[test]
fn where_in_accepts_scalar_list_and_empty_list() {
    let q = parse(
        "MATCH (n) WHERE n.properties.value IN ['x', 1, 2.5, true] \
             OR n.properties.value IN [] RETURN n",
    )
    .unwrap();
    let conds: Vec<_> = q.where_clause.conditions().collect();
    assert_eq!(
        conds[0].value,
        ConditionValue::List(vec![
            ConditionValue::String("x".into()),
            ConditionValue::Integer(1),
            ConditionValue::Number(2.5),
            ConditionValue::Bool(true),
        ])
    );
    assert_eq!(conds[1].value, ConditionValue::List(vec![]));
}

#[test]
fn contains_and_starts_with_require_strings() {
    let contains = parse("MATCH (n) WHERE n.name CONTAINS 1 RETURN n").unwrap_err();
    assert!(contains.to_string().contains("require a string literal"));

    let starts = parse("MATCH (n) WHERE n.name STARTS WITH true RETURN n").unwrap_err();
    assert!(starts.to_string().contains("require a string literal"));
}

#[test]
fn inbound_edge() {
    let q = parse("MATCH (a:paper)<-[e:introduced_by]-(b:concept) RETURN a, b").unwrap();
    let edges: Vec<_> = q.pattern.edges().collect();
    assert_eq!(edges[0].direction, EdgeDirection::In);
}

#[test]
fn undirected_edge() {
    let q = parse("MATCH (a)-[e:competes_with]-(b) RETURN a, b").unwrap();
    let edges: Vec<_> = q.pattern.edges().collect();
    assert_eq!(edges[0].direction, EdgeDirection::Both);
}

#[test]
fn three_node_chain() {
    let q = parse(
        "MATCH (a:concept)-[:introduced_by]->(p:paper)-[:introduced_by]->(c:concept) RETURN a, c",
    )
    .unwrap();
    assert_eq!(q.pattern.elements.len(), 5);
    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(nodes.len(), 3);
}

#[test]
fn not_exists_is_a_scoped_atom_with_existing_boolean_precedence() {
    let mut query = parse("MATCH (r:concept) WHERE r.name = 'always' OR NOT EXISTS { (f:concept {name: 'evidence'})-[e:supports]->(r) } AND r.name = 'eligible' RETURN r.id").unwrap();
    let WhereExpr::Or(left, right) = &query.where_clause else {
        panic!("OR must be outermost")
    };
    assert!(matches!(left.as_ref(), WhereExpr::Condition(_)));
    let WhereExpr::And(absence, condition) = right.as_ref() else {
        panic!("AND must bind first")
    };
    let WhereExpr::NotExists(pattern) = absence.as_ref() else {
        panic!("expected scoped absence")
    };
    assert_eq!(pattern.elements.len(), 3);
    assert_eq!(
        pattern
            .nodes()
            .map(|n| n.variable.as_deref())
            .collect::<Vec<_>>(),
        [Some("f"), Some("r")]
    );
    assert_eq!(pattern.edges().next().unwrap().relations, ["supports"]);
    assert!(matches!(condition.as_ref(), WhereExpr::Condition(_)));
    assert_eq!(
        query
            .where_clause
            .conditions()
            .map(|c| c.variable.as_str())
            .collect::<Vec<_>>(),
        ["r", "r"]
    );
    let mut visited = 0;
    query
        .where_clause
        .for_each_condition_mut(&mut |_| visited += 1);
    assert_eq!(
        visited, 2,
        "block-local inline constraints are not outer scalar conditions"
    );
    let singleton =
        parse("MATCH (r) WHERE NOT EXISTS { (r:concept {name: 'present'}) } RETURN r.id").unwrap();
    assert!(matches!(singleton.where_clause, WhereExpr::NotExists(ref p) if p.elements.len() == 1));
    for variable in ["not", "exists", "count"] {
        let query = parse(&format!(
            "MATCH ({variable}) WHERE {variable}.name = 'literal' RETURN {variable}.id"
        ))
        .unwrap();
        assert_eq!(
            query.where_clause.conditions().next().unwrap().variable,
            variable
        );
    }
}

#[test]
fn not_exists_refuses_query_clauses_and_malformed_blocks() {
    for source in [
        "MATCH (r) WHERE NOT EXISTS { } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (r) RETURN r",
        "MATCH (r) WHERE NOT EXISTS { MATCH (r) } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (r) WHERE r.name = 'x' } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (r) RETURN r } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (r) ORDER BY r.id } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { (r), (s) } RETURN r",
        "MATCH (r) WHERE NOT EXISTS { NOT EXISTS { (r) } } RETURN r",
        "MATCH (r) OPTIONAL MATCH (f)-[:supports]->(r) RETURN r",
    ] {
        assert!(parse(source).is_err(), "must refuse: {source}");
    }
}
