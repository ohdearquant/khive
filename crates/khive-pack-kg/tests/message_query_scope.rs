//! Query admission uses the same mailbox partition as generic note reads.

use std::sync::{Arc, Mutex};

use chrono::Utc;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_query::CompileOptions;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, MailboxView, Namespace, PackRuntime,
    RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder, VerifiedActor,
};
use khive_storage::event::Event;
use khive_storage::note::NoteFilter;
use khive_storage::types::{Edge, LinkId, SqlRow, SqlValue};
use khive_storage::{EdgeRelation, Entity, Note};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

fn runtime(actor: Option<&str>) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        actor_id: actor.map(str::to_owned),
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

fn registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.build().expect("shared KG and Comm registry")
}

fn id(value: u128) -> Uuid {
    Uuid::from_u128(0xaabb_0000_0000_4000_8000_0000_0000_0000 + value)
}

fn note(index: u128, kind: &str, properties: Option<Value>) -> Note {
    let mut note = Note::new("local", kind, format!("fixture content {index}"));
    note.id = id(index);
    note.properties = properties;
    note
}

async fn seed(rt: &KhiveRuntime, notes: &[Note]) {
    let token = rt.authorize(Namespace::local()).expect("seed token");
    rt.notes(&token)
        .unwrap()
        .upsert_notes(notes.to_vec())
        .await
        .expect("seed notes");
}

// SPARQL requires an edge; observation targets also expose the stored note body.
async fn seed_query_observations(rt: &KhiveRuntime, note_ids: &[Uuid]) {
    let token = rt.authorize(Namespace::local()).expect("observation token");
    let event = Event::new(
        "local",
        "memory.recall",
        EventKind::RecallExecuted,
        SubstrateKind::Note,
        "fixture",
    )
    .with_payload(json!({"selected": note_ids}));
    rt.events(&token)
        .unwrap()
        .append_event(event)
        .await
        .expect("seed query observations");
}

async fn query(registry: &VerbRegistry, actor: &str, source: &str, page_size: usize) -> Value {
    registry
        .dispatch_as(
            "query",
            json!({"query": source, "page_size": page_size}),
            VerifiedActor::new(actor).expect("actor"),
        )
        .await
        .unwrap_or_else(|error| panic!("query as {actor}: {source}: {error}"))
}

fn page_ids(page: &Value, column: &str) -> Vec<String> {
    page["rows"]
        .as_array()
        .expect("query rows")
        .iter()
        .map(|row| row[column].as_str().expect("UUID column").to_owned())
        .collect()
}

fn raw_ids(rows: &[SqlRow], column: &str) -> Vec<String> {
    rows.iter()
        .map(|row| match row.get(column).expect("column exists") {
            SqlValue::Text(id) => id.clone(),
            SqlValue::Uuid(id) => id.to_string(),
            other => panic!("unexpected UUID value {other:?}"),
        })
        .collect()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_preserves_only_each_actors_message_and_quarantine_copies() {
    let rt = runtime(Some("observer"));
    let registry = registry(&rt);
    let sent = registry
        .dispatch_as(
            "comm.send",
            json!({
                "to": "recipient", "content": "private message body", "subject": "Query scope",
                "idempotency_key": "query-mailbox-copy-fixture",
            }),
            VerifiedActor::new("sender").unwrap(),
        )
        .await
        .expect("real message pair");
    let outbound_id = Uuid::parse_str(sent["full_id"].as_str().unwrap()).unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let outbound = rt
        .notes(&token)
        .unwrap()
        .get_note(outbound_id)
        .await
        .unwrap()
        .unwrap();
    let inbound_id = outbound.properties.as_ref().unwrap()["inbound_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let quarantine = note(
        3,
        "message",
        Some(json!({
            "direction": "inbound", "to_actor": "maintainer",
            "quarantine_content_ref": "private-quarantine-reference"
        })),
    );
    let ordinary = note(4, "observation", None);
    seed(&rt, &[quarantine.clone(), ordinary.clone()]).await;
    seed_query_observations(
        &rt,
        &[
            outbound_id,
            Uuid::parse_str(&inbound_id).expect("inbound UUID"),
            quarantine.id,
            ordinary.id,
        ],
    )
    .await;

    for (actor, own) in [
        ("sender", Some(outbound_id.to_string())),
        ("recipient", Some(inbound_id)),
        ("maintainer", Some(quarantine.id.to_string())),
        ("observer", None),
    ] {
        for (source, broad, projects_content) in [
            ("MATCH (n:message) RETURN n.id, n.properties", false, false),
            ("MATCH (n:note) RETURN n.id, n.properties", true, false),
            (
                "MATCH (ev:event)-[:observed_as_selected]->(n:message) RETURN n.id, n.content, n.properties",
                false,
                true,
            ),
            (
                "MATCH (ev:event)-[:observed_as_selected]->(n:note) RETURN n.id, n.content, n.properties",
                true,
                true,
            ),
            (
                "SELECT ?n WHERE { ?ev a :event . ?ev :observed_as_selected ?n . ?n a :message }",
                false,
                true,
            ),
            (
                "SELECT ?n WHERE { ?ev a :event . ?ev :observed_as_selected ?n . ?n a :note }",
                true,
                true,
            ),
        ] {
            let page = query(&registry, actor, source, 100).await;
            let mut actual = page_ids(&page, "n_id");
            let mut expected: Vec<_> = own.iter().cloned().collect();
            if broad {
                expected.push(ordinary.id.to_string());
            }
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{actor}: {source}");
            assert_eq!(page["has_more"], false);
            let body = page.to_string();
            if actor != "maintainer" {
                assert!(!body.contains("private-quarantine-reference"));
                assert!(!body.contains(&quarantine.content));
            } else {
                assert!(body.contains("private-quarantine-reference"));
                if projects_content {
                    assert!(body.contains(&quarantine.content));
                }
            }
            if own.is_none() {
                assert!(!body.contains("private message body"));
            }
        }
        let ordinary_page = query(
            &registry,
            actor,
            "MATCH (ev:event)-[:observed_as_selected]->(n:observation) RETURN n.id, n.content",
            10,
        )
        .await;
        assert_eq!(page_ids(&ordinary_page, "n_id"), [ordinary.id.to_string()]);
        assert_eq!(ordinary_page["rows"][0]["n_content"], ordinary.content);
    }

    for source in [
        "MATCH (n:note) RETURN n.id",
        "SELECT ?n WHERE { ?ev a :event . ?ev :observed_as_selected ?n . ?n a :note }",
    ] {
        let rows = rt.query(&token, source).await.expect("direct query");
        assert_eq!(raw_ids(&rows, "n_id"), [ordinary.id.to_string()]);
        let page = rt
            .query_with_metadata(&token, source, CompileOptions::default())
            .await
            .unwrap();
        assert_eq!(raw_ids(&page.rows, "n_id"), [ordinary.id.to_string()]);
        assert!(!page.has_more);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_mailbox_matches_rust_and_note_store_for_json_routing_shapes() {
    let values = [
        None,
        Some(Value::Null),
        Some(json!(true)),
        Some(json!(7)),
        Some(json!(["x"])),
        Some(json!({"k":"v"})),
        Some(json!("reader")),
        Some(json!("local")),
        Some(json!(r#"["x"]"#)),
        Some(json!(r#"{"k":"v"}"#)),
        Some(json!("reader' OR 1=1 --")),
    ];
    let directions = [
        None,
        Some(Value::Null),
        Some(json!(true)),
        Some(json!(7)),
        Some(json!(["inbound"])),
        Some(json!({"direction":"inbound"})),
        Some(json!("inbound")),
        Some(json!("outbound")),
        Some(json!("unknown")),
    ];
    let mut notes = vec![note(1, "observation", None), note(2, "message", None)];
    for direction in &directions {
        for route in &values {
            let mut properties = json!({});
            if let Some(direction) = direction {
                properties["direction"] = direction.clone();
            }
            if let Some(route) = route {
                properties["to_actor"] = route.clone();
                properties["from_actor"] = route.clone();
            }
            notes.push(note(notes.len() as u128 + 1, "message", Some(properties)));
        }
    }
    for actor in [
        None,
        Some("local"),
        Some("reader"),
        Some(r#"["x"]"#),
        Some(r#"{"k":"v"}"#),
        Some("reader' OR 1=1 --"),
    ] {
        let rt = runtime(actor);
        seed(&rt, &notes).await;
        let note_ids: Vec<_> = notes.iter().map(|note| note.id).collect();
        seed_query_observations(&rt, &note_ids).await;
        let token = rt.authorize(Namespace::local()).unwrap();
        assert_eq!(token.actor().is_anonymous(), actor.is_none());
        let scope = MailboxView {
            actor_id: token.actor().id.clone(),
            delegated: false,
            maintainer_actor: rt.config().actor_id.clone().filter(|id| !id.is_empty()),
        }
        .note_scope(&token);
        let mut expected: Vec<_> = notes
            .iter()
            .filter(|note| MailboxView::scope_permits_message_note(&scope, note))
            .map(|note| note.id.to_string())
            .collect();
        expected.sort();
        let filter = NoteFilter {
            mailbox: Some(scope),
            ..Default::default()
        };
        let mut stored: Vec<_> = rt
            .list_notes_filtered(&token, filter, 500, 0)
            .await
            .unwrap()
            .iter()
            .map(|note| note.id.to_string())
            .collect();
        stored.sort();
        assert_eq!(stored, expected, "note-store parity for {actor:?}");
        for source in [
            "MATCH (n:note) RETURN n.id",
            "SELECT ?n WHERE { ?ev a :event . ?ev :observed_as_selected ?n . ?n a :note }",
        ] {
            let page = rt
                .query_with_metadata(
                    &token,
                    source,
                    CompileOptions {
                        max_limit: 500,
                        scopes: vec!["caller-cannot-override".into()],
                    },
                )
                .await
                .unwrap();
            let mut actual = raw_ids(&page.rows, "n_id");
            actual.sort();
            assert_eq!(actual, expected, "query parity for {actor:?}: {source}");
            assert!(!page.has_more);
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn hidden_notes_do_not_consume_query_pages_or_continuation_sentinels() {
    let rt = runtime(Some("reader"));
    let registry = registry(&rt);
    let mut notes = Vec::new();
    for index in 1..=12 {
        notes.push(note(
            index,
            "message",
            Some(json!({"direction":"inbound",
            "to_actor": if index % 3 == 0 && index < 12 {"reader"} else {"other"}})),
        ));
    }
    seed(&rt, &notes).await;
    let source = "MATCH (n:note) RETURN n.id";
    let whole = query(&registry, "reader", source, 100).await;
    let expected = vec![id(3).to_string(), id(6).to_string(), id(9).to_string()];
    assert_eq!(page_ids(&whole, "n_id"), expected);
    for page_size in [1, 2, 3] {
        let mut offset = 0;
        let mut seen = Vec::new();
        for _ in 0..=expected.len() {
            let page = query(
                &registry,
                "reader",
                &format!("{source} SKIP {offset}"),
                page_size,
            )
            .await;
            let ids = page_ids(&page, "n_id");
            assert_eq!(page["offset"], offset);
            assert_eq!(page["truncated"], page["has_more"]);
            seen.extend(ids.iter().cloned());
            if page["has_more"] == false {
                assert!(page.get("next_offset").is_none());
                break;
            }
            assert_eq!(ids.len(), page_size, "hidden rows must not shorten pages");
            let next = page["next_offset"].as_u64().unwrap() as usize;
            assert_eq!(next, offset + ids.len());
            offset = next;
        }
        assert_eq!(seen, expected);
    }
    let hidden = query(&registry, "nobody", source, 1).await;
    assert!(page_ids(&hidden, "n_id").is_empty());
    assert_eq!(hidden["has_more"], false);
    assert!(hidden.get("next_offset").is_none());
}

async fn edge(rt: &KhiveRuntime, index: u128, source: Uuid, target: Uuid) {
    let token = rt.authorize(Namespace::local()).unwrap();
    let now = Utc::now();
    rt.graph(&token)
        .unwrap()
        .upsert_edge(Edge {
            id: LinkId(id(index)),
            namespace: "local".into(),
            source_id: source,
            target_id: target,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn graph_and_observation_queries_cannot_bypass_message_scope() {
    let rt = runtime(Some("observer"));
    let registry = registry(&rt);
    let token = rt.authorize(Namespace::local()).unwrap();
    let visible = note(1, "observation", None);
    let hidden = note(
        2,
        "message",
        Some(json!({"direction":"inbound","to_actor":"owner"})),
    );
    let malformed = note(3, "message", None);
    let mut tombstone = note(
        4,
        "message",
        Some(json!({"direction":"outbound","from_actor":"owner"})),
    );
    tombstone.deleted_at = Some(tombstone.created_at);
    seed(
        &rt,
        &[
            visible.clone(),
            hidden.clone(),
            malformed.clone(),
            tombstone.clone(),
        ],
    )
    .await;
    let mut target = Entity::new("local", "concept", "ordinary target");
    target.id = id(10);
    rt.entities(&token)
        .unwrap()
        .upsert_entity(target.clone())
        .await
        .unwrap();
    edge(&rt, 21, visible.id, target.id).await;
    edge(&rt, 22, hidden.id, target.id).await;
    edge(&rt, 23, malformed.id, target.id).await;
    edge(&rt, 24, tombstone.id, target.id).await;
    edge(&rt, 25, target.id, hidden.id).await;
    let mut unreachable = Entity::new("local", "concept", "behind hidden message");
    unreachable.id = id(11);
    rt.entities(&token)
        .unwrap()
        .upsert_entity(unreachable.clone())
        .await
        .unwrap();
    edge(&rt, 26, hidden.id, unreachable.id).await;
    let event = Event::new(
        "local",
        "memory.recall",
        EventKind::RecallExecuted,
        SubstrateKind::Note,
        "fixture",
    )
    .with_payload(json!({"selected": [visible.id, hidden.id, malformed.id]}));
    rt.events(&token)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();

    let direct = query(&registry, "observer", "MATCH (e:edge) RETURN e.id", 100).await;
    assert_eq!(page_ids(&direct, "e_id"), [id(21).to_string()]);
    let fixed = query(
        &registry,
        "observer",
        "MATCH (n:note)-[e:annotates]->(t:entity) RETURN e.id",
        100,
    )
    .await;
    assert_eq!(page_ids(&fixed, "e_id"), [id(21).to_string()]);
    let observed = query(
        &registry,
        "observer",
        "MATCH (ev:event)-[:observed_as_selected]->(n:note) RETURN n.id",
        100,
    )
    .await;
    assert_eq!(page_ids(&observed, "n_id"), [visible.id.to_string()]);
    let recursive = query(
        &registry,
        "observer",
        &format!(
            "MATCH (s:entity)-[:annotates*1..3]->(t:entity) WHERE s.id = '{}' RETURN t.id",
            target.id
        ),
        100,
    )
    .await;
    assert!(
        page_ids(&recursive, "t_id").is_empty(),
        "hidden intermediate cannot expose its neighbor"
    );
    let positive = query(
        &registry,
        "owner",
        &format!(
            "MATCH (s:entity)-[:annotates*1..3]->(t:entity) WHERE s.id = '{}' RETURN t.id",
            target.id
        ),
        100,
    )
    .await;
    assert!(page_ids(&positive, "t_id").contains(&unreachable.id.to_string()));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_endpoint_decisions_match_generic_get_with_colliding_and_deleted_records() {
    let rt = runtime(Some("observer"));
    let registry = registry(&rt);
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut target = Entity::new("local", "concept", "edge target");
    target.id = id(100);
    rt.entities(&token)
        .unwrap()
        .upsert_entity(target.clone())
        .await
        .unwrap();
    let cases = [
        (1, Some(false), Some("owner"), false, true),
        (2, Some(true), Some("owner"), false, true),
        (3, None, Some("owner"), false, false),
        (4, None, Some("observer"), false, true),
        (5, None, None, false, false),
        (6, None, Some("owner"), true, false),
        (7, None, Some("observer"), true, true),
    ];
    let mut expected_edges = Vec::new();
    for (index, entity_deleted, route, note_deleted, allowed) in cases {
        let mut message = note(
            index,
            "message",
            route.map(|actor| json!({"direction":"inbound", "to_actor":actor})),
        );
        if note_deleted {
            message.deleted_at = Some(message.created_at);
        }
        seed(&rt, std::slice::from_ref(&message)).await;
        if let Some(deleted) = entity_deleted {
            let mut entity = Entity::new("local", "concept", "same UUID entity");
            entity.id = message.id;
            if deleted {
                entity.deleted_at = Some(entity.created_at);
            }
            rt.entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
        }
        edge(&rt, 200 + index, message.id, target.id).await;
        let generic = registry
            .dispatch_as(
                "get",
                json!({"id":id(200 + index)}),
                VerifiedActor::new("observer").unwrap(),
            )
            .await;
        if allowed {
            let record = generic.expect("generic endpoint precedence permits this edge");
            assert_eq!(record["id"], id(200 + index).to_string());
            expected_edges.push(id(200 + index).to_string());
        } else {
            assert!(
                matches!(generic, Err(RuntimeError::NotFound(_))),
                "generic message endpoint must be withheld: {index}: {generic:?}"
            );
        }
    }
    let ordinary = note(8, "observation", None);
    seed(&rt, std::slice::from_ref(&ordinary)).await;
    for index in [8, 9] {
        edge(&rt, 200 + index, id(index), target.id).await;
        registry
            .dispatch_as(
                "get",
                json!({"id":id(200 + index)}),
                VerifiedActor::new("observer").unwrap(),
            )
            .await
            .expect("non-message endpoint unchanged");
        expected_edges.push(id(200 + index).to_string());
    }
    let page = query(&registry, "observer", "MATCH (e:edge) RETURN e.id", 100).await;
    assert_eq!(page_ids(&page, "e_id"), expected_edges);
    let notes = query(&registry, "observer", "MATCH (n:note) RETURN n.id", 100).await;
    assert_eq!(
        page_ids(&notes, "n_id"),
        [id(4).to_string(), id(8).to_string()],
        "entity precedence never admits the colliding hidden message row"
    );
}

#[derive(Debug)]
struct QueryGate {
    seen: Arc<Mutex<Vec<GateRequest>>>,
    deny: bool,
}

impl Gate for QueryGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "query" {
            self.seen.lock().unwrap().push(request.clone());
            if self.deny {
                return Ok(GateDecision::deny("query denied"));
            }
        }
        Ok(GateDecision::allow())
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn direct_query_handler_preserves_gate_identity_arguments_and_denial() {
    for deny in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let rt = KhiveRuntime::new(RuntimeConfig {
            actor_id: Some("observer".into()),
            db_path: None,
            gate: Arc::new(QueryGate {
                seen: seen.clone(),
                deny,
            }),
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let registry = registry(&rt);
        let ordinary = note(1, "observation", None);
        seed(&rt, std::slice::from_ref(&ordinary)).await;
        let token = rt.authorize(Namespace::local()).unwrap();
        let args = json!({"query":"MATCH (n:note) RETURN n.id", "limit":7});
        let result = KgPack::new(rt.clone())
            .dispatch("query", args.clone(), &registry, &token)
            .await;
        if deny {
            assert!(matches!(result, Err(RuntimeError::PermissionDenied { .. })));
        } else {
            assert_eq!(
                page_ids(&result.unwrap(), "n_id"),
                [ordinary.id.to_string()]
            );
        }
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "direct handler checks the ordinary query gate"
        );
        assert_eq!(seen[0].actor, *token.actor());
        assert_eq!(seen[0].namespace, *token.gate_namespace());
        assert_eq!(seen[0].args, args);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_arguments_cannot_select_another_mailbox() {
    let rt = runtime(Some("observer"));
    let registry = registry(&rt);
    seed(
        &rt,
        &[note(
            1,
            "message",
            Some(json!({"direction":"inbound","to_actor":"owner"})),
        )],
    )
    .await;
    for selector in ["actor", "mailbox_actor"] {
        let mut args = json!({"query":"MATCH (n:message) RETURN n.id"});
        args[selector] = json!("owner");
        let result = registry
            .dispatch_as("query", args, VerifiedActor::new("observer").unwrap())
            .await;
        assert!(matches!(result, Err(RuntimeError::InvalidInput(_))));
    }
    let page = query(
        &registry,
        "observer",
        "MATCH (n:message) WHERE n.properties.to_actor = 'owner' RETURN n.id",
        10,
    )
    .await;
    assert!(page_ids(&page, "n_id").is_empty());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn runtime_query_cannot_override_mailbox_or_namespace_scopes() {
    let rt = runtime(Some("reader"));
    let local = note(
        1,
        "message",
        Some(json!({"direction":"inbound","to_actor":"reader"})),
    );
    let mut foreign = note(2, "message", local.properties.clone());
    foreign.namespace = "project".into();
    let mut other = note(
        3,
        "message",
        Some(json!({"direction":"inbound","to_actor":"owner"})),
    );
    other.namespace = "project".into();
    for note in [&local, &foreign, &other] {
        let token = rt
            .authorize(Namespace::parse(&note.namespace).unwrap())
            .unwrap();
        rt.notes(&token)
            .unwrap()
            .upsert_note(note.clone())
            .await
            .unwrap();
    }
    for (extra, expected) in [
        (false, vec![local.id.to_string()]),
        (true, vec![local.id.to_string(), foreign.id.to_string()]),
    ] {
        let token = if extra {
            rt.authorize_with_visibility(
                Namespace::local(),
                vec![Namespace::parse("project").unwrap()],
            )
            .unwrap()
        } else {
            rt.authorize(Namespace::local()).unwrap()
        };
        let page = rt
            .query_with_metadata(
                &token,
                "MATCH (n:message) RETURN n.id",
                CompileOptions {
                    scopes: vec!["project".into()],
                    max_limit: 20,
                },
            )
            .await
            .unwrap();
        assert_eq!(raw_ids(&page.rows, "n_id"), expected);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn am6_runtime_query_binds_fixed_maintainer_for_every_actual_caller() {
    let rt = runtime(Some("lambda:maintainer"));
    let registry = registry(&rt);
    let fixtures = vec![
        note(
            900,
            "message",
            Some(json!({"direction":"inbound","to_actor":"lambda:landing",
            "quarantined":true,"quarantine_content_ref":"am6-bool-private"})),
        ),
        note(
            901,
            "message",
            Some(json!({"direction":"outbound","from_actor":"lambda:landing",
            "quarantined":"true","quarantine_content_ref":"am6-string-private"})),
        ),
        note(
            902,
            "message",
            Some(json!({"to_actor":17,"from_actor":false,
            "quarantined":true,"quarantine_content_ref":"am6-unrouted-private"})),
        ),
        note(
            903,
            "message",
            Some(json!({"direction":"inbound","to_actor":"lambda:landing",
            "quarantined":1})),
        ),
        note(904, "observation", Some(json!({"quarantined":true}))),
        note(
            905,
            "message",
            Some(json!({"direction":"inbound","to_actor":"lambda:maintainer"})),
        ),
    ];
    // These rows predate read admission, including transport-owned quarantine
    // properties that the public runtime write API correctly refuses to seed.
    rt.backend()
        .notes()
        .expect("trusted preexisting notes fixture")
        .upsert_notes(fixtures.clone())
        .await
        .expect("seed actual stored quarantine rows");
    seed_query_observations(
        &rt,
        &fixtures.iter().map(|note| note.id).collect::<Vec<_>>(),
    )
    .await;
    for actor in [
        Some("lambda:maintainer"),
        Some("lambda:landing"),
        Some("lambda:other"),
        None,
    ] {
        let indexes: &[usize] = match actor {
            Some("lambda:maintainer") => &[0, 1, 2, 4, 5],
            Some("lambda:landing") => &[3, 4],
            _ => &[4],
        };
        let mut expected: Vec<_> = indexes
            .iter()
            .map(|index| fixtures[*index].id.to_string())
            .collect();
        expected.sort();
        for source in [
            "MATCH (n:note) RETURN n.id, n.properties",
            "MATCH (ev:event)-[:observed_as_selected]->(n:note) RETURN n.id, n.content, n.properties",
            "SELECT ?n WHERE { ?ev a :event . ?ev :observed_as_selected ?n . ?n a :note }",
        ] {
            let response = registry.dispatch_with_identity("query", json!({"query":source,"page_size":100}),
                Some(khive_runtime::RequestIdentity {
                    namespace:"local".into(), actor_id:actor.map(str::to_owned), ..Default::default()
                }),
            ).await.unwrap_or_else(|error| panic!("query {actor:?}: {source}: {error}"));
            let mut actual = page_ids(&response, "n_id");
            actual.sort();
            assert_eq!(actual, expected, "fixed M, actor={actor:?}, source={source}");
            if actor != Some("lambda:maintainer") {
                assert!(!response.to_string().contains("am6-bool-private"));
                assert!(!response.to_string().contains("am6-string-private"));
                assert!(!response.to_string().contains("am6-unrouted-private"));
            }
        }
    }
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut direct = raw_ids(
        &rt.query(&token, "MATCH (n:note) RETURN n.id")
            .await
            .unwrap(),
        "n_id",
    );
    direct.sort();
    let mut expected: Vec<_> = [0, 1, 2, 4, 5]
        .iter()
        .map(|index| fixtures[*index].id.to_string())
        .collect();
    expected.sort();
    assert_eq!(
        direct, expected,
        "direct runtime query also carries configured M"
    );
}
