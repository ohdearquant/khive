#[tokio::test]
async fn test_query_entities_basic() {
    let store = setup_memory_store_ns("ns1");

    for name in &["Alpha", "Beta", "Gamma"] {
        store
            .upsert_entity(make_entity("ns1", "concept", name))
            .await
            .unwrap();
    }
    store
        .upsert_entity(make_entity("ns1", "document", "Paper1"))
        .await
        .unwrap();

    let page = store
        .query_entities(
            "ns1",
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.total, Some(4));

    // Filter by kind
    let concepts = store
        .query_entities(
            "ns1",
            EntityFilter {
                kinds: vec!["concept".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(concepts.items.len(), 3);
}

#[tokio::test]
async fn test_query_by_name_prefix() {
    let store = setup_memory_store_ns("ns1");

    // "Alpha" and "AlphaGo" both start with "Alpha"; "Beta" does not
    for &name in &["Alpha", "AlphaGo", "Beta"] {
        store
            .upsert_entity(make_entity("ns1", "concept", name))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("Alpha".to_string()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Alpha"), "Alpha not found in {names:?}");
    assert!(names.contains(&"AlphaGo"), "AlphaGo not found in {names:?}");
    assert!(!names.contains(&"Beta"));
}

#[tokio::test]
async fn test_query_by_name_prefix_escapes_underscore_wildcard() {
    let store = setup_memory_store_ns("ns1");

    // The real entity's name contains a literal underscore.
    store
        .upsert_entity(make_entity("ns1", "concept", "a_b"))
        .await
        .unwrap();

    // 150 decoys that would match the *unescaped* LIKE pattern "a_b%"
    // (`_` as a wildcard matches any single character) but must not match
    // once the wildcard is escaped. Created after "a_b" so a page ordered
    // by created_at DESC with LIMIT 100 would push "a_b" out unless exact
    // matches are also ranked first.
    for i in 0..150 {
        store
            .upsert_entity(make_entity("ns1", "concept", &format!("aXb-{i:03}")))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("a_b".to_string()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();

    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"a_b"),
        "exact match 'a_b' must survive escaping and page ordering despite 150 wildcard-matching decoys; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("aXb")),
        "escaped '_' must not match decoy names like 'aXb-000'; got {names:?}"
    );
}

#[tokio::test]
async fn test_query_by_name_prefix_escapes_percent_wildcard() {
    let store = setup_memory_store_ns("ns1");

    // The real entity's name contains a literal percent sign.
    store
        .upsert_entity(make_entity("ns1", "concept", "50%off"))
        .await
        .unwrap();

    // 150 decoys that would match the *unescaped* LIKE pattern "50%off%"
    // (`%` as a wildcard matches any sequence, including across "-") but
    // must not match once the wildcard is escaped.
    for i in 0..150 {
        store
            .upsert_entity(make_entity("ns1", "concept", &format!("50-off-{i:03}")))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("50%off".to_string()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();

    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"50%off"),
        "exact match '50%off' must survive escaping and page ordering despite 150 wildcard-matching decoys; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("50-off-")),
        "escaped '%' must not match decoy names like '50-off-000'; got {names:?}"
    );
}

// Regression for a gap on #818/#834: the two tests above prove
// escaping keeps decoys out of the WHERE clause entirely, so they pass even
// without the exact-match-first ORDER BY CASE. This test isolates that
// ordering: every decoy genuinely matches the LIKE pattern "Base%" (no
// wildcard characters, escaping plays no role) and is newer than the exact
// match, so a page ordered only by `created_at DESC` would fill entirely with
// decoys and strand "Base" past the LIMIT 100 boundary. It fails if the
// `ORDER BY CASE WHEN LOWER(name) = ... THEN 0 ELSE 1 END` clause is removed.
#[tokio::test]
async fn test_query_by_name_prefix_exact_match_ranked_before_many_matching_decoys() {
    let store = setup_memory_store_ns("ns1");

    store
        .upsert_entity(make_entity("ns1", "concept", "Base"))
        .await
        .unwrap();

    for i in 0..150 {
        store
            .upsert_entity(make_entity("ns1", "concept", &format!("Base-{i:03}")))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("Base".to_string()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();

    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"Base"),
        "exact match 'Base' must be ranked ahead of 150 newer, genuinely prefix-matching \
         decoys within a LIMIT 100 page; got {names:?}"
    );
}
