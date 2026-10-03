//! Message neighbors and graph context follow the caller's mailbox view.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RequestIdentity, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{Direction, Edge, LinkId, NeighborQuery};
use khive_storage::{EdgeRelation, Entity, Note};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Topology {
    Shared,
    Split,
}

struct Fixture {
    main: KhiveRuntime,
    comm: KhiveRuntime,
    registry: VerbRegistry,
    anchor: Uuid,
    outbound: Note,
    inbound: Note,
    outbound_target: Uuid,
    inbound_target: Uuid,
    edges: Vec<Uuid>,
}

fn runtime(backend: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        backend_id: BackendId::parse(backend).expect("backend id"),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

async fn read(registry: &VerbRegistry, actor: &str, verb: &str, args: Value) -> Value {
    registry
        .dispatch_with_identity(
            verb,
            args,
            Some(RequestIdentity {
                actor_id: Some(actor.to_string()),
                namespace: "local".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("scoped graph read")
}

async fn fixture(topology: Topology) -> Fixture {
    let main = runtime("main");
    let comm = match topology {
        Topology::Shared => main.clone(),
        Topology::Split => runtime("comm"),
    };
    // Keep both factory registrations linked into this integration binary.
    let _ = (KgPack::new(main.clone()), CommPack::new(comm.clone()));
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs_with_runtimes(
        &["kg".into(), "comm".into()],
        &HashMap::from([("kg".into(), main.clone()), ("comm".into(), comm.clone())]),
        &main,
        &mut builder,
    )
    .expect("register KG and Comm");
    let registry = builder.build().expect("registry builds");
    main.install_edge_rules(registry.all_edge_rules());
    comm.install_edge_rules(registry.all_edge_rules());
    registry.call_register_entity_type_validators(&main);

    let sent = read(
        &registry,
        "sender",
        "comm.send",
        json!({
            "to": "recipient",
            "subject": "Graph mailbox example",
            "content": "A message body used by the graph mailbox example.",
            "idempotency_key": "graph-mailbox-example",
        }),
    )
    .await;
    let outbound_id =
        Uuid::parse_str(sent["full_id"].as_str().expect("outbound id")).expect("outbound UUID");
    let token = comm.authorize(Namespace::local()).expect("local token");
    let outbound = comm
        .notes(&token)
        .expect("Comm notes")
        .get_note(outbound_id)
        .await
        .expect("read outbound copy")
        .expect("outbound copy exists");
    let inbound_id = Uuid::parse_str(
        outbound.properties.as_ref().expect("routing properties")["inbound_ref"]
            .as_str()
            .expect("inbound id"),
    )
    .expect("inbound UUID");
    let inbound = comm
        .notes(&token)
        .expect("Comm notes")
        .get_note(inbound_id)
        .await
        .expect("read inbound copy")
        .expect("inbound copy exists");
    assert_ne!(outbound.id, inbound.id);

    let anchor = Entity::new("local", "concept", "Message graph anchor");
    let outbound_target = Entity::new("local", "concept", "Sender graph target");
    let inbound_target = Entity::new("local", "concept", "Recipient graph target");
    let token = main.authorize(Namespace::local()).expect("graph token");
    let entities = main.entities(&token).expect("graph entities");
    for entity in [&anchor, &outbound_target, &inbound_target] {
        entities
            .upsert_entity(entity.clone())
            .await
            .expect("store graph entity");
    }
    let mut edges = Vec::new();
    for (source, target, weight) in [
        (inbound.id, anchor.id, 1.0),
        (outbound.id, anchor.id, 0.9),
        (outbound.id, outbound_target.id, 0.8),
        (inbound.id, inbound_target.id, 0.8),
    ] {
        let id = Uuid::new_v4();
        let now = Utc::now();
        main.graph(&token)
            .expect("graph store")
            .upsert_edge(Edge {
                id: LinkId(id),
                namespace: "local".into(),
                source_id: source,
                target_id: target,
                relation: EdgeRelation::Annotates,
                weight,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: None,
                target_backend: None,
            })
            .await
            .expect("store annotation");
        edges.push(id);
    }
    Fixture {
        main,
        comm,
        registry,
        anchor: anchor.id,
        outbound,
        inbound,
        outbound_target: outbound_target.id,
        inbound_target: inbound_target.id,
        edges,
    }
}

async fn store_edge(
    runtime: &KhiveRuntime,
    namespace: &str,
    id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
) {
    let token = runtime
        .authorize(Namespace::parse(namespace).expect("edge namespace"))
        .expect("edge token");
    let now = Utc::now();
    runtime
        .graph(&token)
        .expect("graph store")
        .upsert_edge(Edge {
            id: LinkId(id),
            namespace: namespace.into(),
            source_id,
            target_id,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .await
        .expect("store edge");
}

async fn project_graph_read(registry: &VerbRegistry, verb: &str, args: Value) -> Value {
    registry
        .dispatch_with_identity(
            verb,
            args,
            Some(RequestIdentity {
                actor_id: Some("sender".into()),
                namespace: "local".into(),
                visible_namespaces: vec!["project".into()],
                ..Default::default()
            }),
        )
        .await
        .expect("two-namespace graph read")
}

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
                text.contains("full UUID") && text.contains("comm.inbox"),
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
