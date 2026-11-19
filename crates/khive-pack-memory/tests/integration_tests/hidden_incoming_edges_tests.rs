async fn hidden_incoming_seed_note(
    rt: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    content: &str,
) -> Uuid {
    let token = rt.authorize(Namespace::parse(namespace).expect("fixture namespace"))
        .expect("fixture note token");
    let note = rt
        .create_note(&token, kind, None, content, Some(0.7), None, Vec::new())
        .await
        .expect("fixture create_note failed; setup failure, never control credit");
    assert_eq!(note.kind, kind, "fixture note kind");
    assert_eq!(note.namespace, namespace, "fixture note namespace");
    note.id
}

async fn hidden_incoming_seed_edge(
    rt: &KhiveRuntime,
    namespace: &str,
    source: Uuid,
    target: Uuid,
    ordinal: u64,
) -> Uuid {
    let id: Uuid = format!("70000000-0000-7000-8000-{ordinal:012x}")
        .parse()
        .expect("fixed fixture edge UUID");
    let edge = serde_json::from_value(json!({
        "id": id,
        "namespace": namespace,
        "source_id": source,
        "target_id": target,
        "relation": "supersedes",
        "weight": 0.75,
        "created_at": "2026-11-04T00:00:00Z",
        "updated_at": "2026-11-04T00:00:00Z",
        "deleted_at": null,
        "metadata": {"fixture": "hidden-incoming"},
        "target_backend": null,
    }))
    .expect("fixture edge representation");
    let token = rt.authorize(Namespace::parse(namespace).expect("fixture edge namespace"))
        .expect("fixture edge token");
    rt.graph(&token).expect("fixture graph").upsert_edge(edge).await
        .expect("fixture upsert_edge failed; setup failure, never control credit");
    id
}

async fn hidden_incoming_neighbors(
    registry: &VerbRegistry,
    anchor: Uuid,
    direction: &str,
    limit: Option<u32>,
    relation_filter: bool,
) -> Value {
    let mut params = json!({"namespace": "local", "id": anchor.to_string(), "direction": direction});
    if let Some(limit) = limit {
        params["limit"] = json!(limit);
    }
    if relation_filter {
        params["relations"] = json!(["supersedes", "supports"]);
    }
    registry.dispatch_with_disposition("neighbors", params, None).await
        .expect("actual neighbors read failed; setup/error failure, never control credit")
}

fn hidden_incoming_assert_private(response: &Value, ids: &[Uuid], namespace: &str) {
    let bytes = response.to_string();
    assert!(!bytes.contains(namespace), "hidden source namespace disclosed");
    for id in ids {
        assert!(!bytes.contains(&id.to_string()), "hidden source/edge ID disclosed");
    }
    assert!(!bytes.contains("private source content"), "hidden source content disclosed");
}

async fn hidden_incoming_fixture() -> (KhiveRuntime, VerbRegistry, Uuid, Uuid) {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "d2-visible-reader");
    let anchor = hidden_incoming_seed_note(
        &rt, "local", "memory", "d2incominganchor uniquesecondreadertoken",
    ).await;
    let target = hidden_incoming_seed_note(
        &rt, "local", "memory", "visible outgoing fixture target",
    ).await;
    let visible_edge = hidden_incoming_seed_edge(&rt, "local", anchor, target, 1).await;
    let baseline = hidden_incoming_neighbors(&registry, anchor, "both", None, true).await;
    let visible = baseline["neighbors"].as_array().expect("memory anchor object baseline");
    assert_eq!(visible.len(), 1, "fixture_visible_edge_baseline");
    assert!(visible[0].to_string().contains(&visible_edge.to_string()), "fixture_visible_edge_id");
    assert_eq!(baseline["hidden_incoming_edges"], json!({
        "total": 0, "by_relation": {"supersedes": 0, "supports": 0},
    }), "fixture_zero_count_baseline");
    (rt, registry, anchor, visible_edge)
}

async fn hidden_incoming_add_private_sources(rt: &KhiveRuntime, anchor: Uuid) -> Vec<Uuid> {
    let namespace = "d2-private-hidden";
    let first = hidden_incoming_seed_note(rt, namespace, "memory", "private source content one").await;
    let second = hidden_incoming_seed_note(rt, namespace, "memory", "private source content two").await;
    let first_edge = hidden_incoming_seed_edge(rt, namespace, first, anchor, 2).await;
    let second_edge = hidden_incoming_seed_edge(rt, namespace, second, anchor, 3).await;
    vec![first, second, first_edge, second_edge]
}

#[tokio::test]
async fn test_hidden_incoming_edges_second_reader_neighbors_counts() {
    let (rt, registry, anchor, visible_edge) = hidden_incoming_fixture().await;
    let hidden = hidden_incoming_add_private_sources(&rt, anchor).await;
    for limit in [None, Some(1)] {
        let response = hidden_incoming_neighbors(&registry, anchor, "both", limit, true).await;
        let visible = response["neighbors"].as_array().expect("memory anchor remains object");
        assert_eq!(visible.len(), 1, "second reader preserves visible adjacency");
        assert!(visible[0].to_string().contains(&visible_edge.to_string()), "second reader visible edge");
        hidden_incoming_assert_private(&response, &hidden, "d2-private-hidden");
        assert_eq!(response["hidden_incoming_edges"]["total"], json!(2),
            "second_reader_neighbors_hidden_count");
        assert_eq!(response["hidden_incoming_edges"]["by_relation"],
            json!({"supersedes": 2, "supports": 0}), "requested relation zero is explicit");
    }
    let unfiltered = hidden_incoming_neighbors(&registry, anchor, "in", None, false).await;
    assert_eq!(unfiltered["neighbors"], json!([]), "hidden endpoints are not returned");
    assert_eq!(unfiltered["hidden_incoming_edges"], json!({
        "total": 2, "by_relation": {"supersedes": 2},
    }), "unfiltered relation map lists only positive counts");
    hidden_incoming_assert_private(&unfiltered, &hidden, "d2-private-hidden");
    let empty_relations = registry.dispatch_with_disposition("neighbors", json!({
        "namespace": "local", "id": anchor.to_string(), "direction": "in", "relations": [],
    }), None).await.expect("actual empty-relations read");
    assert_eq!(empty_relations["neighbors"], unfiltered["neighbors"],
        "empty_relations_preserve_unfiltered_adjacency");
    assert_eq!(empty_relations["hidden_incoming_edges"], unfiltered["hidden_incoming_edges"],
        "empty_relations_match_unfiltered_hidden_counts");
    hidden_incoming_assert_private(&empty_relations, &hidden, "d2-private-hidden");
    let outgoing = hidden_incoming_neighbors(&registry, anchor, "out", None, true).await;
    assert_eq!(outgoing["neighbors"].as_array().expect("outgoing memory object").len(), 1);
    assert_eq!(outgoing["hidden_incoming_edges"], json!({
        "total": 0, "by_relation": {"supersedes": 0, "supports": 0},
    }), "outgoing direction carries zero");
}

async fn hidden_incoming_recall(registry: &VerbRegistry) -> Value {
    registry.dispatch_with_disposition("memory.recall", json!({
        "namespace": "local",
        "query": "uniquesecondreadertoken",
        "fusion_strategy": "keyword_only",
        "min_score": 0.0,
        "limit": 10,
        "full_content": true,
    }), None).await.expect("actual keyword recall failed; invalid fixture, never control credit")
}

fn hidden_incoming_recalled_anchor(response: &Value, anchor: Uuid) -> &Value {
    let hits = response.as_array()
        .or_else(|| response.get("results").and_then(Value::as_array))
        .expect("actual recall hit envelope");
    let id = anchor.to_string();
    hits.iter().find(|hit| hit.get("id").and_then(Value::as_str) == Some(id.as_str()))
        .expect("fixture_keyword_recall_anchor_present")
}

#[tokio::test]
async fn test_hidden_incoming_edges_second_reader_recall_counts() {
    let (rt, registry, anchor, _) = hidden_incoming_fixture().await;
    let baseline = hidden_incoming_recall(&registry).await;
    assert_eq!(hidden_incoming_recalled_anchor(&baseline, anchor)["hidden_incoming_edges"], json!({
        "total": 0, "by_relation": {"supersedes": 0},
    }), "fixture_keyword_recall_zero_baseline");
    let hidden = hidden_incoming_add_private_sources(&rt, anchor).await;
    let response = hidden_incoming_recall(&registry).await;
    let hit = hidden_incoming_recalled_anchor(&response, anchor);
    hidden_incoming_assert_private(&response, &hidden, "d2-private-hidden");
    assert_eq!(hit["hidden_incoming_edges"]["total"], json!(2),
        "second_reader_recall_hidden_count");
    assert_eq!(hit["hidden_incoming_edges"]["by_relation"], json!({"supersedes": 2}));
}

#[tokio::test]
async fn test_hidden_incoming_edges_exclude_nonmemory_and_deleted_endpoints() {
    let (rt, registry, anchor, _) = hidden_incoming_fixture().await;
    let hidden = hidden_incoming_add_private_sources(&rt, anchor).await;
    let namespace = "d2-private-hidden";
    let nonmemory = hidden_incoming_seed_note(&rt, namespace, "observation", "private source content observation").await;
    hidden_incoming_seed_edge(&rt, namespace, nonmemory, anchor, 4).await;
    let deleted = hidden_incoming_seed_note(&rt, namespace, "memory", "private source content deleted").await;
    let token = rt.authorize(Namespace::parse(namespace).expect("deleted source namespace"))
        .expect("deleted source token");
    assert!(rt.delete_note(&token, deleted, false).await
        .expect("fixture soft-delete refused; setup failure, never control credit"),
        "fixture soft-delete must find its source");
    assert!(matches!(rt.resolve_by_id_including_deleted(&token, deleted).await
        .expect("deleted source readback"), Some(khive_runtime::Resolved::Note(note))
        if note.kind == "memory" && note.namespace == namespace), "deleted source retains stored identity");
    hidden_incoming_seed_edge(&rt, namespace, deleted, anchor, 5).await;
    let response = hidden_incoming_neighbors(&registry, anchor, "in", None, true).await;
    assert_eq!(response["hidden_incoming_edges"]["total"], json!(2),
        "nonmemory_and_deleted_sources_do_not_inflate_hidden_count");
    hidden_incoming_assert_private(&response, &hidden, namespace);
    assert!(!response.to_string().contains(&nonmemory.to_string()));
    assert!(!response.to_string().contains(&deleted.to_string()));
}

#[tokio::test]
async fn test_hidden_incoming_edges_nonmemory_anchor_keeps_legacy_array() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "d2-visible-reader");
    let anchor = hidden_incoming_seed_note(&rt, "local", "observation", "nonmemory anchor").await;
    let response = hidden_incoming_neighbors(&registry, anchor, "both", None, true).await;
    assert_eq!(response.to_string(), "[]", "nonmemory_anchor_legacy_array_bytes");
}

#[tokio::test]
async fn test_hidden_incoming_edges_use_stored_edge_namespace() {
    let (rt, registry, hidden_edge_anchor, visible_outgoing) = hidden_incoming_fixture().await;
    let visible_edge_anchor = hidden_incoming_seed_note(
        &rt, "local", "memory", "anchor of a visible legacy edge",
    ).await;
    let hidden_source = hidden_incoming_seed_note(
        &rt, "d2-private-hidden", "memory", "source of a visible legacy edge",
    ).await;
    let visible_edge = hidden_incoming_seed_edge(
        &rt, "local", hidden_source, visible_edge_anchor, 6,
    ).await;
    let returned = hidden_incoming_neighbors(&registry, visible_edge_anchor, "in", None, true).await;
    let incoming = returned["neighbors"].as_array().expect("visible legacy edge anchor object");
    assert_eq!(incoming.len(), 1, "visible_stored_edge_adjacency_baseline");
    assert!(incoming[0].to_string().contains(&visible_edge.to_string()),
        "visible stored legacy edge ID is returned");
    assert!(incoming[0].to_string().contains(&hidden_source.to_string()),
        "visible stored legacy edge source is already returned by the ordinary read");
    assert_eq!(returned["hidden_incoming_edges"], json!({
        "total": 0, "by_relation": {"supersedes": 0, "supports": 0},
    }), "hidden_source_visible_stored_edge_has_zero_count");

    let visible_source = hidden_incoming_seed_note(
        &rt, "local", "memory", "visible source of a hidden legacy edge",
    ).await;
    let hidden_edge = hidden_incoming_seed_edge(
        &rt, "d2-private-hidden", visible_source, hidden_edge_anchor, 7,
    ).await;
    let omitted = hidden_incoming_neighbors(&registry, hidden_edge_anchor, "both", None, true).await;
    let outgoing = omitted["neighbors"].as_array().expect("hidden legacy edge anchor object");
    assert_eq!(outgoing.len(), 1, "hidden stored edge does not change ordinary adjacency");
    assert!(outgoing[0].to_string().contains(&visible_outgoing.to_string()),
        "actual visible outgoing baseline is preserved");
    assert_eq!(omitted["hidden_incoming_edges"]["total"], json!(1),
        "visible_source_hidden_stored_edge_is_counted");
    assert_eq!(omitted["hidden_incoming_edges"]["by_relation"],
        json!({"supersedes": 1, "supports": 0}));
    assert!(!omitted.to_string().contains(&hidden_edge.to_string()), "hidden edge ID omitted");
    assert!(!omitted.to_string().contains("d2-private-hidden"), "hidden edge namespace omitted");
}
