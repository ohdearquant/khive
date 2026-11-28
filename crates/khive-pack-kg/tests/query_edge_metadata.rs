//! Stored edge metadata through the public KG query handler.

use std::collections::BTreeMap;

use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder, VerifiedActor,
};
use khive_storage::{EdgeRelation, EventFilter, PageRequest, SqlStatement};
use khive_types::EventKind;
use serde_json::{json, Value};
use uuid::Uuid;

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

fn registry(runtime: &KhiveRuntime, messages: bool) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_runtime_event_store(runtime).unwrap();
    builder.register(KgPack::new(runtime.clone()));
    if messages {
        builder.register(CommPack::new(runtime.clone()));
    }
    builder.build().unwrap()
}

async fn entity(runtime: &KhiveRuntime, token: &NamespaceToken, name: &str) -> Uuid {
    runtime
        .create_entity_with_embedding_report(
            token,
            "concept",
            None,
            name,
            None,
            Some(json!({"order": -999, "metadata": "node decoy"})),
            vec![],
        )
        .await
        .unwrap()
        .0
        .id
}

async fn observation(runtime: &KhiveRuntime, token: &NamespaceToken) -> Uuid {
    runtime
        .create_note(
            token,
            "observation",
            None,
            "Query metadata fixture",
            None,
            None,
            vec![],
        )
        .await
        .unwrap()
        .id
}

async fn domain_snapshot(runtime: &KhiveRuntime) -> Value {
    let mut snapshot = serde_json::Map::new();
    let mut reader = runtime.sql().reader().await.unwrap();
    for (name, sql) in [
        ("entities", "SELECT * FROM entities ORDER BY id"),
        ("notes", "SELECT * FROM notes ORDER BY id"),
        ("edges", "SELECT * FROM graph_edges ORDER BY id"),
        (
            "domain_events",
            "SELECT * FROM events WHERE kind <> 'audit' ORDER BY id",
        ),
    ] {
        let rows = reader
            .query_all(SqlStatement::new(sql, vec![]))
            .await
            .unwrap();
        assert!(!rows.is_empty(), "populated {name} snapshot");
        // SqlRow's tagged SQL cells retain raw TEXT, NULL and numeric representations.
        snapshot.insert(name.into(), serde_json::to_value(rows).unwrap());
    }
    snapshot.into()
}

async fn event_snapshot(runtime: &KhiveRuntime) -> BTreeMap<String, Value> {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT * FROM events ORDER BY id",
            vec![],
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.text("id").unwrap().to_owned(),
                serde_json::to_value(row).unwrap(),
            )
        })
        .collect()
}

fn ids(result: &Value) -> Vec<String> {
    result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["e_id"].as_str().unwrap().to_owned())
        .collect()
}

async fn query(registry: &VerbRegistry, source: &str, page_size: usize) -> Value {
    registry
        .dispatch("query", json!({"query": source, "page_size": page_size}))
        .await
        .unwrap_or_else(|error| panic!("{source}: {error}"))
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_edge_metadata_returns_ordered_json_without_domain_writes() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = registry(&runtime, false);
    observation(&runtime, &token).await;
    let source = entity(&runtime, &token, "ordered public source").await;
    let mut edges = BTreeMap::new();
    for order in [3, 1, 2] {
        let target = entity(&runtime, &token, &format!("ordered public target {order}")).await;
        let metadata = json!({"order": order, "qualified": {"label": format!("step {order}"), "optional": false}});
        let edge = runtime
            .link(
                &token,
                source,
                target,
                EdgeRelation::VariantOf,
                1.0,
                Some(metadata.clone()),
            )
            .await
            .unwrap();
        edges.insert(
            order,
            (
                edge.id.to_string(),
                serde_json::to_string(&metadata).unwrap(),
            ),
        );
    }
    let before = domain_snapshot(&runtime).await;
    let events = event_snapshot(&runtime).await;
    let source_query = format!("MATCH (a:concept)-[e:variant_of]->(b:concept) WHERE a.id = '{source}' AND e.metadata.qualified.optional = false RETURN e.id, e.metadata ORDER BY e.metadata.order");
    let first = query(&registry, &source_query, 2).await;
    assert_eq!(ids(&first), [edges[&1].0.clone(), edges[&2].0.clone()]);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["page_size"], 2);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["truncated"], true);
    assert_eq!(first["next_offset"], 2);
    let second = query(&registry, &format!("{source_query} SKIP 2"), 2).await;
    assert_eq!(ids(&second), [edges[&3].0.clone()]);
    assert_eq!(second["offset"], 2);
    assert_eq!(second["has_more"], false);
    assert_eq!(second["truncated"], false);
    assert!(second.get("next_offset").is_none());
    for row in first["rows"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["rows"].as_array().unwrap())
    {
        assert_eq!(
            row.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["e_id", "e_metadata"]
        );
        let expected = edges
            .values()
            .find(|(id, _)| id == row["e_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            row["e_metadata"]
                .as_str()
                .expect("whole metadata remains JSON text"),
            expected.1
        );
    }
    registry.shutdown_audit_batch().await.unwrap();
    assert_eq!(domain_snapshot(&runtime).await, before);
    let after_events = event_snapshot(&runtime).await;
    for (id, row) in events {
        assert_eq!(after_events.get(&id), Some(&row), "prior event {id}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_edge_metadata_refusals_preserve_domain_state_and_observation_queries() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = registry(&runtime, false);
    observation(&runtime, &token).await;
    let source = entity(&runtime, &token, "refusal source").await;
    let target = entity(&runtime, &token, "refusal target").await;
    let edge = runtime
        .link(
            &token,
            source,
            target,
            EdgeRelation::VariantOf,
            1.0,
            Some(json!({"order": 1})),
        )
        .await
        .unwrap();
    let events = runtime
        .list_events(
            &token,
            EventFilter {
                kinds: vec![EventKind::LinkCreated],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    let event_id = events.items[0].id;
    let observed_query =
        format!("MATCH (ev)-[e:observed_as_target]->(b) WHERE ev.id = '{event_id}' RETURN b.id");
    let observed = query(&registry, &observed_query, 20).await;
    let observed_ids = observed["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["b_id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        observed_ids,
        [source.to_string(), target.to_string()]
            .into_iter()
            .collect()
    );
    let valid = query(&registry, &format!("MATCH (a:concept)-[e:variant_of*1..2]->(b:concept) WHERE a.id = '{source}' RETURN e.metadata ORDER BY e.metadata.order"), 20).await;
    assert_eq!(valid["rows"].as_array().unwrap().len(), 1);
    assert_eq!(
        valid["rows"][0]["e_metadata"].as_str().unwrap(),
        serde_json::to_string(edge.metadata.as_ref().unwrap()).unwrap()
    );
    let before = domain_snapshot(&runtime).await;
    let history = event_snapshot(&runtime).await;
    for invalid in [
        "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a.metadata",
        "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN e.properties",
        "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN e.metadata ORDER BY e.metadata",
        "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a.properties ORDER BY a.properties",
        "MATCH (a:concept)-[e:variant_of]->(b:concept) WHERE missing.metadata.order = 1 RETURN e.id",
        "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN e.id ORDER BY e.metadata.order SIDEWAYS",
        "MATCH (a:concept)-[e:variant_of*1..2]->(b:concept) RETURN e ORDER BY e.metadata.order",
        "MATCH (a:concept)-[e:variant_of*1..2]->(b:concept) RETURN a ORDER BY a.description",
        "MATCH (a:concept)-[e:variant_of*1..2]->(b:concept) WHERE a.name = 'refusal source' OR e.metadata.order = 1 RETURN e.id",
        "MATCH (ev)-[e:observed_as_target]->(b) RETURN e.metadata",
        "MATCH (ev)-[e:observed_as_target]->(b) WHERE e.metadata.order = 1 RETURN b.id",
        "MATCH (ev)-[e:observed_as_target]->(b) RETURN b.id ORDER BY e.metadata.order",
        "MATCH (a:concept), (b:concept) RETURN a.id",
    ] {
        let error = registry.dispatch("query", json!({"query": invalid})).await.expect_err("unsupported query must refuse");
        assert!(matches!(error.refusal_source(), RuntimeError::Query(_)), "typed query refusal for {invalid}: {error}");
        assert_eq!(domain_snapshot(&runtime).await, before, "{invalid}");
    }
    assert_eq!(
        query(&registry, &observed_query, 20).await["rows"],
        observed["rows"]
    );
    registry.shutdown_audit_batch().await.unwrap();
    assert_eq!(domain_snapshot(&runtime).await, before);
    let after_events = event_snapshot(&runtime).await;
    for (id, row) in history {
        assert_eq!(after_events.get(&id), Some(&row), "prior event {id}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_edge_metadata_order_respects_message_mailbox_scope() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = registry(&runtime, true);
    // Populate both entity and note storage before taking full domain snapshots.
    entity(&runtime, &token, "mailbox query witness").await;
    let target = observation(&runtime, &token).await;
    let mut expected = BTreeMap::new();
    for (recipient, order) in [("reader", 10), ("other", -100)] {
        let sent = registry
            .dispatch_as(
                "comm.send",
                json!({
                    "to": recipient, "content": "A qualified message", "subject": "Metadata query",
                    "idempotency_key": format!("metadata-mailbox-{recipient}"),
                }),
                VerifiedActor::new("sender").unwrap(),
            )
            .await
            .unwrap();
        let outbound = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(sent["full_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        let inbound = Uuid::parse_str(
            outbound.properties.as_ref().unwrap()["inbound_ref"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let edge = runtime
            .link(
                &token,
                inbound,
                target,
                EdgeRelation::Supports,
                1.0,
                Some(json!({"order": order})),
            )
            .await
            .unwrap();
        expected.insert(recipient, edge.id.to_string());
    }
    let before = domain_snapshot(&runtime).await;
    for actor in ["reader", "other"] {
        let result = registry.dispatch_as("query", json!({
            "query": "MATCH (a:message)-[e:supports]->(b:observation) RETURN e.id, e.metadata ORDER BY e.metadata.order",
            "page_size": 1,
        }), VerifiedActor::new(actor).unwrap()).await.unwrap();
        assert_eq!(ids(&result), [expected[actor].clone()], "actor {actor}");
        assert_eq!(
            result["has_more"], false,
            "hidden rows must not become a sentinel"
        );
        assert_eq!(result["truncated"], false);
        assert!(result.get("next_offset").is_none());
    }
    registry.shutdown_audit_batch().await.unwrap();
    assert_eq!(domain_snapshot(&runtime).await, before);
}
