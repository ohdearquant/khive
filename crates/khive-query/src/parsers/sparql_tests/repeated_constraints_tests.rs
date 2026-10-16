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
