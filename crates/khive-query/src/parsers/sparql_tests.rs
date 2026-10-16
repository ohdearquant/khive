use super::*;

#[test]
fn simple_two_node() {
    let q = parse("SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10").unwrap();
    assert_eq!(
        q.return_items,
        vec![
            ReturnItem::Variable("a".into()),
            ReturnItem::Variable("b".into())
        ]
    );
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, Some(10));

    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(nodes[0].kind.as_deref(), Some("concept"));
    assert_eq!(nodes[0].variable.as_deref(), Some("a"));
}

#[test]
fn variable_length_plus() {
    let q = parse("SELECT ?b WHERE { ?a :name 'LoRA' . ?a :extends+ ?b . }").unwrap();
    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(
        nodes[0].properties.get("name").unwrap(),
        &ConditionValue::String("LoRA".into())
    );

    let edges: Vec<_> = q.pattern.edges().collect();
    assert_eq!(edges[0].min_hops, 1);
    assert_eq!(edges[0].max_hops, 5);
}

#[test]
fn explicit_range() {
    let q = parse("SELECT ?a ?b WHERE { ?a :extends{1,3} ?b . }").unwrap();
    let edges: Vec<_> = q.pattern.edges().collect();
    assert_eq!(edges[0].min_hops, 1);
    assert_eq!(edges[0].max_hops, 3);
}

#[test]
fn three_node_chain() {
    let q = parse("SELECT ?a ?c WHERE { ?a :extends ?b . ?b :introduced_by ?c . ?c a :paper . }")
        .unwrap();
    assert_eq!(q.pattern.elements.len(), 5); // 3 nodes + 2 edges
    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[2].kind.as_deref(), Some("paper"));
}

#[test]
fn property_filter() {
    let q = parse("SELECT ?a WHERE { ?a a :concept . ?a :domain 'attention' . ?a :extends+ ?b . }")
        .unwrap();
    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(
        nodes[0].properties.get("domain").unwrap(),
        &ConditionValue::String("attention".into())
    );
}

#[test]
fn conflicting_repeated_kind_constraint_is_rejected() {
    let err =
        parse("SELECT ?a WHERE { ?a a :concept . ?a a :person . ?a :extends ?b . }").unwrap_err();
    assert!(matches!(err, QueryError::Parse { .. }), "{err:?}");
    assert!(err.to_string().contains("conflicting kind"), "{err}");
}

#[test]
fn substrate_and_granular_kinds_constrain_the_same_node_in_both_orders() {
    for kinds in [":entity . ?a a :concept", ":concept . ?a a :entity"] {
        let query = parse(&format!(
            "SELECT ?a WHERE {{ ?a a {kinds} . ?a :extends ?b . }}"
        ))
        .unwrap();
        let start = query.pattern.nodes().next().unwrap();
        assert_eq!(start.kind.as_deref(), Some("entity"));
        let conditions: Vec<_> = query.where_clause.conditions().collect();
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].variable, "a");
        assert_eq!(conditions[0].property, PropertyRef::Field("kind".into()));
        assert_eq!(
            conditions[0].value,
            ConditionValue::String("concept".into())
        );
    }
}

#[test]
fn conflicting_substrate_kinds_are_rejected_in_both_orders() {
    for kinds in [":entity . ?a a :note", ":note . ?a a :entity"] {
        let err = parse(&format!(
            "SELECT ?a WHERE {{ ?a a {kinds} . ?a :extends ?b . }}"
        ))
        .unwrap_err();
        assert!(matches!(err, QueryError::Parse { .. }), "{err:?}");
        assert!(err.to_string().contains("conflicting kind"), "{err}");
    }
}

#[test]
fn conflicting_repeated_string_or_kind_literal_is_rejected() {
    for input in [
        "SELECT ?a WHERE { ?a :name 'x' . ?a :name 'y' . ?a :extends ?b . }",
        "SELECT ?a WHERE { ?a :domain :attention . ?a :domain 'vision' . ?a :extends ?b . }",
    ] {
        let err = parse(input).unwrap_err();
        assert!(matches!(err, QueryError::Parse { .. }), "{err:?}");
        assert!(err.to_string().contains("conflicting constraints"), "{err}");
    }
}

#[test]
fn identical_repeated_kind_and_string_constraints_are_idempotent() {
    let query = parse(
        "SELECT ?a WHERE { ?a a :concept . ?a a :concept . \
             ?a :name 'x' . ?a :name 'x' . ?a :extends ?b . }",
    )
    .expect("identical repeated triples remain valid");
    let start = query.pattern.nodes().next().expect("start node");
    assert_eq!(start.kind.as_deref(), Some("concept"));
    assert_eq!(
        start.properties.get("name"),
        Some(&ConditionValue::String("x".into()))
    );
}

#[test]
fn numeric_property_filter_is_explicit_json_path_in_ast() {
    let q = parse("SELECT ?a WHERE { ?a :score 1.5 . ?a :extends ?b . }").unwrap();
    let cond = q.where_clause.conditions().next().unwrap();
    assert_eq!(cond.property, PropertyRef::JsonPath(vec!["score".into()]));
}

#[test]
fn repeated_numeric_property_values_remain_conjunctive() {
    let query = parse("SELECT ?a WHERE { ?a :score 1 . ?a :score 2 . ?a :extends ?b . }")
        .expect("distinct numeric triples are valid conjuncts");
    assert!(matches!(&query.where_clause, WhereExpr::And(_, _)));
    let conditions: Vec<_> = query.where_clause.conditions().collect();
    assert_eq!(
        conditions.len(),
        2,
        "neither numeric triple may be discarded"
    );
    for (condition, value) in conditions.into_iter().zip([1, 2]) {
        assert_eq!(condition.variable, "a");
        assert_eq!(
            condition.property,
            PropertyRef::JsonPath(vec!["score".into()])
        );
        assert_eq!(condition.op, CompareOp::Eq);
        assert_eq!(condition.value, ConditionValue::Integer(value));
    }
    let compiled =
        crate::compile(&query, &crate::CompileOptions::default()).expect("both conjuncts compile");
    assert_eq!(
        compiled
            .sql
            .matches("json_extract(n0.properties, '$.score') = ?")
            .count(),
        2,
        "SQL must test the scalar JSON property against both values"
    );
}

#[test]
fn disconnected_triples_rejected() {
    let err = parse("SELECT ?a ?d WHERE { ?a :extends ?b . ?c :implements ?d . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn branched_triples_rejected() {
    let err = parse("SELECT ?a ?b ?c WHERE { ?a :extends ?b . ?a :implements ?c . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn disconnected_kind_constraint_rejected() {
    let err = parse("SELECT ?a WHERE { ?a :extends ?b . ?c a :concept . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported for disconnected kind constraint, got {err:?}"
    );
}

#[test]
fn disconnected_property_constraint_rejected() {
    let err = parse("SELECT ?a WHERE { ?a :extends ?b . ?c :name 'LoRA' . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported for disconnected property constraint, got {err:?}"
    );
}

#[test]
fn rejects_trailing_input_after_limit() {
    let err =
        parse("SELECT ?a WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10 GARBAGE").unwrap_err();
    assert!(
        err.to_string().contains("unexpected trailing input"),
        "expected trailing-input parse error, got {err}"
    );
}

#[test]
fn sparql_offset_remains_outside_the_dialect() {
    let err = parse("SELECT ?a WHERE { ?a :extends ?b . } LIMIT 10 OFFSET 10").unwrap_err();
    assert!(
        err.to_string().contains("unexpected trailing input"),
        "SPARQL OFFSET must not leak in through the shared AST, got {err}"
    );
}

#[test]
fn rejects_trailing_input_without_limit() {
    let err =
        parse("SELECT ?a WHERE { ?a a :concept . ?a :extends ?b . } and then some").unwrap_err();
    assert!(
        err.to_string().contains("unexpected trailing input"),
        "expected trailing-input parse error, got {err}"
    );
}

#[test]
fn sparql_insert_data_rejected_with_readonly_message() {
    let err = parse("INSERT DATA { <a> :extends <b> }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "INSERT DATA must return Unsupported; got {err:?}"
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
fn sparql_delete_data_rejected_with_readonly_message() {
    let err = parse("DELETE DATA { <a> :extends <b> }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "DELETE DATA must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn sparql_delete_where_rejected_with_readonly_message() {
    let err = parse("DELETE WHERE { ?s :extends ?o }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "DELETE WHERE must return Unsupported; got {err:?}"
    );
}

#[test]
fn sparql_load_rejected_with_readonly_message() {
    let err = parse("LOAD <http://example.org/graph>").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "LOAD must return Unsupported; got {err:?}"
    );
}

#[test]
fn sparql_select_still_compiles_after_write_guard() {
    let q = parse("SELECT ?a WHERE { ?a :extends ?b . }").unwrap();
    assert!(!q.pattern.elements.is_empty(), "valid SELECT must parse");
}

#[test]
fn sparql_with_delete_where_rejected() {
    let err = parse("WITH <http://g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "WITH … DELETE … must return Unsupported; got {err:?}"
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
fn sparql_prefixed_insert_data_rejected() {
    let err = parse("PREFIX ex: <http://example.org/> INSERT DATA { ex:a ex:b ex:c }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "prefixed INSERT DATA must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn sparql_clear_graph_rejected() {
    let err = parse("CLEAR GRAPH <http://example.org/graph>").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "CLEAR GRAPH must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn sparql_prefixed_select_write_guard_passes_through() {
    let err = parse("PREFIX ex: <http://example.org/> SELECT ?s WHERE { ?s ?p ?o }").unwrap_err();
    assert!(
        !matches!(err, QueryError::Unsupported(_)),
        "prefixed SELECT must not be rejected by the write guard; got {err:?}"
    );
}
