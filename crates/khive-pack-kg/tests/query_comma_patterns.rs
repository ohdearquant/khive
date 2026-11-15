//! Connected graph patterns through the public KG query handler.

use std::collections::{BTreeMap, BTreeSet};

use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_query::QueryError;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder, VerifiedActor,
};
use khive_storage::{
    Edge, EdgeRelation, Entity, EventFilter, PageRequest, SqlRow, SqlStatement, SqlValue,
};
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

async fn entity(runtime: &KhiveRuntime, token: &NamespaceToken, name: &str) -> Entity {
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
}

async fn observation(runtime: &KhiveRuntime, token: &NamespaceToken) -> Uuid {
    runtime
        .create_note(
            token,
            "observation",
            None,
            "Comma query fixture",
            None,
            None,
            vec![],
        )
        .await
        .unwrap()
        .id
}

async fn link(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source: Uuid,
    target: Uuid,
    relation: EdgeRelation,
    metadata: Value,
) -> Edge {
    runtime
        .link(token, source, target, relation, 1.0, Some(metadata))
        .await
        .unwrap()
}

async fn stored_rows(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> Vec<SqlRow> {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(sql, params))
        .await
        .unwrap()
}

async fn domain_snapshot(runtime: &KhiveRuntime) -> Value {
    let mut snapshot = serde_json::Map::new();
    for (name, sql) in [
        ("entities", "SELECT * FROM entities ORDER BY id"),
        ("notes", "SELECT * FROM notes ORDER BY id"),
        ("edges", "SELECT * FROM graph_edges ORDER BY id"),
        (
            "domain_events",
            "SELECT * FROM events WHERE kind <> 'audit' ORDER BY id",
        ),
    ] {
        let rows = stored_rows(runtime, sql, vec![]).await;
        assert!(!rows.is_empty(), "populated {name} snapshot");
        snapshot.insert(name.into(), serde_json::to_value(rows).unwrap());
    }
    snapshot.into()
}

async fn event_snapshot(runtime: &KhiveRuntime) -> BTreeMap<String, Value> {
    stored_rows(runtime, "SELECT * FROM events ORDER BY id", vec![])
        .await
        .into_iter()
        .map(|row| {
            (
                row.text("id").unwrap().to_owned(),
                serde_json::to_value(row).unwrap(),
            )
        })
        .collect()
}

async fn assert_preserved(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    before: &Value,
    history: &BTreeMap<String, Value>,
) {
    registry.shutdown_audit_batch().await.unwrap();
    assert_eq!(&domain_snapshot(runtime).await, before);
    let after = event_snapshot(runtime).await;
    for (id, row) in history {
        assert_eq!(after.get(id), Some(row), "prior event {id}");
    }
}

async fn query(registry: &VerbRegistry, source: &str, page_size: usize) -> Value {
    registry
        .dispatch("query", json!({"query": source, "page_size": page_size}))
        .await
        .unwrap_or_else(|error| panic!("{source}: {error}"))
}

async fn query_as(registry: &VerbRegistry, actor: &str, source: &str, page_size: usize) -> Value {
    registry
        .dispatch_as(
            "query",
            json!({"query": source, "page_size": page_size}),
            VerifiedActor::new(actor).unwrap(),
        )
        .await
        .unwrap_or_else(|error| panic!("{actor}: {source}: {error}"))
}

fn assert_page(page: &Value, expected: &[Value], offset: usize, page_size: usize, more: bool) {
    assert_eq!(page["rows"], json!(expected));
    assert_eq!(page["offset"], offset);
    assert_eq!(page["page_size"], page_size);
    assert_eq!(page["has_more"], more);
    assert_eq!(page["truncated"], more);
    if more {
        assert_eq!(page["next_offset"], offset + expected.len());
    } else {
        assert!(page.get("next_offset").is_none());
    }
}

fn raw_ids(rows: &[SqlRow], column: &str) -> Vec<String> {
    rows.iter()
        .map(|row| match row.get(column).unwrap() {
            SqlValue::Text(id) => id.clone(),
            SqlValue::Uuid(id) => id.to_string(),
            other => panic!("unexpected ID cell {other:?}"),
        })
        .collect()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_comma_patterns_return_exact_star_tuples_without_domain_writes() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = registry(&runtime, false);
    observation(&runtime, &token).await;
    let center = entity(&runtime, &token, "selected center").await;
    let other_center = entity(&runtime, &token, "other center").await;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for order in [1, 2] {
        let target = entity(&runtime, &token, &format!("extends target {order}")).await;
        let edge = link(
            &runtime,
            &token,
            center.id,
            target.id,
            EdgeRelation::Extends,
            json!({"order": order, "nested": {"label": "qualified", "items": [1, false, null]}}),
        )
        .await;
        let raw = serde_json::to_string(edge.metadata.as_ref().unwrap()).unwrap();
        left.push((edge, raw));
    }
    for (order, enabled) in [(10, true), (20, false), (30, true)] {
        let target = entity(&runtime, &token, &format!("variant target {order}")).await;
        let edge = link(
            &runtime,
            &token,
            center.id,
            target.id,
            EdgeRelation::VariantOf,
            json!({"order": order, "enabled": enabled}),
        )
        .await;
        let raw = serde_json::to_string(edge.metadata.as_ref().unwrap()).unwrap();
        right.push((edge, raw));
    }
    for relation in [EdgeRelation::Extends, EdgeRelation::VariantOf] {
        let target = entity(&runtime, &token, &format!("other target {relation:?}")).await;
        link(
            &runtime,
            &token,
            other_center.id,
            target.id,
            relation,
            json!({"order": 2, "enabled": true}),
        )
        .await;
    }
    // Raw-text projection must not pass by parsing and serializing the stored JSON again.
    let formatted =
        "{\n  \"order\" : 1, \"nested\" : {\"label\":\"qualified\", \"items\":[1,false,null]}\n}";
    assert_eq!(
        serde_json::from_str::<Value>(formatted).unwrap(),
        *left[0].0.metadata.as_ref().unwrap()
    );
    let changed = runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "UPDATE graph_edges SET metadata = ?1 WHERE id = ?2",
            vec![
                SqlValue::Text(formatted.into()),
                SqlValue::Text(left[0].0.id.to_string()),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(changed, 1);
    left[0].1 = formatted.into();
    let before = domain_snapshot(&runtime).await;
    let history = event_snapshot(&runtime).await;
    let mut expected = Vec::new();
    for (e, e_raw) in &left {
        for (f, f_raw) in right.iter().rev() {
            expected.push(json!({
                "a_id": center.id.to_string(), "b_id": e.target_id.to_string(),
                "c_id": f.target_id.to_string(), "e_id": e.id.to_string(), "f_id": f.id.to_string(),
                "e_metadata": e_raw, "f_metadata": f_raw,
            }));
        }
    }
    assert_eq!(expected.len(), 6);
    let prefix = "MATCH (a:concept)-[e:extends]->(b:concept), (a)-[f:variant_of]->(c:concept)";
    let projection = "RETURN a.id, b.id, c.id, e.id, f.id, e.metadata, f.metadata ORDER BY e.metadata.order ASC, f.metadata.order DESC";
    let whole = query(
        &registry,
        &format!("{prefix} WHERE a.id = '{}' {projection}", center.id),
        20,
    )
    .await;
    assert_page(&whole, &expected, 0, 20, false);
    let narrowed = query(&registry, &format!("{prefix} WHERE a.id = '{}' AND e.metadata.order = 2 AND f.metadata.enabled = true {projection}", center.id), 20).await;
    let narrowed_expected = vec![expected[3].clone(), expected[5].clone()];
    assert_page(&narrowed, &narrowed_expected, 0, 20, false);
    assert_preserved(&runtime, &registry, &before, &history).await;
}

async fn inbound_message(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    recipient: &str,
    key: &str,
    content: &str,
) -> Uuid {
    let sent = registry.dispatch_as("comm.send", json!({
        "to": recipient, "content": content, "subject": "Comma query scope", "idempotency_key": key,
    }), VerifiedActor::new("sender").unwrap()).await.unwrap();
    let outbound = runtime
        .notes(token)
        .unwrap()
        .get_note(Uuid::parse_str(sent["full_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    Uuid::parse_str(
        outbound.properties.as_ref().unwrap()["inbound_ref"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_comma_patterns_keep_later_message_branches_private() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = registry(&runtime, true);
    entity(&runtime, &token, "mailbox domain witness").await;
    let center = observation(&runtime, &token).await;
    let anchor_id = inbound_message(
        &runtime,
        &registry,
        &token,
        "reader",
        "comma-anchor",
        "Visible anchor",
    )
    .await;
    let visible_id = inbound_message(
        &runtime,
        &registry,
        &token,
        "reader",
        "comma-visible",
        "Visible candidate",
    )
    .await;
    let hidden_id = inbound_message(
        &runtime,
        &registry,
        &token,
        "other",
        "comma-hidden",
        "Foreign candidate body",
    )
    .await;
    let anchor = link(
        &runtime,
        &token,
        anchor_id,
        center,
        EdgeRelation::Supports,
        json!({"branch": "anchor"}),
    )
    .await;
    let visible = link(
        &runtime,
        &token,
        visible_id,
        center,
        EdgeRelation::Supports,
        json!({"branch": "candidate", "order": 10}),
    )
    .await;
    let hidden = link(
        &runtime,
        &token,
        hidden_id,
        center,
        EdgeRelation::Supports,
        json!({"branch": "candidate", "order": -100}),
    )
    .await;
    let before = domain_snapshot(&runtime).await;
    let history = event_snapshot(&runtime).await;
    // This actual caller can read the foreign candidate: the hidden row is not absent setup.
    let foreign_positive = query_as(&registry, "other", &format!(
        "MATCH (m:message)-[f:supports]->(a:observation) WHERE f.id = '{}' RETURN m.id, m.content, f.id",
        hidden.id), 1).await;
    assert_page(
        &foreign_positive,
        &[
            json!({"m_id": hidden_id.to_string(), "m_content": "Foreign candidate body", "f_id": hidden.id.to_string()}),
        ],
        0,
        1,
        false,
    );
    for paths in [
        "(anchor:message)-[e:supports]->(a:observation), (m:message)-[f:supports]->(a)",
        "(m:message)-[f:supports]->(a:observation), (anchor:message)-[e:supports]->(a)",
    ] {
        for with_content in [false, true] {
            let projection = if with_content {
                "RETURN a.id, e.id, f.id, m.id, m.content"
            } else {
                "RETURN a.id, e.id, f.id"
            };
            let source = format!("MATCH {paths} WHERE e.id = '{}' AND f.metadata.branch = 'candidate' {projection} ORDER BY f.metadata.order", anchor.id);
            let mut expected = json!({"a_id": center.to_string(), "e_id": anchor.id.to_string(), "f_id": visible.id.to_string()});
            if with_content {
                expected["m_id"] = json!(visible_id.to_string());
                expected["m_content"] = json!("Visible candidate");
            }
            let page = query_as(&registry, "reader", &source, 1).await;
            assert_page(&page, &[expected], 0, 1, false);
            let encoded = page.to_string();
            assert!(!encoded.contains(&hidden_id.to_string()));
            assert!(!encoded.contains(&hidden.id.to_string()));
            assert!(!encoded.contains("Foreign candidate body"));
            let exhausted = query_as(&registry, "reader", &format!("{source} SKIP 1"), 1).await;
            assert_page(&exhausted, &[], 1, 1, false);
        }
    }
    assert_preserved(&runtime, &registry, &before, &history).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_comma_patterns_filter_every_occurrence_before_paging() {
    let runtime = runtime();
    let local = runtime.authorize(Namespace::local()).unwrap();
    let foreign = runtime
        .authorize(Namespace::parse("comma-hidden").unwrap())
        .unwrap();
    let registry = registry(&runtime, false);
    observation(&runtime, &local).await;
    let center = entity(&runtime, &local, "scope center").await;
    let anchor = entity(&runtime, &local, "scope anchor").await;
    let e = link(
        &runtime,
        &local,
        center.id,
        anchor.id,
        EdgeRelation::Extends,
        json!({"order": 0}),
    )
    .await;
    let mut visible = Vec::new();
    for order in [10, 20] {
        let target = entity(&runtime, &local, &format!("visible {order}")).await;
        visible.push(
            link(
                &runtime,
                &local,
                center.id,
                target.id,
                EdgeRelation::VariantOf,
                json!({"order": order}),
            )
            .await,
        );
    }
    let hidden_target = entity(&runtime, &foreign, "hidden endpoint only").await;
    let hidden_endpoint_edge = link(
        &runtime,
        &local,
        center.id,
        hidden_target.id,
        EdgeRelation::VariantOf,
        json!({"order": -100}),
    )
    .await;
    let local_target = entity(&runtime, &local, "visible endpoint hidden edge").await;
    let hidden_edge = link(
        &runtime,
        &foreign,
        center.id,
        local_target.id,
        EdgeRelation::VariantOf,
        json!({"order": -90}),
    )
    .await;
    let live_target = entity(&runtime, &local, "visible endpoint deleted edge").await;
    let deleted_edge = link(
        &runtime,
        &local,
        center.id,
        live_target.id,
        EdgeRelation::VariantOf,
        json!({"order": -80}),
    )
    .await;
    assert!(runtime
        .delete_edge(&local, deleted_edge.id.into(), false)
        .await
        .unwrap());
    let mut deleted_target = entity(&runtime, &local, "deleted endpoint only").await;
    let live_edge_to_deleted = link(
        &runtime,
        &local,
        center.id,
        deleted_target.id,
        EdgeRelation::VariantOf,
        json!({"order": -70}),
    )
    .await;
    // Use the typed store to isolate the node filter from any runtime incident-edge cleanup.
    deleted_target.deleted_at = Some(deleted_target.created_at);
    runtime
        .entities(&local)
        .unwrap()
        .upsert_entity(deleted_target.clone())
        .await
        .unwrap();
    for (edge, namespace, deleted) in [
        (&hidden_endpoint_edge, "local", false),
        (&hidden_edge, "comma-hidden", false),
        (&deleted_edge, "local", true),
        (&live_edge_to_deleted, "local", false),
    ] {
        let rows = stored_rows(
            &runtime,
            "SELECT namespace, deleted_at FROM graph_edges WHERE id = ?1",
            vec![SqlValue::Text(edge.id.to_string())],
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("namespace").unwrap(), namespace);
        assert_eq!(
            matches!(rows[0].get("deleted_at"), Some(SqlValue::Integer(_))),
            deleted
        );
        if !deleted {
            assert!(matches!(rows[0].get("deleted_at"), Some(SqlValue::Null)));
        }
    }
    for (target, namespace, deleted) in [
        (&hidden_target, "comma-hidden", false),
        (&local_target, "local", false),
        (&live_target, "local", false),
        (&deleted_target, "local", true),
    ] {
        let rows = stored_rows(
            &runtime,
            "SELECT namespace, deleted_at FROM entities WHERE id = ?1",
            vec![SqlValue::Text(target.id.to_string())],
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("namespace").unwrap(), namespace);
        assert_eq!(
            matches!(rows[0].get("deleted_at"), Some(SqlValue::Integer(_))),
            deleted
        );
        if !deleted {
            assert!(matches!(rows[0].get("deleted_at"), Some(SqlValue::Null)));
        }
    }
    let before = domain_snapshot(&runtime).await;
    let history = event_snapshot(&runtime).await;
    let expected: Vec<Value> = visible.iter().map(|f| json!({
        "a_id": center.id.to_string(), "b_id": anchor.id.to_string(), "c_id": f.target_id.to_string(),
        "e_id": e.id.to_string(), "f_id": f.id.to_string(),
    })).collect();
    for paths in [
        "(a:concept)-[e:extends]->(b:concept), (a)-[f:variant_of]->(c:concept)",
        "(a:concept)-[f:variant_of]->(c:concept), (a)-[e:extends]->(b:concept)",
    ] {
        let source = format!("MATCH {paths} WHERE a.id = '{}' RETURN a.id, b.id, c.id, e.id, f.id ORDER BY f.metadata.order", center.id);
        assert_page(
            &query(&registry, &source, 20).await,
            &expected,
            0,
            20,
            false,
        );
        assert_page(
            &query(&registry, &source, 1).await,
            &expected[..1],
            0,
            1,
            true,
        );
        assert_page(
            &query(&registry, &format!("{source} SKIP 1"), 1).await,
            &expected[1..],
            1,
            1,
            false,
        );
        assert_page(
            &query(&registry, &format!("{source} SKIP 2"), 1).await,
            &[],
            2,
            1,
            false,
        );
        let wider = runtime
            .authorize_with_visibility(
                Namespace::local(),
                vec![Namespace::parse("comma-hidden").unwrap()],
            )
            .unwrap();
        let rows = runtime.query(&wider, &source).await.unwrap();
        assert_eq!(
            raw_ids(&rows, "f_id"),
            [
                hidden_endpoint_edge.id.to_string(),
                hidden_edge.id.to_string(),
                visible[0].id.to_string(),
                visible[1].id.to_string()
            ]
        );
    }
    assert_preserved(&runtime, &registry, &before, &history).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn query_comma_patterns_refuse_unsupported_shapes_and_preserve_single_paths() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = registry(&runtime, false);
    observation(&runtime, &token).await;
    let source = entity(&runtime, &token, "compatibility source").await;
    let target = entity(&runtime, &token, "compatibility target").await;
    let edge = link(
        &runtime,
        &token,
        source.id,
        target.id,
        EdgeRelation::VariantOf,
        json!({"order": 1}),
    )
    .await;
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
    let observation_query = format!(
        "MATCH (ev)-[e:observed_as_target]->(b) WHERE ev.id = '{}' RETURN b.id",
        events.items[0].id
    );
    let recursive_query = format!("MATCH (a:concept)-[e:variant_of*1..2]->(b:concept) WHERE a.id = '{}' RETURN e.metadata ORDER BY e.metadata.order", source.id);
    let before = domain_snapshot(&runtime).await;
    let history = event_snapshot(&runtime).await;
    let observed = query(&registry, &observation_query, 20).await;
    let observed_ids: BTreeSet<String> = observed["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["b_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        observed_ids,
        [source.id.to_string(), target.id.to_string()]
            .into_iter()
            .collect()
    );
    let recursive = query(&registry, &recursive_query, 20).await;
    assert_page(
        &recursive,
        &[
            json!({"e_metadata": serde_json::to_string(edge.metadata.as_ref().unwrap()).unwrap(), "_depth": 1, "_total_weight": 1.0}),
        ],
        0,
        20,
        false,
    );
    let connected = query(&registry, &format!("MATCH (a:concept)-[e:variant_of]->(b:concept), (a:concept) WHERE a.id = '{}' RETURN e.id", source.id), 20).await;
    assert_page(
        &connected,
        &[json!({"e_id": edge.id.to_string()})],
        0,
        20,
        false,
    );
    for invalid in [
        "MATCH (a:concept), (b:concept) RETURN a.id",
        "MATCH (a:concept)-[e:variant_of*1..2]->(b:concept), (a)-[f:variant_of]->(c) RETURN a.id",
        "MATCH (ev)-[e:observed_as_target]->(a), (a)-[f:variant_of]->(b) RETURN b.id",
        "MATCH (a:concept)-[e:variant_of]->(b), (a)-[e:variant_of]->(c) RETURN a.id",
        "MATCH (a:concept)-[e:variant_of]->(a), (a)-[f:variant_of]->(b) RETURN a.id",
    ] {
        let error = registry
            .dispatch("query", json!({"query": invalid}))
            .await
            .expect_err("shape must refuse");
        assert!(
            matches!(
                error.refusal_source(),
                RuntimeError::Query(QueryError::Unsupported(_))
            ),
            "{invalid}: {error}"
        );
        assert_eq!(domain_snapshot(&runtime).await, before, "{invalid}");
    }
    for invalid in [
        "MATCH , (a:concept) RETURN a.id",
        "MATCH (a:concept), RETURN a.id",
        "MATCH (a:concept)-[e:variant_of]->(b), (a)-[f:variant_of]->(e) RETURN a.id",
        "MATCH (a:concept), (a:concept {namespace:'foreign'}) RETURN a.id",
        "MATCH (a:concept), (a:concept) RETURN missing.id",
        "MATCH (a:concept)-[e:variant_of]->(b), (a) WHERE e.metadata.order;DELETE = 1 RETURN e.id",
    ] {
        let error = registry
            .dispatch("query", json!({"query": invalid}))
            .await
            .expect_err("invalid query must refuse");
        assert!(
            matches!(error.refusal_source(), RuntimeError::Query(_)),
            "{invalid}: {error}"
        );
        assert_eq!(domain_snapshot(&runtime).await, before, "{invalid}");
    }
    assert_eq!(
        query(&registry, &observation_query, 20).await["rows"],
        observed["rows"]
    );
    assert_eq!(
        query(&registry, &recursive_query, 20).await["rows"],
        recursive["rows"]
    );
    assert_preserved(&runtime, &registry, &before, &history).await;
}
