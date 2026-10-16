fn assert_absent(response: &Value, ids: &[Uuid], text: &[&str]) {
    let rendered = response.to_string();
    for id in ids {
        assert!(
            !rendered.contains(&id.to_string()),
            "unexpected record {id}: {response}"
        );
    }
    for value in text {
        assert!(
            !rendered.contains(value),
            "unexpected message text: {response}"
        );
    }
}

fn neighbor_rows(response: &Value) -> &[Value] {
    response
        .as_array()
        .or_else(|| response["neighbors"].as_array())
        .expect("neighbor array")
}

async fn reader_checkouts(fixture: &Fixture) -> u64 {
    let mut total = 0;
    let mut seen = HashSet::new();
    for runtime in [&fixture.main, &fixture.comm] {
        if !seen.insert(runtime.backend_id().clone()) {
            continue;
        }
        total += runtime
            .db_diagnostics()
            .await
            .expect("reader diagnostics")
            .reader_contention
            .pooled_reader_checkouts;
    }
    total
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ordinary_summary_reuses_graph_reads_on_shared_and_split_backends() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let token = f.main.authorize(Namespace::local()).expect("graph token");
        let anchor = Entity::new("local", "concept", "Summary read budget anchor");
        f.main
            .entities(&token)
            .expect("entity store")
            .upsert_entity(anchor.clone())
            .await
            .expect("store anchor");
        for index in 0..3 {
            let neighbor = Entity::new("local", "concept", format!("Summary neighbor {index}"));
            f.main
                .entities(&token)
                .expect("entity store")
                .upsert_entity(neighbor.clone())
                .await
                .expect("store neighbor");
            store_edge(
                &f.main,
                "local",
                Uuid::new_v4(),
                anchor.id,
                neighbor.id,
                EdgeRelation::Supports,
                1.0,
            )
            .await;
        }
        let note = Note::new("local", "observation", "Summary ordinary note neighbor");
        f.main
            .notes(&token)
            .expect("note store")
            .upsert_note(note.clone())
            .await
            .expect("store ordinary note");
        store_edge(
            &f.main,
            "local",
            Uuid::new_v4(),
            note.id,
            anchor.id,
            EdgeRelation::Annotates,
            2.0,
        )
        .await;
        // Compare identical caller visibility; dispatch also admits the named actor's namespace.
        let observer = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: Some("observer".into()),
            ..f.main.config().clone()
        })
        .expect("in-memory observer authorization");
        let observer_namespace = Namespace::parse("observer").expect("observer namespace");
        let query_token = observer
            .authorize_with_visibility(Namespace::local(), vec![observer_namespace.clone()])
            .expect("observer graph token");
        assert_eq!(query_token.actor().id, "observer");
        assert_eq!(
            query_token.visible_namespaces(),
            &[Namespace::local(), observer_namespace]
        );
        reader_checkouts(&f).await;
        let before_query = reader_checkouts(&f).await;
        let hits = f
            .main
            .neighbors_with_query_page(
                &query_token,
                anchor.id,
                NeighborQuery {
                    direction: Direction::Both,
                    relations: None,
                    limit: Some(3),
                    min_weight: None,
                },
                None,
                None,
                true,
            )
            .await
            .expect("ordinary adjacency query");
        let query_cost = reader_checkouts(&f).await - before_query;
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].kind.as_deref(), Some("observation"));

        let before_summary = reader_checkouts(&f).await;
        let response = read(
            &f.registry,
            "observer",
            "neighbors",
            json!({"node_id": anchor.id, "direction": "both", "limit": 2, "projection": "summary"}),
        )
        .await;
        let summary_cost = reader_checkouts(&f).await - before_summary;
        assert_eq!(neighbor_rows(&response).len(), 2, "{response}");
        assert_eq!(neighbor_rows(&response)[0]["id"], json!(note.id));
        assert!(response["next_after"].is_string(), "{response}");
        assert_eq!(
            summary_cost, query_cost,
            "ordinary Summary must reuse the existing graph reads"
        );
    }
}

#[tokio::test]
async fn message_live_origin_scope_is_checked_after_other_backend_tombstone() {
    let f = fixture(Topology::Split).await;
    let token = f.main.authorize(Namespace::local()).expect("entity token");
    let mut tombstone = Entity::new("local", "concept", "Former graph origin");
    tombstone.id = f.outbound.id;
    tombstone.deleted_at = Some(Utc::now().timestamp_micros());
    f.main
        .entities(&token)
        .expect("entity store")
        .upsert_entity(tombstone)
        .await
        .expect("store former origin");

    let response = read(
        &f.registry,
        "observer",
        "neighbors",
        json!({"node_id": f.outbound.id, "direction": "both", "projection": "summary"}),
    )
    .await;
    assert!(neighbor_rows(&response).is_empty(), "{response}");
    assert_absent(&response, &f.edges, &[&f.outbound.content]);

    let own = read(
        &f.registry,
        "sender",
        "neighbors",
        json!({"node_id": f.outbound.id, "direction": "both", "projection": "summary"}),
    )
    .await;
    assert_eq!(neighbor_rows(&own).len(), 2, "{own}");
}

#[tokio::test]
async fn message_graph_scope_retains_tombstone_endpoint_checks_on_both_backends() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let token = f.comm.authorize(Namespace::local()).expect("note token");
        for mut note in [f.outbound.clone(), f.inbound.clone()] {
            note.deleted_at = Some(Utc::now().timestamp_micros());
            f.comm
                .notes(&token)
                .expect("note store")
                .upsert_note(note)
                .await
                .expect("store message tombstone");
        }
        let mut hidden_ids = f.edges.clone();
        hidden_ids.extend([f.outbound.id, f.inbound.id]);
        for origin in [f.anchor, f.outbound.id, f.inbound.id] {
            for projection in ["record", "summary", "edge"] {
                let response = read(
                    &f.registry,
                    "observer",
                    "neighbors",
                    json!({"node_id": origin, "projection": projection, "limit": 1}),
                )
                .await;
                assert!(neighbor_rows(&response).is_empty(), "{response}");
                assert!(response["next_after"].is_null(), "{response}");
                assert_absent(&response, &hidden_ids, &[&f.outbound.content]);
            }
        }
        let context = read(
            &f.registry,
            "observer",
            "context",
            json!({"entity_ids": [f.anchor], "hops": 2, "direction": "both"}),
        )
        .await;
        assert!(context["anchors"][0]["neighbors"]
            .as_array()
            .expect("context neighbors")
            .is_empty());
        assert_absent(&context, &hidden_ids, &[&f.outbound.content]);
    }
}

#[tokio::test]
async fn message_neighbors_scope_on_shared_and_split_backends() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        for projection in ["record", "summary", "edge"] {
            for limit in [None, Some(1)] {
                for origin in [f.anchor, f.outbound.id, f.inbound.id] {
                    let mut args = json!({
                        "node_id": origin,
                        "direction": "both",
                        "projection": projection,
                    });
                    if let Some(limit) = limit {
                        args["limit"] = json!(limit);
                    }
                    let response = read(&f.registry, "observer", "neighbors", args).await;
                    assert!(neighbor_rows(&response).is_empty(), "{response}");
                    assert!(response["next_after"].is_null(), "{response}");
                    let mut ids = f.edges.clone();
                    ids.extend([
                        f.outbound.id,
                        f.inbound.id,
                        f.outbound_target,
                        f.inbound_target,
                    ]);
                    assert_absent(
                        &response,
                        &ids,
                        &[&f.outbound.content, "Graph mailbox example"],
                    );
                }
            }
            for (actor, own, other) in [
                ("sender", f.outbound.id, f.inbound.id),
                ("recipient", f.inbound.id, f.outbound.id),
            ] {
                let response = read(
                    &f.registry,
                    actor,
                    "neighbors",
                    json!({
                        "node_id": f.anchor,
                        "direction": "both",
                        "projection": projection,
                        "limit": 1,
                    }),
                )
                .await;
                assert_eq!(neighbor_rows(&response).len(), 1, "{response}");
                assert!(
                    response.to_string().contains(&own.to_string()),
                    "{response}"
                );
                assert_absent(&response, &[other], &[]);
                assert!(response["next_after"].is_null(), "{response}");
                let from_message = read(
                    &f.registry,
                    actor,
                    "neighbors",
                    json!({"node_id": own, "direction": "both", "projection": projection}),
                )
                .await;
                assert_eq!(neighbor_rows(&from_message).len(), 2, "{from_message}");
                assert_absent(&from_message, &[other], &[]);
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn message_context_scope_on_shared_and_split_backends() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        for hops in [1, 2] {
            let response = read(
                &f.registry,
                "observer",
                "context",
                json!({"entity_ids": [f.anchor], "hops": hops, "direction": "both"}),
            )
            .await;
            let anchors = response["anchors"].as_array().expect("anchor array");
            assert_eq!(anchors.len(), 1, "{response}");
            assert!(anchors[0]["neighbors"]
                .as_array()
                .expect("neighbor array")
                .is_empty());
            let mut ids = f.edges.clone();
            ids.extend([
                f.outbound.id,
                f.inbound.id,
                f.outbound_target,
                f.inbound_target,
            ]);
            assert_absent(
                &response,
                &ids,
                &[&f.outbound.content, "Graph mailbox example"],
            );
        }
        for (actor, own, own_target, other, other_target) in [
            (
                "sender",
                f.outbound.id,
                f.outbound_target,
                f.inbound.id,
                f.inbound_target,
            ),
            (
                "recipient",
                f.inbound.id,
                f.inbound_target,
                f.outbound.id,
                f.outbound_target,
            ),
        ] {
            let first = read(
                &f.registry,
                actor,
                "context",
                json!({"entity_ids": [f.anchor], "hops": 1, "fanout": 1, "direction": "both"}),
            )
            .await;
            let rows = first["anchors"][0]["neighbors"]
                .as_array()
                .expect("neighbor array");
            assert_eq!(rows.len(), 1, "{first}");
            assert_eq!(rows[0]["id"], json!(own), "{first}");
            assert_eq!(rows[0]["kind"], json!("message"), "{first}");
            assert_eq!(rows[0]["description"], json!(f.outbound.content), "{first}");
            assert_absent(&first, &[other, other_target], &[]);

            let second = read(
                &f.registry,
                actor,
                "context",
                json!({"entity_ids": [f.anchor], "hops": 2, "direction": "both"}),
            )
            .await;
            let rows = second["anchors"][0]["neighbors"]
                .as_array()
                .expect("neighbor array");
            assert_eq!(rows.len(), 2, "{second}");
            let target = rows
                .iter()
                .find(|row| row["id"] == json!(own_target))
                .expect("second-hop target");
            assert_eq!(target["hop"], json!(2), "{second}");
            assert_eq!(target["via"], json!(own), "{second}");
            assert_absent(&second, &[other, other_target], &[]);
        }
    }
}
