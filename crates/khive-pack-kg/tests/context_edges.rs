//! Whole context edge payload controls through the public registry dispatch.
use std::collections::{BTreeSet, HashMap};

use chrono::Utc;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RequestIdentity, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{Edge, LinkId};
use khive_storage::{EdgeRelation, Entity, Note};
use serde_json::{json, Value};
use uuid::Uuid;

fn runtime(backend: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        backend_id: BackendId::parse(backend).unwrap(),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

fn fixture() -> Fixture {
    let runtime = runtime("main");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.call_register_entity_type_validators(&runtime);
    Fixture { runtime, registry }
}

impl Fixture {
    async fn create(&self, name: &str, description: Option<&str>) -> Uuid {
        let result = self
            .registry
            .dispatch(
                "create",
                json!({"kind": "concept", "name": name, "description": description, "skip_dedup_check": true}),
            )
            .await
            .unwrap();
        Uuid::parse_str(result["id"].as_str().unwrap()).unwrap()
    }

    async fn link(&self, source: Uuid, target: Uuid, relation: &str, weight: f64) {
        self.registry
            .dispatch(
                "link",
                json!({"source_id": source, "target_id": target, "relation": relation, "weight": weight}),
            )
            .await
            .unwrap();
    }

    async fn context(&self, args: Value) -> Value {
        self.registry.dispatch("context", args).await.unwrap()
    }

    async fn stored_entity(&self, id: Uuid, name: &str) {
        let mut entity = Entity::new("local", "concept", name);
        entity.id = id;
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .entities(&token)
            .unwrap()
            .upsert_entity(entity)
            .await
            .unwrap();
    }
}

async fn stored_edge(
    runtime: &KhiveRuntime,
    namespace: &str,
    source: Uuid,
    target: Uuid,
    relation: EdgeRelation,
    weight: f64,
) {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    let now = Utc::now();
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edge(Edge {
            id: LinkId(Uuid::new_v4()),
            namespace: namespace.into(),
            source_id: source,
            target_id: target,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .await
        .unwrap();
}

fn scalar_cost(value: &Value) -> usize {
    serde_json::to_string(value).unwrap().chars().count()
}

/// Check correspondence against both actual response views, including names of
/// via parents. Fixture-specific assertions separately pin the expected graph.
fn assert_paired(response: &Value) {
    let anchors = response["anchors"].as_array().unwrap();
    let edges = response["edges"]
        .as_array()
        .expect("edges is always an array");
    let mut names = HashMap::new();
    let mut discoveries = Vec::new();
    for anchor in anchors {
        let id = anchor["entity"]["id"].as_str().unwrap();
        names.insert(id, &anchor["entity"]["name"]);
        for neighbor in anchor["neighbors"].as_array().unwrap() {
            names.insert(neighbor["id"].as_str().unwrap(), &neighbor["name"]);
            discoveries.push((id, neighbor));
        }
    }
    assert_eq!(edges.len(), discoveries.len(), "{response}");
    let fields = BTreeSet::from([
        "source_id",
        "source_name",
        "target_id",
        "target_name",
        "relation",
        "weight",
        "direction",
        "hop",
        "via",
    ]);
    for (edge, (anchor, neighbor)) in edges.iter().zip(discoveries) {
        assert_eq!(
            edge.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            fields
        );
        let parent = neighbor["via"].as_str().unwrap_or(anchor);
        let child = neighbor["id"].as_str().unwrap();
        let (source, target) = match neighbor["direction"].as_str().unwrap() {
            "incoming" => (child, parent),
            "outgoing" | "both" => (parent, child),
            other => panic!("unexpected direction: {other}"),
        };
        assert_eq!(edge["source_id"], source);
        assert_eq!(edge["target_id"], target);
        assert_eq!(&edge["source_name"], names[source]);
        assert_eq!(&edge["target_name"], names[target]);
        for field in ["relation", "weight", "direction", "hop", "via"] {
            assert_eq!(edge[field], neighbor[field], "{field}: {response}");
        }
        if neighbor["hop"] == 1 {
            assert!(edge["via"].is_null());
        } else {
            assert_eq!(neighbor["hop"], 2);
            assert!(edge["via"].is_string());
        }
        if matches!(
            neighbor["relation"].as_str(),
            Some("competes_with" | "composed_with")
        ) {
            assert_eq!(edge["direction"], "both");
        }
    }
    assert_eq!(
        response["dropped"]["edges"],
        response["dropped"]["neighbors"]
    );
    assert!(response["dropped"].get("edges").is_some());
    assert_eq!(response["dropped"]["stage"], "budget");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn directed_edges_at_both_hops_preserve_assertion_orientation_and_names() {
    for direction in ["outgoing", "incoming"] {
        let f = fixture();
        let anchor = f.create("Anchor", None).await;
        let parent = f.create("Parent", None).await;
        let child = f.create("Child", None).await;
        for (from, to, weight) in [(anchor, parent, 0.9), (parent, child, 0.7)] {
            let (source, target) = if direction == "incoming" {
                (to, from)
            } else {
                (from, to)
            };
            f.link(source, target, "extends", weight).await;
        }
        let response = f
            .context(
                json!({"entity_ids": [anchor], "direction": direction, "hops": 2, "budget": 65536}),
            )
            .await;
        assert_paired(&response);
        let edges = response["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 2);
        let expected = if direction == "incoming" {
            [
                (parent, "Parent", anchor, "Anchor"),
                (child, "Child", parent, "Parent"),
            ]
        } else {
            [
                (anchor, "Anchor", parent, "Parent"),
                (parent, "Parent", child, "Child"),
            ]
        };
        for (edge, (source, source_name, target, target_name)) in edges.iter().zip(expected) {
            assert_eq!(edge["source_id"], json!(source));
            assert_eq!(edge["source_name"], source_name);
            assert_eq!(edge["target_id"], json!(target));
            assert_eq!(edge["target_name"], target_name);
            assert_eq!(edge["relation"], "extends");
            assert_eq!(edge["direction"], direction);
        }
        assert_eq!(edges[0]["hop"], 1);
        assert_eq!(edges[0]["weight"], 0.9);
        assert!(edges[0]["via"].is_null());
        assert_eq!(edges[1]["hop"], 2);
        assert_eq!(edges[1]["weight"], 0.7);
        assert_eq!(edges[1]["via"], json!(parent));
        let one = f
            .context(
                json!({"entity_ids": [anchor], "direction": direction, "hops": 1, "budget": 65536}),
            )
            .await;
        assert_paired(&one);
        assert_eq!(one["edges"], json!([edges[0]]));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn symmetric_selected_hits_are_both_on_all_filter_and_direction_paths() {
    for relation in [EdgeRelation::CompetesWith, EdgeRelation::ComposedWith] {
        let f = fixture();
        let low = Uuid::from_u128(1);
        let high = Uuid::from_u128(2);
        f.stored_entity(low, "Low").await;
        f.stored_entity(high, "High").await;
        // Canonical stored order is low -> high. Incoming/both select high as
        // parent and must return high -> low without assertion semantics.
        stored_edge(&f.runtime, "local", low, high, relation, 0.6).await;
        for filter in [
            None,
            Some(json!([])),
            Some(json!([relation.as_str(), "extends"])),
            Some(json!([relation.as_str()])),
        ] {
            for direction in ["outgoing", "incoming", "both"] {
                let parent = if direction == "outgoing" { low } else { high };
                let other = if parent == low { high } else { low };
                let mut args = json!({"entity_ids": [parent], "direction": direction, "hops": 1, "budget": 65536, "fanout": 1});
                if let Some(value) = &filter {
                    args["relations"] = value.clone();
                }
                let response = f.context(args).await;
                assert_paired(&response);
                assert_eq!(response["edges"].as_array().unwrap().len(), 1, "{response}");
                let edge = &response["edges"][0];
                assert_eq!(edge["source_id"], json!(parent));
                assert_eq!(edge["target_id"], json!(other));
                assert_eq!(edge["direction"], "both");
                assert_eq!(edge["relation"], relation.as_str());
                assert_eq!(edge["weight"], 0.6);
            }
        }
        // Normalizing a selected hit must not widen absent/mixed filter
        // selection into the opposite direction of its stored assertion.
        for filter in [None, Some(json!([relation.as_str(), "extends"]))] {
            let mut args = json!({"entity_ids": [high], "direction": "outgoing", "hops": 1});
            if let Some(value) = filter {
                args["relations"] = value;
            }
            let response = f.context(args).await;
            assert_paired(&response);
            assert!(response["edges"].as_array().unwrap().is_empty());
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn successful_empty_shapes_and_existing_input_refusals_remain_explicit() {
    let f = fixture();
    let anchor = f.create("Anchor without neighbors", None).await;
    for hops in [0, 1, 2] {
        let response = f
            .context(json!({"entity_ids": [anchor], "hops": hops}))
            .await;
        assert_paired(&response);
        assert_eq!(response["edges"], json!([]));
        assert_eq!(response["dropped"]["edges"], 0);
        assert_eq!(response["truncated"], false);
    }
    let empty = fixture()
        .context(json!({"query": "no matching record", "hops": 2}))
        .await;
    assert_paired(&empty);
    assert_eq!(empty["anchors"], json!([]));
    assert_eq!(empty["edges"], json!([]));
    let child = f.create("Connected child", None).await;
    f.link(anchor, child, "extends", 0.5).await;
    let zero = f.context(json!({"entity_ids": [anchor], "hops": 0})).await;
    assert_paired(&zero);
    assert_eq!(zero["edges"], json!([]));
    assert_eq!(zero["dropped"]["edges"], 0);
    assert!(matches!(
        f.registry
            .dispatch("context", json!({"entity_ids": [Uuid::new_v4()]}))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
    assert!(matches!(
        f.registry
            .dispatch(
                "context",
                json!({"entity_ids": [anchor], "relations": ["not_a_relation"]})
            )
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
    assert!(matches!(
        f.registry
            .dispatch(
                "context",
                json!({"entity_ids": [anchor], "include_deleted": true})
            )
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
    let note = Note::new(
        "local",
        "observation",
        "A note remains invalid as an explicit anchor.",
    );
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    f.runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    assert!(matches!(
        f.registry
            .dispatch("context", json!({"entity_ids": [note.id]}))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn real_unicode_endpoint_cost_is_charged_atomically_at_exact_fit_and_minimum() {
    let f = fixture();
    let anchor = f.create(&"锚".repeat(20), None).await;
    let child = f.create(&"邻".repeat(120), None).await;
    f.link(anchor, child, "extends", 0.8).await;
    let ample = f
        .context(json!({"entity_ids": [anchor], "hops": 1, "budget": 65536}))
        .await;
    assert_paired(&ample);
    assert_eq!(ample["edges"].as_array().unwrap().len(), 1);
    let entity_cost = scalar_cost(&ample["anchors"][0]["entity"]);
    let neighbor_cost = scalar_cost(&ample["anchors"][0]["neighbors"][0]);
    let edge_cost = scalar_cost(&ample["edges"][0]);
    let exact = entity_cost + neighbor_cost + edge_cost;
    assert!(exact > 256 && exact < 65536);
    assert!(entity_cost + neighbor_cost >= 256);
    assert!(serde_json::to_string(&ample["edges"][0]).unwrap().len() > edge_cost);
    let fit = f
        .context(json!({"entity_ids": [anchor], "hops": 1, "budget": exact}))
        .await;
    assert_paired(&fit);
    assert_eq!(fit["anchors"], ample["anchors"]);
    assert_eq!(fit["edges"], ample["edges"]);
    assert_eq!(fit["truncated"], false);
    for budget in [exact - 1, entity_cost + neighbor_cost, 256, 1] {
        let cut = f
            .context(json!({"entity_ids": [anchor], "hops": 1, "budget": budget}))
            .await;
        assert_paired(&cut);
        assert_eq!(cut["anchors"].as_array().unwrap().len(), 1);
        assert_eq!(cut["edges"], json!([]));
        assert_eq!(cut["anchors"][0]["neighbors"], json!([]));
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["dropped"]["neighbors"], 1);
        assert_eq!(cut["dropped"]["edges"], 1);
        if budget == 1 {
            assert_eq!(cut["effective_budget"], 256);
            assert_eq!(cut["budget_clamped"], true);
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn anchor_priority_continues_to_a_later_fitting_pair() {
    let f = fixture();
    let first = f.create("First anchor", None).await;
    let second = f.create("Second anchor", None).await;
    let huge = f.create("Huge neighbor", Some(&"x".repeat(4000))).await;
    let small = f.create("Small neighbor", None).await;
    f.link(first, huge, "extends", 0.9).await;
    f.link(second, small, "extends", 0.8).await;
    let ample = f
        .context(json!({"entity_ids": [first, second], "hops": 1, "budget": 65536}))
        .await;
    assert_paired(&ample);
    let budget = ample["anchors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| scalar_cost(&a["entity"]))
        .sum::<usize>()
        + scalar_cost(&ample["anchors"][1]["neighbors"][0])
        + scalar_cost(&ample["edges"][1]);
    assert!(budget >= 256);
    let response = f
        .context(json!({"entity_ids": [first, second], "hops": 1, "budget": budget}))
        .await;
    assert_paired(&response);
    assert_eq!(response["anchors"].as_array().unwrap().len(), 2);
    assert_eq!(response["anchors"][0]["neighbors"], json!([]));
    assert_eq!(response["anchors"][1]["neighbors"][0]["id"], json!(small));
    assert_eq!(response["edges"], json!([ample["edges"][1]]));
    assert_eq!(response["truncated"], true);
    assert_eq!(response["dropped"]["anchors"], 0);
    assert_eq!(response["dropped"]["edges"], 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ordered_diamond_and_cycle_preserve_first_owner_and_flattened_edge_order() {
    let f = fixture();
    let [a, b, p, q, x, z, t] = [1, 2, 10, 11, 20, 21, 30].map(Uuid::from_u128);
    for (id, name) in [
        (a, "A"),
        (b, "B"),
        (p, "P"),
        (q, "Q"),
        (x, "X"),
        (z, "Z"),
        (t, "T"),
    ] {
        f.stored_entity(id, name).await;
    }
    for (source, target, weight) in [
        (a, p, 0.8),
        (a, q, 0.8),
        (p, x, 0.7),
        (q, x, 0.7),
        (q, z, 0.7),
        (p, a, 0.6),
        (b, x, 1.0),
        (b, t, 0.5),
    ] {
        stored_edge(
            &f.runtime,
            "local",
            source,
            target,
            EdgeRelation::Extends,
            weight,
        )
        .await;
    }
    let args = json!({"entity_ids": [a,b], "hops": 2, "direction": "outgoing", "budget": 65536});
    let response = f.context(args.clone()).await;
    assert_paired(&response);
    let rows = response["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(
        rows.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(),
        vec![json!(p), json!(q), json!(x), json!(z)]
    );
    assert_eq!(rows[2]["via"], json!(p));
    assert_eq!(rows[3]["via"], json!(q));
    assert_eq!(
        response["anchors"][1]["neighbors"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(response["anchors"][1]["neighbors"][0]["id"], json!(t));
    assert_eq!(
        response["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["target_id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(p), json!(q), json!(x), json!(z), json!(t)]
    );
    assert_eq!(response, f.context(args).await);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_and_unnamed_note_parents_keep_nullable_endpoint_names() {
    for name in [None, Some("Named note")] {
        let f = fixture();
        let anchor = f.create("Note anchor", None).await;
        let child = f.create("Note child", None).await;
        let mut note = Note::new(
            "local",
            "observation",
            "Note content is not its display name.",
        );
        note.name = name.map(str::to_owned);
        let token = f.runtime.authorize(Namespace::local()).unwrap();
        f.runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note.clone())
            .await
            .unwrap();
        for (target, weight) in [(anchor, 0.9), (child, 0.8)] {
            stored_edge(
                &f.runtime,
                "local",
                note.id,
                target,
                EdgeRelation::Annotates,
                weight,
            )
            .await;
        }
        let response = f
            .context(
                json!({"entity_ids": [anchor], "hops": 2, "direction": "both", "budget": 65536}),
            )
            .await;
        assert_paired(&response);
        assert_eq!(response["edges"].as_array().unwrap().len(), 2);
        assert_eq!(response["edges"][0]["source_id"], json!(note.id));
        assert_eq!(response["edges"][0]["source_name"], json!(name));
        assert_eq!(response["edges"][0]["target_name"], "Note anchor");
        assert_eq!(response["edges"][1]["source_id"], json!(note.id));
        assert_eq!(response["edges"][1]["source_name"], json!(name));
        assert_eq!(response["edges"][1]["target_name"], "Note child");
        assert_eq!(response["edges"][1]["via"], json!(note.id));
        assert!(response["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| !edge.to_string().contains(&note.content)));
        // An actual note-to-note annotation puts the nullable via note on the target side.
        let origin = Note::new("local", "observation", "Origin content.").with_name("Origin note");
        f.runtime
            .notes(&token)
            .unwrap()
            .upsert_note(origin.clone())
            .await
            .unwrap();
        f.link(origin.id, note.id, "annotates", 0.7).await;
        let with_origin = f
            .context(
                json!({"entity_ids": [anchor], "hops": 2, "direction": "both", "budget": 65536}),
            )
            .await;
        assert_paired(&with_origin);
        let target_note = with_origin["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["source_id"] == json!(origin.id))
            .unwrap();
        assert_eq!(target_note["source_name"], "Origin note");
        assert_eq!(target_note["target_id"], json!(note.id));
        assert_eq!(target_note["target_name"], json!(name));
        assert_eq!(target_note["direction"], "incoming");
        assert_eq!(target_note["hop"], 2);
        assert_eq!(target_note["via"], json!(note.id));
        // Reaching the note from an entity still preserves its assertion orientation.
        let second_anchor = f.create("Reverse note anchor", None).await;
        f.link(second_anchor, child, "extends", 0.9).await;
        let reverse = f.context(json!({"entity_ids": [second_anchor], "hops": 2, "direction": "both", "budget": 65536})).await;
        assert_paired(&reverse);
        let note_row = reverse["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["hop"] == 2 && edge["source_id"] == json!(note.id))
            .unwrap();
        assert_eq!(note_row["source_name"], json!(name));
        assert_eq!(note_row["via"], json!(child));
    }
}

#[derive(Clone, Copy)]
enum Topology {
    Shared,
    Split,
}

struct MailboxFixture {
    registry: VerbRegistry,
    anchor: Uuid,
    outbound: Note,
    inbound: Note,
    outbound_target: Uuid,
    inbound_target: Uuid,
}

async fn actor_read(registry: &VerbRegistry, actor: &str, verb: &str, args: Value) -> Value {
    registry
        .dispatch_with_identity(
            verb,
            args,
            Some(RequestIdentity {
                actor_id: Some(actor.into()),
                namespace: "local".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
}

async fn mailbox_fixture(topology: Topology, unnamed: bool) -> MailboxFixture {
    let main = runtime("main");
    let comm = match topology {
        Topology::Shared => main.clone(),
        Topology::Split => runtime("comm"),
    };
    let _ = (KgPack::new(main.clone()), CommPack::new(comm.clone()));
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs_with_runtimes(
        &["kg".into(), "comm".into()],
        &HashMap::from([("kg".into(), main.clone()), ("comm".into(), comm.clone())]),
        &main,
        &mut builder,
    )
    .unwrap();
    let registry = builder.build().unwrap();
    main.install_edge_rules(registry.all_edge_rules());
    comm.install_edge_rules(registry.all_edge_rules());
    registry.call_register_entity_type_validators(&main);
    let sent = actor_read(&registry, "sender", "comm.send", json!({
        "to": "recipient", "subject": "Message edge fixture", "content": "Authorized mailbox body.",
        "idempotency_key": "context-edge-mailbox-fixture",
    })).await;
    let outbound_id = Uuid::parse_str(sent["full_id"].as_str().unwrap()).unwrap();
    let token = comm.authorize(Namespace::local()).unwrap();
    let store = comm.notes(&token).unwrap();
    let mut outbound = store.get_note(outbound_id).await.unwrap().unwrap();
    let inbound_id = Uuid::parse_str(
        outbound.properties.as_ref().unwrap()["inbound_ref"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let mut inbound = store.get_note(inbound_id).await.unwrap().unwrap();
    outbound.name = if unnamed {
        None
    } else {
        Some("Sender message label".into())
    };
    inbound.name = if unnamed {
        None
    } else {
        Some("Recipient message label".into())
    };
    store.upsert_note(outbound.clone()).await.unwrap();
    store.upsert_note(inbound.clone()).await.unwrap();
    let anchor = Entity::new("local", "concept", "Mailbox anchor");
    let outbound_target = Entity::new("local", "concept", "Sender target");
    let inbound_target = Entity::new("local", "concept", "Recipient target");
    let token = main.authorize(Namespace::local()).unwrap();
    for entity in [&anchor, &outbound_target, &inbound_target] {
        main.entities(&token)
            .unwrap()
            .upsert_entity(entity.clone())
            .await
            .unwrap();
    }
    for (source, target, weight) in [
        (inbound.id, anchor.id, 1.0),
        (outbound.id, anchor.id, 0.9),
        (outbound.id, outbound_target.id, 0.8),
        (inbound.id, inbound_target.id, 0.8),
    ] {
        stored_edge(
            &main,
            "local",
            source,
            target,
            EdgeRelation::Annotates,
            weight,
        )
        .await;
    }
    MailboxFixture {
        registry,
        anchor: anchor.id,
        outbound,
        inbound,
        outbound_target: outbound_target.id,
        inbound_target: inbound_target.id,
    }
}

fn assert_absent(response: &Value, ids: &[Uuid], names: &[&str]) {
    let text = response.to_string();
    for id in ids {
        assert!(!text.contains(&id.to_string()), "hidden ID in {response}");
    }
    for name in names {
        assert!(!text.contains(name), "hidden name/body in {response}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn actual_shared_and_split_mailboxes_filter_the_whole_paired_payload() {
    for topology in [Topology::Shared, Topology::Split] {
        for unnamed in [false, true] {
            let f = mailbox_fixture(topology, unnamed).await;
            for hops in [1, 2] {
                // Hop 1 retains the fanout-1 mailbox refill control. At hop 2 the
                // already visited anchor occupies the parent's first raw hit.
                let fanout = if hops == 1 { 1 } else { 2 };
                let observer = actor_read(&f.registry, "observer", "context", json!({"entity_ids": [f.anchor], "hops": hops, "direction": "both", "fanout": fanout, "budget": 65536})).await;
                assert_paired(&observer);
                assert_eq!(observer["edges"], json!([]));
                assert_absent(
                    &observer,
                    &[
                        f.outbound.id,
                        f.inbound.id,
                        f.outbound_target,
                        f.inbound_target,
                    ],
                    &[
                        "Sender message label",
                        "Recipient message label",
                        "Authorized mailbox body.",
                        "Sender target",
                        "Recipient target",
                    ],
                );
                for (actor, own, own_target, other, other_target, hidden_name) in [
                    (
                        "sender",
                        &f.outbound,
                        f.outbound_target,
                        &f.inbound,
                        f.inbound_target,
                        "Recipient message label",
                    ),
                    (
                        "recipient",
                        &f.inbound,
                        f.inbound_target,
                        &f.outbound,
                        f.outbound_target,
                        "Sender message label",
                    ),
                ] {
                    let response = actor_read(&f.registry, actor, "context", json!({"entity_ids": [f.anchor], "hops": hops, "direction": "both", "fanout": fanout, "budget": 65536})).await;
                    assert_paired(&response);
                    assert_absent(&response, &[other.id, other_target], &[hidden_name]);
                    assert_eq!(response["edges"].as_array().unwrap().len(), hops as usize);
                    assert_eq!(response["edges"][0]["source_id"], json!(own.id));
                    assert_eq!(response["edges"][0]["source_name"], json!(own.name));
                    assert_eq!(response["edges"][0]["target_id"], json!(f.anchor));
                    assert_eq!(response["edges"][0]["direction"], "incoming");
                    if hops == 2 {
                        assert_eq!(response["edges"][1]["source_id"], json!(own.id));
                        assert_eq!(response["edges"][1]["source_name"], json!(own.name));
                        assert_eq!(response["edges"][1]["target_id"], json!(own_target));
                        assert_eq!(response["edges"][1]["direction"], "outgoing");
                        assert_eq!(response["edges"][1]["via"], json!(own.id));
                    }
                    assert_eq!(response["truncated"], false);
                    assert_eq!(response["dropped"]["edges"], 0);
                }
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn context_edges_do_not_widen_the_visible_namespace_set() {
    let f = fixture();
    let anchor = f.create("Visible anchor", None).await;
    let hidden = Entity::new("private", "concept", "Private endpoint label");
    let token = f
        .runtime
        .authorize(Namespace::parse("private").unwrap())
        .unwrap();
    f.runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(hidden.clone())
        .await
        .unwrap();
    stored_edge(
        &f.runtime,
        "private",
        anchor,
        hidden.id,
        EdgeRelation::Extends,
        1.0,
    )
    .await;
    let response = f
        .context(json!({"entity_ids": [anchor], "direction": "both", "hops": 2}))
        .await;
    assert_paired(&response);
    assert_eq!(response["edges"], json!([]));
    assert_absent(&response, &[hidden.id], &["Private endpoint label"]);
    assert!(matches!(
        f.registry
            .dispatch("context", json!({"entity_ids": [hidden.id]}))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
}
