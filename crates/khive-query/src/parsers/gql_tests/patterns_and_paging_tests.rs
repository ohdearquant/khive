#[test]
fn simple_two_node_pattern() {
    let q = parse("MATCH (a:concept)-[e:introduced_by]->(b:paper) RETURN a, e, b").unwrap();
    assert_eq!(q.pattern.elements.len(), 3);
    assert_eq!(
        q.return_items,
        vec![
            ReturnItem::Variable("a".into()),
            ReturnItem::Variable("e".into()),
            ReturnItem::Variable("b".into()),
        ]
    );

    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(nodes[0].kind.as_deref(), Some("concept"));
    assert_eq!(nodes[1].kind.as_deref(), Some("paper"));

    let edges: Vec<_> = q.pattern.edges().collect();
    assert_eq!(edges[0].relations, vec!["introduced_by"]);
    assert_eq!(edges[0].direction, EdgeDirection::Out);
}

#[test]
fn variable_length_with_multiple_relations() {
    let q = parse("MATCH (a {name: 'LoRA'})-[:extends|variant_of*1..3]->(b) RETURN b LIMIT 20")
        .unwrap();
    let nodes: Vec<_> = q.pattern.nodes().collect();
    assert_eq!(
        nodes[0].properties.get("name").unwrap(),
        &ConditionValue::String("LoRA".into())
    );

    let edges: Vec<_> = q.pattern.edges().collect();
    assert_eq!(edges[0].relations, vec!["extends", "variant_of"]);
    assert_eq!(edges[0].min_hops, 1);
    assert_eq!(edges[0].max_hops, 3);
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, Some(20));
}

#[test]
fn skip_and_limit_parse_as_page_window() {
    let q = parse("MATCH (a:concept) RETURN a SKIP 500 LIMIT 200").unwrap();
    assert_eq!(q.offset, 500);
    assert_eq!(q.limit, Some(200));
}

#[test]
fn skip_without_limit_is_supported() {
    let q = parse("MATCH (a:concept) RETURN a SKIP 500").unwrap();
    assert_eq!(q.offset, 500);
    assert_eq!(q.limit, None);
}

#[test]
fn negative_skip_is_rejected() {
    let err = parse("MATCH (a:concept) RETURN a SKIP -1").unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "negative SKIP must be a parse error, got {err:?}"
    );
}

#[test]
fn skip_after_limit_is_rejected() {
    let err = parse("MATCH (a:concept) RETURN a LIMIT 20 SKIP 10").unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "SKIP must precede LIMIT, got {err:?}"
    );
}

#[test]
fn sql_offset_keyword_remains_outside_gql() {
    let err = parse("MATCH (a:concept) RETURN a OFFSET 10").unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "GQL paging uses SKIP rather than OFFSET, got {err:?}"
    );
}

#[test]
fn distinct_and_binding_counts_parse_without_reserving_count_names() {
    let q = parse(
        "MATCH (n)-[e:extends]->(m) RETURN DISTINCT n.name, count(n), count(e) SKIP 2 LIMIT 3",
    )
    .unwrap();
    assert!(q.distinct);
    assert_eq!(
        q.return_items,
        vec![
            ReturnItem::Property("n".into(), "name".into()),
            ReturnItem::Count("n".into()),
            ReturnItem::Count("e".into())
        ]
    );
    assert_eq!((q.offset, q.limit), (2, Some(3)));
    for source in [
        "MATCH (count) RETURN count",
        "MATCH (count) RETURN count.id",
        "MATCH (count) RETURN count(count)",
    ] {
        let q = parse(source).unwrap();
        assert!(!q.distinct);
        assert_eq!(q.return_items[0].variable(), "count");
    }
}

#[test]
fn unsupported_aggregate_and_alias_grammar_remains_refused() {
    for tail in [
        "count(*)",
        "count(DISTINCT n)",
        "count(n.name)",
        "count()",
        "count(n, n)",
        "sum(n)",
        "avg(n)",
        "count(n) AS total",
        "n.name AS label",
        "count(n) ORDER BY count(n)",
        "count(n) ORDER BY count_n",
        "DISTINCT",
    ] {
        let source = format!("MATCH (n) RETURN {tail}");
        assert!(parse(&source).is_err(), "{source}");
    }
}
