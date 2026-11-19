#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn context_keeps_fanout_per_visible_namespace() {
    let f = fixture(Topology::Shared).await;
    let sent = read(
        &f.registry,
        "sender",
        "comm.send",
        json!({
            "namespace": "project",
            "to": "recipient",
            "subject": "Project graph mailbox example",
            "content": "A message for the second visible graph namespace.",
            "idempotency_key": "project-graph-mailbox-example",
        }),
    )
    .await;
    let project_message =
        Uuid::parse_str(sent["full_id"].as_str().expect("outbound id")).expect("outbound UUID");
    let token = f.main.authorize(Namespace::local()).expect("entity token");
    let entity_anchor = Entity::new("local", "concept", "Two-namespace entity anchor");
    let message_anchor = Entity::new("local", "concept", "Two-namespace message anchor");
    let local_neighbor = Entity::new("local", "concept", "Local graph neighbor");
    let project_neighbor = Entity::new("project", "concept", "Project graph neighbor");
    for entity in [
        &entity_anchor,
        &message_anchor,
        &local_neighbor,
        &project_neighbor,
    ] {
        f.main
            .entities(&token)
            .expect("entity store")
            .upsert_entity(entity.clone())
            .await
            .expect("store graph entity");
    }
    for (namespace, source, target, relation) in [
        (
            "local",
            local_neighbor.id,
            entity_anchor.id,
            EdgeRelation::Extends,
        ),
        (
            "project",
            project_neighbor.id,
            entity_anchor.id,
            EdgeRelation::Extends,
        ),
        (
            "local",
            f.outbound.id,
            message_anchor.id,
            EdgeRelation::Annotates,
        ),
        (
            "project",
            project_message,
            message_anchor.id,
            EdgeRelation::Annotates,
        ),
    ] {
        store_edge(
            &f.main,
            namespace,
            Uuid::new_v4(),
            source,
            target,
            relation,
            1.0,
        )
        .await;
    }
    for direction in ["in", "both"] {
        for (anchor, expected) in [
            (entity_anchor.id, [local_neighbor.id, project_neighbor.id]),
            (message_anchor.id, [f.outbound.id, project_message]),
        ] {
            let response = project_graph_read(
                &f.registry,
                "context",
                json!({"entity_ids": [anchor], "hops": 1, "fanout": 1, "direction": direction}),
            )
            .await;
            let rows = response["anchors"][0]["neighbors"]
                .as_array()
                .expect("neighbor array");
            assert_eq!(rows.len(), 2, "{response}");
            let actual: HashSet<String> = rows
                .iter()
                .map(|row| row["id"].as_str().expect("neighbor id").to_string())
                .collect();
            assert_eq!(
                actual,
                expected.into_iter().map(|id| id.to_string()).collect(),
                "{response}"
            );
            assert!(
                rows.iter().all(|row| row["direction"] == json!("incoming")),
                "{response}"
            );
        }
    }
}

#[tokio::test]
async fn neighbors_refills_visible_namespaces_before_global_ordering() {
    let f = fixture(Topology::Shared).await;
    let anchor = Entity::new("local", "concept", "Neighbor page ordering anchor");
    let token = f.main.authorize(Namespace::local()).expect("entity token");
    f.main
        .entities(&token)
        .expect("entity store")
        .upsert_entity(anchor.clone())
        .await
        .expect("store anchor");
    let local = read(
        &f.registry,
        "sender",
        "comm.send",
        json!({"to": "recipient", "content": "A second local graph message.", "idempotency_key": "local-graph-page-example"}),
    ).await;
    let local_second =
        Uuid::parse_str(local["full_id"].as_str().expect("outbound id")).expect("outbound UUID");
    let project = read(
        &f.registry,
        "sender",
        "comm.send",
        json!({"namespace": "project", "to": "recipient", "content": "A project graph message.", "idempotency_key": "project-graph-page-example"}),
    ).await;
    let project_outbound =
        Uuid::parse_str(project["full_id"].as_str().expect("outbound id")).expect("outbound UUID");
    let project_token = f
        .comm
        .authorize(Namespace::parse("project").expect("project namespace"))
        .expect("project token");
    let project_note = f
        .comm
        .notes(&project_token)
        .expect("note store")
        .get_note(project_outbound)
        .await
        .expect("read outbound")
        .expect("outbound exists");
    let project_inbound = Uuid::parse_str(
        project_note
            .properties
            .as_ref()
            .expect("routing properties")["inbound_ref"]
            .as_str()
            .expect("inbound id"),
    )
    .expect("inbound UUID");
    let extra = Note::new("project", "message", "Another project graph message.").with_properties(
        json!({"direction": "inbound", "from_actor": "another-sender", "to_actor": "recipient"}),
    );
    f.comm
        .notes(&project_token)
        .expect("note store")
        .upsert_note(extra.clone())
        .await
        .expect("store project message");
    for (namespace, source, weight) in [
        ("local", f.outbound.id, 0.4),
        ("local", local_second, 0.3),
        ("project", project_inbound, 1.0),
        ("project", extra.id, 0.9),
        ("project", project_outbound, 0.8),
    ] {
        store_edge(
            &f.main,
            namespace,
            Uuid::new_v4(),
            source,
            anchor.id,
            EdgeRelation::Annotates,
            weight,
        )
        .await;
    }
    let first = project_graph_read(
        &f.registry,
        "neighbors",
        json!({"node_id": anchor.id, "direction": "both", "limit": 1}),
    )
    .await;
    let rows = neighbor_rows(&first);
    assert_eq!(rows.len(), 1, "{first}");
    assert_eq!(rows[0]["id"], json!(project_outbound), "{first}");
    assert_absent(&first, &[project_inbound, extra.id], &[]);
    let cursor = first["next_after"].as_str().expect("continuation cursor");
    let cursor_value: Value = serde_json::from_str(cursor).expect("cursor JSON");
    assert_eq!(cursor_value["node_id"], json!(project_outbound), "{first}");
    let second = project_graph_read(
        &f.registry,
        "neighbors",
        json!({"node_id": anchor.id, "direction": "both", "limit": 1, "after": cursor}),
    )
    .await;
    assert_eq!(
        neighbor_rows(&second)[0]["id"],
        json!(f.outbound.id),
        "{second}"
    );
    assert_absent(&second, &[project_inbound, extra.id], &[]);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn graph_message_prefix_errors_keep_candidates_within_mailbox_scope() {
    let f = fixture(Topology::Shared).await;
    let token = f.comm.authorize(Namespace::local()).expect("note token");
    let message_ids = [
        "cafe1001-0000-4000-8000-000000000001",
        "cafe1001-0000-4000-8000-000000000002",
        "cafe1002-0000-4000-8000-000000000003",
    ]
    .map(|id| Uuid::parse_str(id).expect("message UUID"));
    for id in message_ids {
        let mut message = Note::new("local", "message", "Graph prefix example").with_properties(
            json!({"direction": "inbound", "from_actor": "sender", "to_actor": "recipient"}),
        );
        message.id = id;
        f.comm
            .notes(&token)
            .expect("note store")
            .upsert_note(message)
            .await
            .expect("store message");
    }
    let edge_ids = [
        "bead1001-0000-4000-8000-000000000001",
        "bead1001-0000-4000-8000-000000000002",
        "bead1002-0000-4000-8000-000000000003",
    ]
    .map(|id| Uuid::parse_str(id).expect("edge UUID"));
    for (source, id) in message_ids.into_iter().zip(edge_ids) {
        store_edge(
            &f.main,
            "local",
            id,
            source,
            f.anchor,
            EdgeRelation::Annotates,
            1.0,
        )
        .await;
    }
    for reference in ["cafe1001", "cafe1002", "bead1001", "bead1002"] {
        for verb in ["neighbors", "context"] {
            let args = if verb == "neighbors" {
                json!({"node_id": reference})
            } else {
                json!({"entity_ids": [reference]})
            };
            let error = f
                .registry
                .dispatch_with_identity(
                    verb,
                    args,
                    Some(RequestIdentity {
                        actor_id: Some("observer".into()),
                        namespace: "local".into(),
                        ..Default::default()
                    }),
                )
                .await
                .expect_err("message prefix needs a full UUID");
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
            let text = error.to_string();
            assert!(
                text.contains("no record matches prefix") && text.contains(reference),
                "{text}"
            );
            for id in message_ids.into_iter().chain(edge_ids) {
                assert!(!text.contains(&id.to_string()), "{text}");
            }
            assert!(!text.contains("Graph prefix example"), "{text}");
        }
    }
    let entity_ids = [
        "deed1001-0000-4000-8000-000000000001",
        "deed1001-0000-4000-8000-000000000002",
        "deed1002-0000-4000-8000-000000000003",
    ]
    .map(|id| Uuid::parse_str(id).expect("entity UUID"));
    let token = f.main.authorize(Namespace::local()).expect("entity token");
    for id in entity_ids {
        let mut entity = Entity::new("local", "concept", "Graph prefix entity");
        entity.id = id;
        f.main
            .entities(&token)
            .expect("entity store")
            .upsert_entity(entity)
            .await
            .expect("store entity");
    }
    for verb in ["neighbors", "context"] {
        let args = if verb == "neighbors" {
            json!({"node_id": "deed1001"})
        } else {
            json!({"entity_ids": ["deed1001"]})
        };
        let error = f
            .registry
            .dispatch_with_identity(
                verb,
                args,
                Some(RequestIdentity {
                    actor_id: Some("observer".into()),
                    namespace: "local".into(),
                    ..Default::default()
                }),
            )
            .await
            .expect_err("ordinary entity prefix stays ambiguous");
        assert!(
            matches!(error, RuntimeError::AmbiguousPrefix { matches, .. }
            if matches.as_slice() == &entity_ids[..2]),
            "ordinary prefix shape changed"
        );
    }
    let neighbors = read(
        &f.registry,
        "observer",
        "neighbors",
        json!({"node_id": "deed1002"}),
    )
    .await;
    assert!(neighbor_rows(&neighbors).is_empty());
    let context = read(
        &f.registry,
        "observer",
        "context",
        json!({"entity_ids": ["deed1002"], "hops": 0}),
    )
    .await;
    assert_eq!(context["anchors"][0]["entity"]["id"], json!(entity_ids[2]));
}
