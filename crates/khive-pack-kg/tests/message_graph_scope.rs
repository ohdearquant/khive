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

async fn try_read(
    registry: &VerbRegistry,
    actor: &str,
    verb: &str,
    args: Value,
) -> Result<Value, RuntimeError> {
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
}

async fn read(registry: &VerbRegistry, actor: &str, verb: &str, args: Value) -> Value {
    try_read(registry, actor, verb, args)
        .await
        .expect("scoped graph read")
}

fn scoped_registry(topology: Topology) -> (KhiveRuntime, KhiveRuntime, VerbRegistry) {
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
    (main, comm, registry)
}

async fn fixture(topology: Topology) -> Fixture {
    let (main, comm, registry) = scoped_registry(topology);

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

#[derive(Clone, Copy)]
enum Run {
    Readable(usize),
    Withheld(usize),
}

/// Ids are seeded by position, so two stores written from the same runs hold
/// the readable records under the same ids and their pages compare as values.
/// Edge ids rise with creation order, which keeps every sort tie in that order.
fn seeded(kind: u128, position: usize) -> Uuid {
    Uuid::from_u128((kind << 64) | position as u128)
}

/// Writes `runs` in order. A readable edge joins two concepts. A withheld edge
/// runs from one of `messages` to a concept, and is skipped when there are no
/// messages: that is the store that never held it. Returns the ids of the
/// readable edges and of the withheld ones, which a store without them still
/// names.
async fn write_runs(
    main: &KhiveRuntime,
    messages: &[Uuid],
    runs: &[Run],
) -> (Vec<Uuid>, Vec<Uuid>) {
    let mut readable = Vec::new();
    let mut withheld = Vec::new();
    let mut position = 0;
    for run in runs {
        let (count, hidden) = match *run {
            Run::Readable(count) => (count, false),
            Run::Withheld(count) => (count, true),
        };
        for _ in 0..count {
            position += 1;
            let edge = seeded(0xed, position);
            let target = seeded(0xc0, position);
            if hidden {
                withheld.push(edge);
                let Some(&message) = messages.get(position % 2) else {
                    continue;
                };
                store_seeded_concept(main, target).await;
                store_edge(
                    main,
                    "local",
                    edge,
                    message,
                    target,
                    EdgeRelation::Annotates,
                    1.0,
                )
                .await;
            } else {
                let source = seeded(0x5c, position);
                store_seeded_concept(main, source).await;
                store_seeded_concept(main, target).await;
                store_edge(
                    main,
                    "local",
                    edge,
                    source,
                    target,
                    EdgeRelation::Extends,
                    1.0,
                )
                .await;
                readable.push(edge);
            }
        }
    }
    (readable, withheld)
}

/// A page as a value, without the stamps the store assigns when it writes.
fn page_without_stamps(page: &Value) -> Value {
    let mut page = page.clone();
    for key in ["items", "edges"] {
        if let Some(edges) = page.get_mut(key).and_then(Value::as_array_mut) {
            for edge in edges {
                for stamp in ["created_at", "updated_at"] {
                    edge.as_object_mut().expect("edge object").remove(stamp);
                }
            }
        }
    }
    page
}

fn page_keys(page: &Value) -> Vec<&String> {
    page.as_object().expect("page object").keys().collect()
}

/// Reads every page of the observer's edge list, advancing the offset by
/// `limit` or following `next_after`.
async fn walk_pages(registry: &VerbRegistry, limit: u32, cursor: bool) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut after = String::new();
    let mut offset = 0;
    loop {
        let args = if cursor {
            json!({"kind": "edge", "limit": limit, "after": after})
        } else {
            json!({"kind": "edge", "limit": limit, "offset": offset})
        };
        let page = read(registry, "observer", "list", args).await;
        pages.push(page_without_stamps(&page));
        assert!(pages.len() <= 64, "the walk does not end: {page}");
        if cursor {
            let Some(next) = page["next_after"].as_str() else {
                return pages;
            };
            after = next.to_string();
        } else {
            if page["has_more"] != json!(true) {
                return pages;
            }
            offset += limit;
        }
    }
}

/// The observer cannot read the messages that the withheld edges touch. Every
/// answer it gets from the edge list is the answer of a store built from the
/// same readable records in which those messages and edges were never written.
async fn assert_walks_match_the_store_without_them(topology: Topology, runs: &[Run]) {
    let f = fixture(topology).await;
    let (readable, withheld) = write_runs(&f.main, &[f.outbound.id, f.inbound.id], runs).await;
    let (twin_main, _, twin_registry) = scoped_registry(topology);
    let (twin_readable, twin_withheld) = write_runs(&twin_main, &[], runs).await;
    assert_eq!(readable, twin_readable);
    assert_eq!(withheld, twin_withheld);

    for cursor in [false, true] {
        for limit in [1, 2, 3] {
            let mode = if cursor { "cursor" } else { "offset" };
            let seen = walk_pages(&f.registry, limit, cursor).await;
            let expected = walk_pages(&twin_registry, limit, cursor).await;
            assert_eq!(
                seen.len(),
                expected.len(),
                "{mode} limit {limit}: pages in the walk"
            );
            for (index, (page, twin)) in seen.iter().zip(&expected).enumerate() {
                let context = format!("{mode} limit {limit} page {index}");
                assert_eq!(page_keys(page), page_keys(twin), "{context}: keys");
                assert_eq!(page, twin, "{context}");
            }
        }
    }

    // At limit 0 the offset form returns no rows and a cursor read is refused.
    let args = json!({"kind": "edge", "limit": 0});
    let seen = read(&f.registry, "observer", "list", args.clone()).await;
    let twin = read(&twin_registry, "observer", "list", args.clone()).await;
    assert_eq!(
        page_without_stamps(&seen),
        page_without_stamps(&twin),
        "{args}"
    );
    let args = json!({"kind": "edge", "limit": 0, "after": ""});
    let seen = try_read(&f.registry, "observer", "list", args.clone())
        .await
        .expect_err("a cursor read at limit 0 is refused");
    let twin = try_read(&twin_registry, "observer", "list", args)
        .await
        .expect_err("a cursor read at limit 0 is refused");
    assert_eq!(seen.to_string(), twin.to_string());
    assert_eq!(std::mem::discriminant(&seen), std::mem::discriminant(&twin));

    let args = json!({"kind": "edge", "limit": 1, "after": withheld[0].to_string()});
    let seen = try_read(&f.registry, "observer", "list", args.clone())
        .await
        .expect_err("a cursor naming an unreadable edge is refused");
    let twin = try_read(&twin_registry, "observer", "list", args)
        .await
        .expect_err("a cursor naming no edge is refused");
    assert_eq!(seen.to_string(), twin.to_string());
    assert_eq!(std::mem::discriminant(&seen), std::mem::discriminant(&twin));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_withheld_run_between_readable_edges() {
    for topology in [Topology::Shared, Topology::Split] {
        let runs = [Run::Readable(1), Run::Withheld(250), Run::Readable(2)];
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_withheld_run_before_last_readable_edge() {
    for topology in [Topology::Shared, Topology::Split] {
        let runs = [Run::Withheld(250), Run::Readable(1)];
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_withheld_run_after_last_readable_edge() {
    for topology in [Topology::Shared, Topology::Split] {
        let runs = [Run::Readable(1), Run::Withheld(250)];
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_only_withheld_edges() {
    for topology in [Topology::Shared, Topology::Split] {
        assert_walks_match_the_store_without_them(topology, &[Run::Withheld(250)]).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_interleaved_withheld_edges() {
    let mut runs = vec![Run::Readable(1)];
    for size in [1, 2, 4, 8, 16, 32, 64, 128] {
        runs.extend([Run::Withheld(size), Run::Readable(1)]);
    }
    for topology in [Topology::Shared, Topology::Split] {
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

/// Reads the observer's `args` and counts the edge-list statements SQLite
/// started, in the cursor form or the offset form.
async fn edge_list_reads(fixture: &Fixture, args: Value, cursor: bool) -> (usize, Value) {
    let observation = fixture
        .main
        .backend()
        .pool()
        .observe_test_statement_starts(100_000)
        .expect("statement observation");
    let page = read(&fixture.registry, "observer", "list", args).await;
    let started = observation
        .started_statements()
        .expect("complete statement observation");
    drop(observation);
    let reads = started
        .iter()
        .filter(|statement| {
            if cursor {
                statement
                    .sql
                    .contains("FROM graph_edges_seq CROSS JOIN graph_edges")
            } else {
                statement.sql.starts_with("SELECT namespace, id, source_id")
                    && statement.sql.contains(" OFFSET ?")
            }
        })
        .count();
    (reads, page)
}

/// The most reads of the store one scan may start:
/// `floor(log2(1 + T / w)) + floor(T / C) + 1` for `T` edges passed over, a
/// window `w` that first holds a withheld edge and `C = EDGE_LIST_MAX_LIMIT`.
fn window_formula(passed_over: u32, first_window: u32) -> usize {
    let doublings = (1 + passed_over / first_window).ilog2();
    let at_the_cap = passed_over / KhiveRuntime::EDGE_LIST_MAX_LIMIT;
    (doublings + at_the_cap + 1) as usize
}

async fn assert_a_long_withheld_run_stays_within_the_window_formula(cursor: bool) {
    let f = fixture(Topology::Shared).await;
    let messages = [f.outbound.id, f.inbound.id];
    let runs = [Run::Withheld(2000), Run::Readable(1)];
    let (readable, _) = write_runs(&f.main, &messages, &runs).await;
    // The fixture's own four edges come first, and the one readable edge last.
    let passed_over = f.edges.len() as u32 + 2000 + 1;

    // A filter nothing matches takes one pass, which counts the statements a pass starts.
    let mut args = json!({"kind": "edge", "limit": 1, "relations": ["supports"]});
    if cursor {
        args["after"] = json!("");
    }
    let (per_pass, _) = edge_list_reads(&f, args, cursor).await;
    assert!(per_pass >= 1, "no edge-list statement was recognised");

    let mut args = json!({"kind": "edge", "limit": 1});
    if cursor {
        args["after"] = json!("");
    }
    let (reads, page) = edge_list_reads(&f, args, cursor).await;
    assert_eq!(edge_ids(&page), [readable[0]], "{page}");
    let allowed = per_pass * window_formula(passed_over, 2);
    assert!(
        reads > per_pass,
        "the run is crossed in several passes: {reads} statements"
    );
    assert!(
        reads <= allowed,
        "{reads} edge-list statements for 2000 withheld edges; the formula allows {allowed}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_cursor_store_reads_stay_within_the_window_formula() {
    assert_a_long_withheld_run_stays_within_the_window_formula(true).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_offset_store_reads_stay_within_the_window_formula() {
    assert_a_long_withheld_run_stays_within_the_window_formula(false).await;
}
