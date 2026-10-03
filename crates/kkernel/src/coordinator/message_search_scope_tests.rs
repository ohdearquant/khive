//! Read scoping of message notes on generic reads through the coordinator.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RequestIdentity, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{EdgeRelation, Entity, Note};
use serde_json::{json, Value};
use uuid::Uuid;

use super::super::{BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService};

#[derive(Clone, Copy)]
enum Topology {
    Shared,
    Split,
}

struct Fixture {
    main: KhiveRuntime,
    comm: KhiveRuntime,
    outbound: Note,
    inbound: Note,
}

fn runtime(backend: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        backend_id: BackendId::parse(backend).expect("backend id"),
        actor_id: Some("backend-owner".into()),
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

fn registry(f: &Fixture, actor: &str, namespace: &str, visible: &[&str]) -> VerbRegistry {
    let _ = (KgPack::new(f.main.clone()), CommPack::new(f.comm.clone()));
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs_with_runtimes(
        &["kg".into(), "comm".into()],
        &HashMap::from([
            ("kg".into(), f.main.clone()),
            ("comm".into(), f.comm.clone()),
        ]),
        &f.main,
        &mut builder,
    )
    .expect("register KG and Comm");
    builder.with_actor_id(Some(actor.into()));
    builder.with_default_namespace(namespace);
    builder.with_visible_namespaces(
        visible
            .iter()
            .map(|ns| Namespace::parse(ns).unwrap())
            .collect(),
    );
    builder.build().expect("registry builds")
}

async fn send(f: &Fixture, namespace: &str, content: &str) -> (Note, Note) {
    let registry = registry(f, "sender", "local", &[]);
    let sent = registry
        .dispatch_with_identity(
            "comm.send",
            json!({
                "to": "recipient", "namespace": namespace, "content": content,
                "idempotency_key": format!("{namespace}:{content}"),
            }),
            Some(RequestIdentity {
                namespace: "local".into(),
                actor_id: Some("sender".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("send message");
    let id = Uuid::parse_str(sent["full_id"].as_str().expect("outbound id")).expect("UUID");
    let token = f
        .comm
        .authorize(Namespace::parse(namespace).unwrap())
        .expect("storage token");
    let store = f.comm.notes(&token).expect("Comm notes");
    let outbound = store.get_note(id).await.unwrap().expect("outbound copy");
    let inbound_id = Uuid::parse_str(
        outbound.properties.as_ref().unwrap()["inbound_ref"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let inbound = store
        .get_note(inbound_id)
        .await
        .unwrap()
        .expect("inbound copy");
    (outbound, inbound)
}

async fn fixture(topology: Topology) -> Fixture {
    let main = runtime("main");
    let comm = match topology {
        Topology::Shared => main.clone(),
        Topology::Split => runtime("comm"),
    };
    let placeholder = Note::new("local", "message", "placeholder");
    let mut f = Fixture {
        main,
        comm,
        outbound: placeholder.clone(),
        inbound: placeholder,
    };
    (f.outbound, f.inbound) = send(&f, "local", "coordinatedmailbox example body").await;
    f
}

fn service(f: &Fixture) -> Arc<SubstrateCoordinatorService> {
    let mut backends = BackendRegistry::new();
    backends.register(BackendId::main(), Arc::new(f.main.clone()));
    if f.main.backend_id() != f.comm.backend_id() {
        backends.register(BackendId::parse("comm").unwrap(), Arc::new(f.comm.clone()));
    }
    Arc::new(SubstrateCoordinatorService::new(SubstrateCoordinator::new(
        backends,
    )))
}

async fn search_envelope(
    f: &Fixture,
    actor: &str,
    namespace: &str,
    visible: &[&str],
    args: Value,
) -> Value {
    let server = KhiveMcpServer::from_registry(registry(f, actor, namespace, visible))
        .with_coordinator(service(f));
    let raw = server
        .dispatch_request_local(RequestParams {
            ops: json!([{"tool": "search", "args": args}]).to_string(),
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..Default::default()
        })
        .await
        .expect("coordinated request");
    let envelope: Value = serde_json::from_str(&raw).expect("response JSON");
    envelope
}

async fn search(f: &Fixture, actor: &str, namespace: &str, visible: &[&str], args: Value) -> Value {
    let envelope = search_envelope(f, actor, namespace, visible, args).await;
    assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
    envelope["results"][0]["result"].clone()
}

fn ids(rows: &Value) -> BTreeSet<String> {
    rows.as_array()
        .expect("search rows")
        .iter()
        .map(|row| row["id"].as_str().expect("hit id").to_string())
        .collect()
}

async fn incident_edges(f: &Fixture, messages: &[&Note]) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for message in messages {
        let token = f
            .comm
            .authorize(Namespace::parse(&message.namespace).unwrap())
            .expect("message graph token");
        let anchor = Entity::new(&message.namespace, "concept", "Mailbox search graph anchor");
        f.comm
            .entities(&token)
            .expect("message graph entities")
            .upsert_entity(anchor.clone())
            .await
            .expect("store message graph anchor");
        let edge = f
            .comm
            .link(
                &token,
                message.id,
                anchor.id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .expect("link message to graph anchor");
        let stored = f
            .comm
            .graph(&token)
            .expect("message graph")
            .get_edge(edge.id)
            .await
            .expect("read message edge")
            .expect("message edge exists");
        assert_eq!(stored.source_id, message.id);
        assert_eq!(stored.target_id, anchor.id);
        ids.push(edge.id.0);
    }
    ids
}

fn assert_no_message_artifacts(envelope: &Value, messages: &[&Note], incident_edges: &[Uuid]) {
    for row in envelope["results"][0]["result"]
        .as_array()
        .expect("observer search rows")
    {
        assert_ne!(row["kind"], "message", "{envelope}");
        assert_ne!(row["note_kind"], "message", "{envelope}");
    }
    let rendered = envelope.to_string();
    for message in messages {
        assert!(!rendered.contains(&message.id.to_string()), "{envelope}");
        assert!(!rendered.contains(&message.content), "{envelope}");
    }
    for id in incident_edges {
        assert!(!rendered.contains(&id.to_string()), "{envelope}");
    }
}

#[tokio::test]
async fn coordinated_message_search_preserves_each_party_on_both_topologies() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        for kind in [
            json!({"kind": "message"}),
            json!({"kind": "note", "note_kind": "message"}),
            json!({"kind": "note"}),
        ] {
            let mut args = kind;
            args["query"] = json!("coordinatedmailbox");
            args["limit"] = json!(100);
            assert!(ids(&search(&f, "observer", "local", &[], args.clone()).await).is_empty());
            for (actor, own) in [("sender", f.outbound.id), ("recipient", f.inbound.id)] {
                assert_eq!(
                    ids(&search(&f, actor, "local", &[], args.clone()).await),
                    BTreeSet::from([own.to_string()])
                );
            }
        }
    }
}

#[tokio::test]
async fn coordinated_message_scope_runs_before_ordering_and_limits() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let registry = registry(&f, "correspondent", "local", &[]);
        registry
            .dispatch(
                "comm.send",
                json!({"to": "observer", "content": "coordinatedmailbox example body"}),
            )
            .await
            .expect("comparison send");
        let token = f.comm.authorize(Namespace::local()).unwrap();
        for mut note in [f.outbound.clone(), f.inbound.clone()] {
            note.salience = Some(0.0);
            f.comm
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
        }
        for order in ["score", "created_at", "updated_at"] {
            for (actor, own) in [("sender", f.outbound.id), ("recipient", f.inbound.id)] {
                let rows = search(&f, actor, "local", &[], json!({
                    "kind": "note", "query": "coordinatedmailbox", "limit": 1, "order_by": order,
                })).await;
                assert_eq!(ids(&rows), BTreeSet::from([own.to_string()]));
            }
        }
    }
}

#[tokio::test]
async fn coordinated_message_search_preserves_default_and_explicit_namespace_selection() {
    for topology in [Topology::Shared, Topology::Split] {
        let f = fixture(topology).await;
        let (primary, primary_inbound) =
            send(&f, "tenant-primary", "coordinatedmailbox primary").await;
        let (visible, visible_inbound) =
            send(&f, "tenant-visible", "coordinatedmailbox visible").await;
        let (actor_namespace, actor_namespace_inbound) =
            send(&f, "sender", "coordinatedmailbox actor namespace").await;
        let messages = [
            &f.outbound,
            &f.inbound,
            &primary,
            &primary_inbound,
            &visible,
            &visible_inbound,
            &actor_namespace,
            &actor_namespace_inbound,
        ];
        let edges = incident_edges(&f, &messages).await;
        let reg = registry(&f, "sender", "local", &[]);
        let mut ordinary = HashMap::new();
        for namespace in ["local", "tenant-primary", "tenant-visible", "sender"] {
            let created = reg
                .dispatch(
                    "create",
                    json!({
                        "kind": "observation", "namespace": namespace,
                        "content": "coordinatedmailbox ordinary example",
                    }),
                )
                .await
                .expect("ordinary namespace comparison note");
            ordinary.insert(namespace, created["id"].as_str().unwrap().to_string());
        }
        // A shared backend falls through to registry dispatch; the
        // coordinator's existing primary-namespace selection applies to split backends.
        let (selected_message, selected_namespace, excluded_message, excluded_namespace) =
            match topology {
                Topology::Shared => (&actor_namespace, "sender", &primary, "tenant-primary"),
                Topology::Split => (&primary, "tenant-primary", &actor_namespace, "sender"),
            };
        let expected_messages = BTreeSet::from([
            f.outbound.id.to_string(),
            visible.id.to_string(),
            selected_message.id.to_string(),
        ]);
        let expected_ordinary = BTreeSet::from([
            ordinary["local"].clone(),
            ordinary["tenant-visible"].clone(),
            ordinary[selected_namespace].clone(),
        ]);
        let observer_ordinary = match topology {
            Topology::Shared => BTreeSet::from([
                ordinary["local"].clone(),
                ordinary["tenant-visible"].clone(),
            ]),
            Topology::Split => expected_ordinary.clone(),
        };
        for kind in [
            json!({"kind": "message"}),
            json!({"kind": "note", "note_kind": "message"}),
            json!({"kind": "observation"}),
            json!({"kind": "note"}),
        ] {
            let mut args = kind.clone();
            args["query"] = json!("coordinatedmailbox");
            args["limit"] = json!(100);
            let expected = match kind["kind"].as_str().unwrap() {
                "observation" => expected_ordinary.clone(),
                "note" if kind.get("note_kind").is_none() => expected_messages
                    .union(&expected_ordinary)
                    .cloned()
                    .collect(),
                _ => expected_messages.clone(),
            };
            let rows = search(
                &f,
                "sender",
                "tenant-primary",
                &["tenant-visible"],
                args.clone(),
            )
            .await;
            let actual = ids(&rows);
            assert_eq!(actual, expected);
            assert!(!actual.contains(&excluded_message.id.to_string()));
            assert!(!actual.contains(&ordinary[excluded_namespace]));
            let observed = search_envelope(
                &f,
                "observer",
                "tenant-primary",
                &["tenant-visible"],
                args.clone(),
            )
            .await;
            let observed_result = &observed["results"][0];
            assert_eq!(observed_result["ok"], true, "{observed}");
            assert_eq!(observed_result["status"], "complete", "{observed}");
            let expected_observer = match kind["kind"].as_str().unwrap() {
                "observation" => observer_ordinary.clone(),
                "note" if kind.get("note_kind").is_none() => observer_ordinary.clone(),
                _ => BTreeSet::new(),
            };
            assert_eq!(ids(&observed_result["result"]), expected_observer);
            assert_no_message_artifacts(&observed, &messages, &edges);
            args["namespace"] = json!("tenant-visible");
            let rows = search(
                &f,
                "sender",
                "tenant-primary",
                &["tenant-visible"],
                args.clone(),
            )
            .await;
            let expected = match kind["kind"].as_str().unwrap() {
                "observation" => BTreeSet::from([ordinary["tenant-visible"].clone()]),
                "note" if kind.get("note_kind").is_none() => {
                    BTreeSet::from([visible.id.to_string(), ordinary["tenant-visible"].clone()])
                }
                _ => BTreeSet::from([visible.id.to_string()]),
            };
            assert_eq!(ids(&rows), expected);
            let observed =
                search_envelope(&f, "observer", "tenant-primary", &["tenant-visible"], args).await;
            let observed_result = &observed["results"][0];
            assert_eq!(observed_result["ok"], true, "{observed}");
            assert_eq!(observed_result["status"], "complete", "{observed}");
            let expected_observer = match kind["kind"].as_str().unwrap() {
                "observation" => BTreeSet::from([ordinary["tenant-visible"].clone()]),
                "note" if kind.get("note_kind").is_none() => {
                    BTreeSet::from([ordinary["tenant-visible"].clone()])
                }
                _ => BTreeSet::new(),
            };
            assert_eq!(ids(&observed_result["result"]), expected_observer);
            assert_no_message_artifacts(&observed, &messages, &edges);
        }
    }
}

#[derive(Debug)]
struct DenyExtraNamespace;

impl khive_runtime::Gate for DenyExtraNamespace {
    fn check(
        &self,
        request: &khive_runtime::GateRequest,
    ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
        if request.verb == "authorize.visible" && request.namespace.as_str() == "denied-extra" {
            return Ok(khive_runtime::GateDecision::deny(
                "extra namespace read is not admitted",
            ));
        }
        Ok(khive_runtime::GateDecision::allow())
    }
}

fn runtime_denies_extra(backend: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        backend_id: BackendId::parse(backend).expect("backend id"),
        actor_id: Some("backend-owner".into()),
        gate: Arc::new(DenyExtraNamespace),
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

#[tokio::test]
async fn coordinated_note_search_retains_backend_visible_admission() {
    for topology in [Topology::Shared, Topology::Split] {
        let main = match topology {
            Topology::Shared => runtime_denies_extra("main"),
            Topology::Split => runtime("main"),
        };
        let comm = match topology {
            Topology::Shared => main.clone(),
            Topology::Split => runtime_denies_extra("comm"),
        };
        let placeholder = Note::new("local", "message", "placeholder");
        let mut f = Fixture {
            main,
            comm,
            outbound: placeholder.clone(),
            inbound: placeholder,
        };
        (f.outbound, f.inbound) = send(&f, "local", "coordinatedmailbox example body").await;
        let messages = [&f.outbound, &f.inbound];
        let edges = incident_edges(&f, &messages).await;
        let reg = registry(&f, "sender", "local", &[]);
        let ordinary = reg
            .dispatch(
                "create",
                json!({
                    "kind": "observation", "content": "coordinatedmailbox ordinary example",
                }),
            )
            .await
            .expect("ordinary comparison note");
        let args = json!({"kind": "note", "query": "coordinatedmailbox", "limit": 100});
        let admitted = search(&f, "sender", "local", &[], args.clone()).await;
        assert_eq!(
            ids(&admitted),
            BTreeSet::from([
                f.outbound.id.to_string(),
                ordinary["id"].as_str().unwrap().to_string(),
            ])
        );
        let observed = search_envelope(&f, "observer", "local", &[], args.clone()).await;
        let observed_result = &observed["results"][0];
        assert_eq!(observed_result["ok"], true, "{observed}");
        assert_eq!(observed_result["status"], "complete", "{observed}");
        assert_eq!(
            ids(&observed_result["result"]),
            BTreeSet::from([ordinary["id"].as_str().unwrap().to_string()])
        );
        assert_no_message_artifacts(&observed, &messages, &edges);
        let envelope =
            search_envelope(&f, "sender", "local", &["denied-extra"], args.clone()).await;
        let result = &envelope["results"][0];
        match topology {
            Topology::Shared => {
                assert_eq!(result["ok"], true, "{envelope}");
                assert_eq!(result["status"], "complete", "{envelope}");
                assert_eq!(
                    ids(&result["result"]),
                    BTreeSet::from([
                        f.outbound.id.to_string(),
                        ordinary["id"].as_str().unwrap().to_string(),
                    ])
                );
                assert!(result.get("missing_backends").is_none(), "{envelope}");
            }
            Topology::Split => {
                assert_eq!(result["ok"], true, "{envelope}");
                assert_eq!(result["status"], "partial", "{envelope}");
                assert_eq!(
                    ids(&result["result"]),
                    BTreeSet::from([ordinary["id"].as_str().unwrap().to_string()])
                );
                assert_eq!(result["missing_backends"], json!(["comm"]), "{envelope}");
                let rendered = envelope.to_string();
                assert!(!rendered.contains(&f.outbound.id.to_string()));
                assert!(!rendered.contains(&f.outbound.content));
            }
        }
        let rendered = envelope.to_string();
        assert!(!rendered.contains(&f.inbound.id.to_string()));
        let observed = search_envelope(&f, "observer", "local", &["denied-extra"], args).await;
        let observed_result = &observed["results"][0];
        assert_eq!(observed_result["ok"], true, "{observed}");
        assert_eq!(
            ids(&observed_result["result"]),
            BTreeSet::from([ordinary["id"].as_str().unwrap().to_string()])
        );
        match topology {
            Topology::Shared => {
                assert_eq!(observed_result["status"], "complete", "{observed}");
                assert!(
                    observed_result.get("missing_backends").is_none(),
                    "{observed}"
                );
            }
            Topology::Split => {
                assert_eq!(observed_result["status"], "partial", "{observed}");
                assert_eq!(
                    observed_result["missing_backends"],
                    json!(["comm"]),
                    "{observed}"
                );
            }
        }
        assert_no_message_artifacts(&observed, &messages, &edges);
    }
}
