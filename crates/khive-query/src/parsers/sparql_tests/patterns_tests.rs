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
fn explicit_zero_ranges_preserve_bounds_without_admitting_bare_star() {
    for max in [0, 1, 3] {
        let q = parse(&format!(
            "SELECT ?a ?b WHERE {{ ?a :extends{{0,{max}}} ?b . }}"
        ))
        .unwrap();
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!((edge.min_hops, edge.max_hops), (0, max));
        assert!(q.pattern.has_variable_length());
    }
    assert!(matches!(
        parse("SELECT ?a ?b WHERE { ?a :extends* ?b . }"),
        Err(QueryError::Unsupported(_))
    ));
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
