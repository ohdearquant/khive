async fn link(fixture: &Fixture, source: Uuid, target: Uuid, relation: EdgeRelation) -> Uuid {
    let id = Uuid::new_v4();
    store_edge(&fixture.main, "local", id, source, target, relation, 1.0).await;
    id
}

async fn store_message(fixture: &Fixture, id: &str) -> Uuid {
    let id = Uuid::parse_str(id).expect("message UUID");
    let token = fixture
        .comm
        .authorize(Namespace::local())
        .expect("note token");
    let mut message = Note::new("local", "message", "Graph prefix example").with_properties(
        json!({"direction": "inbound", "from_actor": "sender", "to_actor": "recipient"}),
    );
    message.id = id;
    fixture
        .comm
        .notes(&token)
        .expect("note store")
        .upsert_note(message)
        .await
        .expect("store message");
    id
}

async fn store_concept(fixture: &Fixture, id: &str) -> Uuid {
    let id = Uuid::parse_str(id).expect("entity UUID");
    let token = fixture
        .main
        .authorize(Namespace::local())
        .expect("entity token");
    let mut entity = Entity::new("local", "concept", "Graph prefix entity");
    entity.id = id;
    fixture
        .main
        .entities(&token)
        .expect("entity store")
        .upsert_entity(entity)
        .await
        .expect("store entity");
    id
}

async fn store_seeded_concept(main: &KhiveRuntime, id: Uuid) {
    let token = main.authorize(Namespace::local()).expect("entity token");
    let mut entity = Entity::new("local", "concept", format!("Concept {}", id.simple()));
    entity.id = id;
    let entities = main.entities(&token).expect("entity store");
    entities.upsert_entity(entity).await.expect("store entity");
}

fn edge_ids(response: &Value) -> Vec<Uuid> {
    response["items"]
        .as_array()
        .or_else(|| response["edges"].as_array())
        .expect("edge array")
        .iter()
        .map(|edge| Uuid::parse_str(edge["id"].as_str().expect("edge id")).expect("edge UUID"))
        .collect()
}

fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort_unstable();
    ids
}

fn prefix_args(verb: &str, reference: &str) -> Value {
    match verb {
        "neighbors" => json!({"node_id": reference}),
        "context" => json!({"entity_ids": [reference]}),
        _ => json!({"kind": "edge", "source_id": reference}),
    }
}

/// An observer that cannot read the message under `cafe2001` is answered for
/// that prefix exactly as for a prefix no record carries.
async fn assert_prefix_reads_as_no_match(fixture: &Fixture, verbs: &[&str]) {
    for verb in verbs {
        let withheld = try_read(
            &fixture.registry,
            "observer",
            verb,
            prefix_args(verb, "cafe2001"),
        )
        .await
        .expect_err("a prefix naming only an unreadable message is refused");
        let missing = try_read(
            &fixture.registry,
            "observer",
            verb,
            prefix_args(verb, "cafe9999"),
        )
        .await
        .expect_err("a prefix naming nothing is refused");
        assert!(
            matches!(missing, RuntimeError::InvalidInput(_)),
            "{verb}: {missing}"
        );
        assert_eq!(
            std::mem::discriminant(&withheld),
            std::mem::discriminant(&missing),
            "{verb}: {withheld} / {missing}"
        );
        assert!(withheld.to_string().contains("cafe2001"), "{verb}");
        assert_eq!(
            withheld.to_string().replace("cafe2001", "<prefix>"),
            missing.to_string().replace("cafe9999", "<prefix>"),
            "{verb}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_withholds_edges_touching_unreadable_messages() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let control = link(&f, f.anchor, f.outbound_target, EdgeRelation::Extends).await;
        let observer = read(&f.registry, "observer", "list", json!({"kind": "edge"})).await;
        assert_eq!(edge_ids(&observer), [control], "{observer}");
        assert_absent(&observer, &f.edges, &[]);
        let owner = read(&f.registry, "sender", "list", json!({"kind": "edge"})).await;
        assert_eq!(
            sorted(edge_ids(&owner)),
            sorted(vec![f.edges[1], f.edges[2], control]),
            "{owner}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_full_id_of_unreadable_message_matches_an_id_with_no_edges() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let withheld = read(
            &f.registry,
            "observer",
            "list",
            json!({"kind": "edge", "source_id": f.outbound.id}),
        )
        .await;
        let none = read(
            &f.registry,
            "observer",
            "list",
            json!({"kind": "edge", "source_id": f.inbound_target}),
        )
        .await;
        assert!(edge_ids(&none).is_empty(), "{none}");
        assert_eq!(withheld, none);
        let owner = read(
            &f.registry,
            "sender",
            "list",
            json!({"kind": "edge", "source_id": f.outbound.id}),
        )
        .await;
        assert_eq!(
            sorted(edge_ids(&owner)),
            sorted(vec![f.edges[1], f.edges[2]]),
            "{owner}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_prefix_of_unreadable_message_answers_like_no_match() {
    let f = fixture(Topology::Shared).await;
    let message = store_message(&f, "cafe2001-0000-4000-8000-000000000001").await;
    let edge = link(&f, message, f.anchor, EdgeRelation::Annotates).await;
    assert_prefix_reads_as_no_match(&f, &["list"]).await;
    let owner = read(
        &f.registry,
        "recipient",
        "list",
        json!({"kind": "edge", "source_id": "cafe2001"}),
    )
    .await;
    assert_eq!(edge_ids(&owner), [edge], "{owner}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_pages_fill_from_readable_edges_in_offset_and_cursor_modes() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let token = f.main.authorize(Namespace::local()).expect("entity token");
        let source = Entity::new("local", "concept", "Edge page source");
        f.main
            .entities(&token)
            .expect("entity store")
            .upsert_entity(source.clone())
            .await
            .expect("store source");
        let mut readable = Vec::new();
        let mut withheld = f.edges.clone();
        withheld.extend([f.outbound.id, f.inbound.id]);
        for index in 0..5 {
            let target = Entity::new("local", "concept", format!("Edge page target {index}"));
            f.main
                .entities(&token)
                .expect("entity store")
                .upsert_entity(target.clone())
                .await
                .expect("store target");
            readable.push(link(&f, source.id, target.id, EdgeRelation::Extends).await);
            for message in [f.outbound.id, f.inbound.id] {
                withheld.push(link(&f, message, target.id, EdgeRelation::Annotates).await);
            }
        }

        let mut seen = Vec::new();
        for (offset, len, more) in [(0, 2, true), (2, 2, true), (4, 1, false)] {
            let page = read(
                &f.registry,
                "observer",
                "list",
                json!({"kind": "edge", "limit": 2, "offset": offset}),
            )
            .await;
            assert_absent(&page, &withheld, &[]);
            assert_eq!(edge_ids(&page).len(), len, "{page}");
            assert_eq!(page["has_more"], json!(more), "{page}");
            seen.extend(edge_ids(&page));
        }
        assert_eq!(sorted(seen), sorted(readable.clone()));

        let mut seen = Vec::new();
        let mut lengths = Vec::new();
        let mut after = String::new();
        for _ in 0..4 {
            let page = read(
                &f.registry,
                "observer",
                "list",
                json!({"kind": "edge", "limit": 2, "after": after}),
            )
            .await;
            assert_absent(&page, &withheld, &[]);
            lengths.push(edge_ids(&page).len());
            seen.extend(edge_ids(&page));
            let Some(next) = page["next_after"].as_str() else {
                assert_eq!(page["has_more"], json!(false), "{page}");
                break;
            };
            assert!(
                readable.contains(&Uuid::parse_str(next).expect("cursor UUID")),
                "{page}"
            );
            assert_eq!(page["has_more"], json!(true), "{page}");
            after = next.to_string();
        }
        assert_eq!(lengths, [2, 2, 1]);
        assert_eq!(sorted(seen), sorted(readable));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_cursor_naming_an_unreadable_edge_reads_as_no_edge() {
    let f = fixture(Topology::Shared).await;
    let unknown = Uuid::new_v4();
    let missing = try_read(
        &f.registry,
        "observer",
        "list",
        json!({"kind": "edge", "limit": 1, "after": unknown.to_string()}),
    )
    .await
    .expect_err("a cursor naming no edge is refused");
    let withheld = try_read(
        &f.registry,
        "observer",
        "list",
        json!({"kind": "edge", "limit": 1, "after": f.edges[1].to_string()}),
    )
    .await
    .expect_err("a cursor naming an unreadable edge is refused");
    assert!(matches!(withheld, RuntimeError::NotFound(_)), "{withheld}");
    assert_eq!(
        withheld
            .to_string()
            .replace(&f.edges[1].to_string(), "<id>"),
        missing.to_string().replace(&unknown.to_string(), "<id>")
    );
    let owner = read(
        &f.registry,
        "sender",
        "list",
        json!({"kind": "edge", "limit": 1, "after": f.edges[1].to_string()}),
    )
    .await;
    assert_eq!(edge_ids(&owner), [f.edges[2]], "{owner}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_limit_zero_is_refused_with_a_cursor_and_returns_no_rows_by_offset() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let first = link(&f, f.anchor, f.outbound_target, EdgeRelation::Extends).await;
        link(&f, f.anchor, f.inbound_target, EdgeRelation::Extends).await;

        // A cursor read needs room for a row, whoever asks and wherever it starts.
        for actor in ["observer", "sender"] {
            for after in [String::new(), first.to_string()] {
                let error = try_read(
                    &f.registry,
                    actor,
                    "list",
                    json!({"kind": "edge", "after": after, "limit": 0}),
                )
                .await
                .expect_err("a cursor read at limit 0 is refused");
                assert!(
                    matches!(&error, RuntimeError::InvalidInput(message)
                        if message == "cursor pagination requires limit greater than zero"),
                    "{actor}: {error}"
                );
            }
        }

        // The offset form returns no rows and says whether a row exists.
        let page = read(
            &f.registry,
            "observer",
            "list",
            json!({"kind": "edge", "limit": 0}),
        )
        .await;
        assert!(edge_ids(&page).is_empty(), "{page}");
        assert_eq!(page["has_more"], json!(true), "{page}");
        assert_eq!(page["requested_limit"], json!(0), "{page}");
        assert_eq!(page["effective_limit"], json!(0), "{page}");
        assert_eq!(page["limit_clamped"], json!(false), "{page}");
    }
}
