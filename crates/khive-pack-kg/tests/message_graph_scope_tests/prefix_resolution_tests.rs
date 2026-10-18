#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn neighbors_and_context_answer_an_unreadable_message_prefix_like_no_match() {
    let f = fixture(Topology::Shared).await;
    let message = store_message(&f, "cafe2001-0000-4000-8000-000000000001").await;
    link(&f, message, f.anchor, EdgeRelation::Annotates).await;
    assert_prefix_reads_as_no_match(&f, &["neighbors", "context"]).await;

    // A full id keeps the answer it has always had.
    let neighbors = read(
        &f.registry,
        "observer",
        "neighbors",
        json!({"node_id": message}),
    )
    .await;
    assert!(neighbor_rows(&neighbors).is_empty(), "{neighbors}");
    let error = try_read(
        &f.registry,
        "observer",
        "context",
        json!({"entity_ids": [message]}),
    )
    .await
    .expect_err("a message is not an entity");
    assert!(matches!(error, RuntimeError::NotFound(_)), "{error}");

    let owner = read(
        &f.registry,
        "recipient",
        "neighbors",
        json!({"node_id": "cafe2001"}),
    )
    .await;
    assert_eq!(neighbor_rows(&owner).len(), 1, "{owner}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ambiguous_prefix_leaves_out_unreadable_messages() {
    let f = fixture(Topology::Shared).await;
    let message = store_message(&f, "cafe4001-0000-4000-8000-000000000001").await;
    let record = store_concept(&f, "cafe4001-1000-4000-8000-000000000002").await;
    let edge = link(&f, record, f.anchor, EdgeRelation::Extends).await;

    let neighbors = read(
        &f.registry,
        "observer",
        "neighbors",
        json!({"node_id": "cafe4001"}),
    )
    .await;
    assert_eq!(neighbor_rows(&neighbors).len(), 1, "{neighbors}");
    assert_eq!(neighbor_rows(&neighbors)[0]["id"], json!(f.anchor));
    assert_absent(&neighbors, &[message], &[]);
    let context = read(
        &f.registry,
        "observer",
        "context",
        json!({"entity_ids": ["cafe4001"], "hops": 0}),
    )
    .await;
    assert_eq!(context["anchors"][0]["entity"]["id"], json!(record));
    assert_absent(&context, &[message], &[]);
    let listed = read(
        &f.registry,
        "observer",
        "list",
        json!({"kind": "edge", "source_id": "cafe4001"}),
    )
    .await;
    assert_eq!(edge_ids(&listed), [edge], "{listed}");

    // Someone who can read both records still sees the prefix as ambiguous.
    let error = try_read(
        &f.registry,
        "recipient",
        "neighbors",
        json!({"node_id": "cafe4001"}),
    )
    .await
    .expect_err("two readable records share the prefix");
    assert!(
        matches!(&error, RuntimeError::AmbiguousPrefix { matches, .. }
            if matches.contains(&message) && matches.contains(&record)),
        "{error}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ambiguous_prefix_beyond_the_resolver_sample_counts_only_readable_records() {
    let f = fixture(Topology::Shared).await;
    // The prefix resolver reads entities, then notes, and stops once two ids
    // match. Its sample is therefore the concept and the two messages, whatever
    // order each table returns its rows in. The edge that also carries the prefix
    // is beyond the sample, so the sample alone shows one readable record.
    let record = store_concept(&f, "cafe3001-1000-4000-8000-000000000002").await;
    let first = store_message(&f, "cafe3001-0000-4000-8000-000000000001").await;
    let second = store_message(&f, "cafe3001-3000-4000-8000-000000000003").await;
    let edge = Uuid::parse_str("cafe3001-2000-4000-8000-000000000004").expect("edge UUID");
    store_edge(
        &f.main,
        "local",
        edge,
        f.anchor,
        f.outbound_target,
        EdgeRelation::Extends,
        1.0,
    )
    .await;
    let readable = sorted(vec![record, edge]);
    for (verb, args) in [
        ("neighbors", json!({"node_id": "cafe3001"})),
        ("list", json!({"kind": "edge", "source_id": "cafe3001"})),
    ] {
        let error = try_read(&f.registry, "observer", verb, args.clone())
            .await
            .expect_err("two readable records share the prefix");
        assert!(
            matches!(&error, RuntimeError::AmbiguousPrefix { matches, .. }
                if sorted(matches.clone()) == readable),
            "{verb}: {error}"
        );
        for message in [first, second] {
            assert!(
                !error.to_string().contains(&message.to_string()),
                "{verb}: {error}"
            );
        }
        // Someone who can read both messages gets the resolver's own sample.
        let owner = try_read(&f.registry, "recipient", verb, args)
            .await
            .expect_err("the concept and both messages share the prefix");
        assert!(
            matches!(&owner, RuntimeError::AmbiguousPrefix { matches, .. }
                if matches.contains(&first) && matches.contains(&second)),
            "{verb}: {owner}"
        );
    }
}
