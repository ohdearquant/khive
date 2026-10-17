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

include!("message_graph_scope_tests/namespace_and_fanout_visibility_tests.rs");

include!("message_graph_scope_tests/message_read_scope_tests.rs");

include!("message_graph_scope_tests/edge_list_scope_tests.rs");

include!("message_graph_scope_tests/prefix_resolution_tests.rs");

include!("message_graph_scope_tests/page_walk_equivalence_tests.rs");

include!("message_graph_scope_tests/read_window_budget_tests.rs");
