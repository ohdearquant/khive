//! Read scoping of message notes on generic reads.

use std::collections::HashMap;

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

#[derive(Clone, Copy)]
enum Topology {
    Shared,
    Split,
}

struct Fixture {
    main: KhiveRuntime,
    comm: KhiveRuntime,
    registry: VerbRegistry,
    outbound: Note,
    inbound: Note,
}

fn runtime(backend: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        backend_id: BackendId::parse(backend).expect("backend id"),
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

async fn dispatch(
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
                namespace: "local".into(),
                actor_id: Some(actor.into()),
                ..Default::default()
            }),
        )
        .await
}

async fn fixture(topology: Topology) -> Fixture {
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
    .expect("register KG and Comm");
    let registry = builder.build().expect("registry builds");
    let sent = dispatch(
        &registry,
        "sender",
        "comm.send",
        json!({
            "to": "recipient",
            "subject": "Mailbox example",
            "content": "mailboxsearch example body",
            "idempotency_key": "mailbox-search-example",
        }),
    )
    .await
    .expect("send message");
    let outbound_id =
        Uuid::parse_str(sent["full_id"].as_str().expect("outbound id")).expect("outbound UUID");
    let token = comm.authorize(Namespace::local()).expect("local token");
    let store = comm.notes(&token).expect("Comm notes");
    let outbound = store
        .get_note(outbound_id)
        .await
        .expect("outbound read")
        .expect("outbound");
    let inbound_id = Uuid::parse_str(
        outbound.properties.as_ref().expect("routing properties")["inbound_ref"]
            .as_str()
            .expect("inbound id"),
    )
    .expect("inbound UUID");
    let inbound = store
        .get_note(inbound_id)
        .await
        .expect("inbound read")
        .expect("inbound");
    Fixture {
        main,
        comm,
        registry,
        outbound,
        inbound,
    }
}

async fn assert_missing(registry: &VerbRegistry, actor: &str, args: Value) {
    let error = dispatch(registry, actor, "get", args)
        .await
        .expect_err("outside mailbox");
    assert!(
        matches!(&error, RuntimeError::NotFound(_))
            || matches!(&error, RuntimeError::Khive(error) if error.kind() == khive_types::ErrorKind::NotFound),
        "a message outside the caller mailbox answers as missing: {error}"
    );
}

#[tokio::test]
async fn message_get_preserves_each_party_copy_on_both_backends() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        for (actor, own, other) in [
            ("sender", &f.outbound, &f.inbound),
            ("recipient", &f.inbound, &f.outbound),
        ] {
            let found = dispatch(&f.registry, actor, "get", json!({"id": own.id}))
                .await
                .expect("own copy");
            assert_eq!(found["id"], own.id.to_string());
            assert_eq!(found["content"], own.content);
            assert_missing(&f.registry, actor, json!({"id": other.id})).await;
        }
        for note in [&f.outbound, &f.inbound] {
            assert_missing(&f.registry, "observer", json!({"id": note.id})).await;
        }
    }
}

#[tokio::test]
async fn message_get_keys_and_tombstones_follow_the_mailbox_view() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let token = f.main.authorize(Namespace::local()).expect("main token");
        for (actor, direction, route) in [
            ("sender", "outbound", "from_actor"),
            ("recipient", "inbound", "to_actor"),
        ] {
            let mut note = Note::new("local", "message", "Keyed mailbox example")
                .with_properties(json!({"direction": direction, (route): actor}));
            note.key = Some(format!("mailbox/{actor}"));
            f.main
                .notes(&token)
                .unwrap()
                .upsert_note(note.clone())
                .await
                .unwrap();
            for selector in [
                json!({"key": note.key}),
                json!({"key": note.key, "kind": "message"}),
            ] {
                let found = dispatch(&f.registry, actor, "get", selector.clone())
                    .await
                    .expect("own keyed note");
                assert_eq!(found["id"], note.id.to_string());
                assert_missing(&f.registry, "observer", selector).await;
            }
        }
        let token = f.comm.authorize(Namespace::local()).expect("Comm token");
        for (actor, mut note) in [
            ("sender", f.outbound.clone()),
            ("recipient", f.inbound.clone()),
        ] {
            note.deleted_at = Some(note.updated_at);
            f.comm
                .notes(&token)
                .unwrap()
                .upsert_note(note.clone())
                .await
                .unwrap();
            let args = json!({"id": note.id, "include_deleted": true});
            let found = dispatch(&f.registry, actor, "get", args.clone())
                .await
                .expect("own tombstone");
            assert_eq!(found["id"], note.id.to_string());
            assert_eq!(found["content"], note.content);
            assert_missing(&f.registry, "observer", args).await;
        }
    }
}

fn edge(source: Uuid, target: Uuid, relation: EdgeRelation) -> Edge {
    let now = Utc::now();
    Edge {
        id: LinkId(Uuid::new_v4()),
        namespace: "local".into(),
        source_id: source,
        target_id: target,
        relation,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    }
}

#[tokio::test]
async fn message_get_scopes_edge_endpoints_and_annotation_bodies() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let token = f.main.authorize(Namespace::local()).expect("graph token");
        let first = Entity::new("local", "concept", "First example");
        let second = Entity::new("local", "concept", "Second example");
        for entity in [&first, &second] {
            f.main
                .entities(&token)
                .unwrap()
                .upsert_entity(entity.clone())
                .await
                .unwrap();
        }
        let parent = edge(first.id, second.id, EdgeRelation::DerivedFrom);
        f.main
            .graph(&token)
            .unwrap()
            .upsert_edge(parent.clone())
            .await
            .unwrap();
        for note in [&f.outbound, &f.inbound] {
            let annotation = edge(note.id, parent.id.0, EdgeRelation::Annotates);
            f.main
                .graph(&token)
                .unwrap()
                .upsert_edge(annotation.clone())
                .await
                .unwrap();
            assert_missing(&f.registry, "observer", json!({"id": annotation.id.0})).await;
            let actor = if note.id == f.outbound.id {
                "sender"
            } else {
                "recipient"
            };
            let found = dispatch(&f.registry, actor, "get", json!({"id": annotation.id.0}))
                .await
                .expect("own annotation edge");
            assert_eq!(found["source_id"], note.id.to_string());
        }
        let observer = dispatch(&f.registry, "observer", "get", json!({"id": parent.id.0}))
            .await
            .expect("ordinary edge");
        assert!(observer["annotations"].as_array().unwrap().is_empty());
        for (actor, own) in [("sender", &f.outbound), ("recipient", &f.inbound)] {
            let found = dispatch(&f.registry, actor, "get", json!({"id": parent.id.0}))
                .await
                .expect("own annotations");
            let annotations = found["annotations"].as_array().unwrap();
            assert_eq!(annotations.len(), 1);
            assert_eq!(annotations[0]["id"], own.id.to_string());
            assert_eq!(annotations[0]["content"], own.content);
        }
    }
}

#[tokio::test]
async fn message_search_scopes_granular_filtered_and_broad_note_reads() {
    let f = fixture(Topology::Shared).await;
    for kind in [
        json!({"kind": "message"}),
        json!({"kind": "note", "note_kind": "message"}),
        json!({"kind": "note"}),
    ] {
        let mut args = kind;
        args["query"] = json!("mailboxsearch");
        args["limit"] = json!(100);
        let observer = dispatch(&f.registry, "observer", "search", args.clone())
            .await
            .expect("observer search");
        assert!(observer.as_array().unwrap().is_empty());
        for (actor, own) in [("sender", &f.outbound), ("recipient", &f.inbound)] {
            let results = dispatch(&f.registry, actor, "search", args.clone())
                .await
                .expect("party search");
            let rows = results.as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["id"], own.id.to_string());
        }
    }
}

#[tokio::test]
async fn message_search_applies_scope_before_the_result_limit() {
    let f = fixture(Topology::Shared).await;
    let token = f.main.authorize(Namespace::local()).expect("local token");
    let store = f.main.notes(&token).expect("note store");
    for mut own in [f.outbound.clone(), f.inbound.clone()] {
        own.salience = Some(0.0);
        store.upsert_note(own).await.expect("set party salience");
    }
    dispatch(
        &f.registry,
        "correspondent",
        "comm.send",
        json!({
            "to": "observer", "content": "mailboxsearch example body",
        }),
    )
    .await
    .expect("send comparison message");
    for actor in ["sender", "recipient"] {
        let results = dispatch(
            &f.registry,
            actor,
            "search",
            json!({
                "kind": "note", "query": "mailboxsearch", "limit": 1,
            }),
        )
        .await
        .expect("limited party search");
        assert_eq!(results.as_array().unwrap().len(), 1);
        let own = if actor == "sender" {
            f.outbound.id
        } else {
            f.inbound.id
        };
        assert_eq!(results[0]["id"], own.to_string());
    }
}

#[tokio::test]
async fn message_get_prefixes_do_not_return_other_mailbox_candidates() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let token = f.comm.authorize(Namespace::local()).unwrap();
        let store = f.comm.notes(&token).unwrap();
        let mut own = f.inbound.clone();
        own.id = Uuid::parse_str("abcda001-0000-4000-8000-000000000001").unwrap();
        store.upsert_note(own.clone()).await.unwrap();
        let found = dispatch(&f.registry, "recipient", "get", json!({"id": "abcda001"}))
            .await
            .expect("own unique prefix");
        assert_eq!(found["id"], own.id.to_string());
        assert_missing(&f.registry, "observer", json!({"id": "abcda001"})).await;

        let mut candidates = Vec::new();
        for (suffix, actor) in [(1, "other"), (2, "other"), (3, "recipient")] {
            let mut note = f.inbound.clone();
            note.id = Uuid::parse_str(&format!("abcda002-0000-4000-8000-{suffix:012}")).unwrap();
            note.properties.as_mut().unwrap()["to_actor"] = json!(actor);
            store.upsert_note(note.clone()).await.unwrap();
            candidates.push(note);
        }
        for actor in ["observer", "recipient"] {
            let error = dispatch(&f.registry, actor, "get", json!({"id": "abcda002"}))
                .await
                .expect_err("message prefix requires full UUID");
            assert!(matches!(&error, RuntimeError::InvalidInput(_)));
            let rendered = error.to_string();
            assert!(rendered.contains("full UUID") && rendered.contains("comm.inbox"));
            for note in &candidates {
                assert!(!rendered.contains(&note.id.to_string()));
                assert!(!rendered.contains(&note.content));
            }
        }
        let found = dispatch(
            &f.registry,
            "recipient",
            "get",
            json!({"id": candidates[2].id}),
        )
        .await
        .expect("own full UUID remains usable");
        assert_eq!(found["id"], candidates[2].id.to_string());
    }
}
