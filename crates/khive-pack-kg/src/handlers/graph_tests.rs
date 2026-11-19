use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
use khive_types::EdgeRelation;
use serde_json::json;

use super::*;

async fn pooled_reader_checkouts(runtime: &KhiveRuntime) -> u64 {
    runtime
        .db_diagnostics()
        .await
        .expect("reader diagnostics")
        .reader_contention
        .pooled_reader_checkouts
}

/// Regression for #1670: `neighbors` response must carry the edge's
/// stored `source_id`/`target_id`, independent of `direction` and
/// `origin_id` (which always echoes the queried node). Without the
/// `graph.rs` fix, `NeighborHitResponse` serializes only `origin_id` +
/// the flattened `NeighborHit` — `source_id`/`target_id` are absent.
#[tokio::test]
async fn neighbors_stored_direction() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");
    let pack = KgPack::new(rt);

    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Src1670", "entity_kind": "concept"}),
            &registry,
            &token,
        )
        .await
        .expect("create source must succeed");
    let tgt = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Tgt1670", "entity_kind": "concept"}),
            &registry,
            &token,
        )
        .await
        .expect("create target must succeed");
    let src_id = src["id"].as_str().expect("src id").to_string();
    let tgt_id = tgt["id"].as_str().expect("tgt id").to_string();

    pack.dispatch(
        "link",
        json!({"source_id": src_id, "target_id": tgt_id, "relation": "contains", "weight": 1.0}),
        &registry,
        &token,
    )
    .await
    .expect("link must succeed");

    // Querying from src with direction=out: origin_id == src. The
    // stored edge direction is unaffected by the query direction.
    let out = pack
        .dispatch(
            "neighbors",
            json!({"id": src_id, "direction": "out"}),
            &registry,
            &token,
        )
        .await
        .expect("neighbors out must succeed");
    let out_arr = out.as_array().expect("neighbors out returns array");
    let out_hit = out_arr
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(tgt_id.as_str()))
        .expect("must find tgt in outbound neighbors");
    assert_eq!(
        out_hit.get("source_id").and_then(Value::as_str),
        Some(src_id.as_str()),
        "source_id must be the edge's stored source; hit={out_hit}"
    );
    assert_eq!(
        out_hit.get("target_id").and_then(Value::as_str),
        Some(tgt_id.as_str()),
        "target_id must be the edge's stored target; hit={out_hit}"
    );
    assert_eq!(
        out_hit.get("origin_id").and_then(Value::as_str),
        Some(src_id.as_str()),
        "origin_id must echo the queried node"
    );

    // Querying from tgt with direction=in: origin_id == tgt, but the
    // stored edge direction (source == src, target == tgt) must not flip.
    let incoming = pack
        .dispatch(
            "neighbors",
            json!({"id": tgt_id, "direction": "in"}),
            &registry,
            &token,
        )
        .await
        .expect("neighbors in must succeed");
    let in_arr = incoming.as_array().expect("neighbors in returns array");
    let in_hit = in_arr
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(src_id.as_str()))
        .expect("must find src in inbound neighbors");
    assert_eq!(
        in_hit.get("source_id").and_then(Value::as_str),
        Some(src_id.as_str()),
        "source_id must be the edge's stored source regardless of query direction; hit={in_hit}"
    );
    assert_eq!(
        in_hit.get("target_id").and_then(Value::as_str),
        Some(tgt_id.as_str()),
        "target_id must be the edge's stored target regardless of query direction; hit={in_hit}"
    );
    assert_eq!(
        in_hit.get("origin_id").and_then(Value::as_str),
        Some(tgt_id.as_str()),
        "origin_id must echo the queried node, not the stored source"
    );

    // Omitted direction defaults to `both`, which the storage layer serves
    // through separate UNION ALL SQL rather than the single-direction
    // query, so it needs its own coverage: the stored endpoints must be
    // identical whichever side of the edge the query originates from.
    for (queried, expected_neighbor) in [(&src_id, &tgt_id), (&tgt_id, &src_id)] {
        let both = pack
            .dispatch("neighbors", json!({"id": queried}), &registry, &token)
            .await
            .expect("neighbors with omitted direction must succeed");
        let both_arr = both.as_array().expect("neighbors both returns array");
        let hit = both_arr
            .iter()
            .find(|h| h.get("id").and_then(Value::as_str) == Some(expected_neighbor.as_str()))
            .expect("must find the opposite endpoint under default direction");
        assert_eq!(
            hit.get("source_id").and_then(Value::as_str),
            Some(src_id.as_str()),
            "default-direction hit must carry the stored source; queried={queried} hit={hit}"
        );
        assert_eq!(
            hit.get("target_id").and_then(Value::as_str),
            Some(tgt_id.as_str()),
            "default-direction hit must carry the stored target; queried={queried} hit={hit}"
        );
        assert_eq!(
            hit.get("origin_id").and_then(Value::as_str),
            Some(queried.as_str()),
            "origin_id must echo the queried node under default direction"
        );
    }
}

/// Pins the encoding for an edge that could not be read back.
///
/// The neighbor query and the per-hit edge read are two separate reads, so
/// an edge deleted between them leaves the endpoints unknown. Reporting a
/// pair of nil UUIDs there would be a value-shaped absence: syntactically a
/// direction, semantically false, and indistinguishable from a real edge
/// between nil nodes. Since the entire purpose of these fields is to convey
/// the STORED direction, an unknown direction has to stay distinguishable
/// from a known one.
///
/// This asserts the wire format rather than the race: it fails if the
/// unknown case is ever encoded as `Uuid::nil()` again.
#[test]
fn unresolvable_edge_endpoints_serialize_as_null_never_nil_uuids() {
    let response = NeighborHitResponse {
        origin_id: Uuid::from_u128(1),
        source_id: None,
        target_id: None,
        hit: NeighborHit {
            node_id: Uuid::from_u128(2),
            edge_id: Uuid::from_u128(3),
            relation: EdgeRelation::Contains,
            weight: 1.0,
            name: None,
            kind: None,
            entity_type: None,
        },
    };

    let encoded = serde_json::to_value(&response).expect("serialize");
    assert_eq!(
        encoded.get("source_id"),
        Some(&Value::Null),
        "an unreadable edge must report a null source_id; encoded={encoded}"
    );
    assert_eq!(
        encoded.get("target_id"),
        Some(&Value::Null),
        "an unreadable edge must report a null target_id; encoded={encoded}"
    );

    let nil = Uuid::nil().to_string();
    assert_ne!(
        encoded.get("source_id").and_then(Value::as_str),
        Some(nil.as_str()),
        "the unknown case must not be encoded as the nil UUID: a consumer \
             cannot tell that apart from a stored direction"
    );

    // Control: a KNOWN direction still serializes as the plain id, so the
    // assertions above are about the unknown case and not about the field
    // disappearing from the response altogether.
    let known = NeighborHitResponse {
        source_id: Some(Uuid::from_u128(7)),
        target_id: Some(Uuid::from_u128(8)),
        ..response
    };
    let encoded = serde_json::to_value(&known).expect("serialize");
    assert_eq!(
        encoded.get("source_id").and_then(Value::as_str),
        Some(Uuid::from_u128(7).to_string().as_str()),
        "a known source must still serialize as its id; encoded={encoded}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn neighbors_limit_cursor_kind_and_projection_contract() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");
    let pack = KgPack::new(rt.clone());

    let root = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Root2674", "entity_kind": "concept"}),
            &registry,
            &token,
        )
        .await
        .expect("create root must succeed");
    let root_id = root["id"].as_str().expect("root id").to_string();

    for index in 0..5 {
        let target = pack
            .dispatch(
                "create",
                json!({
                    "kind": "entity",
                    "name": format!("Neighbor2674-{index}"),
                    "entity_kind": "concept"
                }),
                &registry,
                &token,
            )
            .await
            .expect("create target must succeed");
        pack.dispatch(
            "link",
            json!({
                "source_id": root_id,
                "target_id": target["id"],
                "relation": "supports",
                "weight": 1.0
            }),
            &registry,
            &token,
        )
        .await
        .expect("link target must succeed");
    }

    let note = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "Neighbor observation 2674"
            }),
            &registry,
            &token,
        )
        .await
        .expect("create note must succeed");
    pack.dispatch(
        "link",
        json!({
            "source_id": note["id"],
            "target_id": root_id,
            "relation": "annotates",
            "weight": 1.0
        }),
        &registry,
        &token,
    )
    .await
    .expect("link note must succeed");

    let control = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "both"}),
            &registry,
            &token,
        )
        .await
        .expect("control neighbors must succeed");
    let control_ids: Vec<&str> = control
        .as_array()
        .expect("no-limit neighbors remain an array")
        .iter()
        .map(|hit| hit["id"].as_str().expect("control neighbor id"))
        .collect();
    assert_eq!(control_ids.len(), 6);

    let mut page = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "both", "limit": 2}),
            &registry,
            &token,
        )
        .await
        .expect("first neighbor page must succeed");
    assert_eq!(page["requested_limit"], 2);
    assert_eq!(page["effective_limit"], 2);
    assert_eq!(page["limit_clamped"], false);
    let mut paged_ids = Vec::new();
    loop {
        let page_items = page["neighbors"].as_array().expect("neighbor page array");
        assert!(page_items.len() <= 2);
        paged_ids.extend(
            page_items
                .iter()
                .map(|hit| hit["id"].as_str().expect("paged neighbor id").to_string()),
        );
        let Some(after) = page["next_after"].as_str().map(str::to_owned) else {
            break;
        };
        page = pack
            .dispatch(
                "neighbors",
                json!({
                    "id": root_id,
                    "direction": "both",
                    "limit": 2,
                    "after": after
                }),
                &registry,
                &token,
            )
            .await
            .expect("continuation neighbor page must succeed");
    }
    assert_eq!(paged_ids, control_ids);

    let over_cap = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "both", "limit": 2001}),
            &registry,
            &token,
        )
        .await
        .expect("over-cap neighbors must succeed");
    assert_eq!(over_cap["neighbors"].as_array().unwrap().len(), 6);
    assert_eq!(over_cap["requested_limit"], 2001);
    assert_eq!(over_cap["effective_limit"], 1000);
    assert_eq!(over_cap["limit_clamped"], true);

    let filtered = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "both", "neighbor_kinds": ["observation"]}),
            &registry,
            &token,
        )
        .await
        .expect("kind-filtered neighbors must succeed");
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0]["kind"], "observation");

    let edge_projection = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "out", "limit": 1, "projection": "edge"}),
            &registry,
            &token,
        )
        .await
        .expect("edge projection must succeed");
    assert!(edge_projection["neighbors"][0].get("source_id").is_some());
    assert!(edge_projection["neighbors"][0].get("target_id").is_some());
    assert!(edge_projection["neighbors"][0].get("id").is_none());

    let summary_projection = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "out", "limit": 1, "projection": "summary"}),
            &registry,
            &token,
        )
        .await
        .expect("summary projection must succeed");
    assert!(summary_projection["neighbors"][0].get("id").is_some());
    assert!(summary_projection["neighbors"][0].get("name").is_some());
    assert!(summary_projection["neighbors"][0]
        .get("source_id")
        .is_none());

    let record_projection = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "out", "limit": 1, "projection": "record"}),
            &registry,
            &token,
        )
        .await
        .expect("record projection must succeed");
    let record_hit = &record_projection["neighbors"][0];
    let expected_summary = json!({
        "neighbors": [{
            "origin_id": record_hit["origin_id"],
            "id": record_hit["id"],
            "edge_id": record_hit["edge_id"],
            "relation": record_hit["relation"],
            "weight": record_hit["weight"],
            "name": record_hit["name"],
            "kind": record_hit["kind"],
            "entity_type": record_hit["entity_type"],
        }],
        "next_after": record_projection["next_after"],
        "requested_limit": 1,
        "effective_limit": 1,
        "limit_clamped": false,
    });
    assert_eq!(
        serde_json::to_vec(&summary_projection).expect("serialize summary"),
        serde_json::to_vec(&expected_summary).expect("serialize expected summary"),
        "Summary must keep its existing wire response"
    );

    // The pool counter measures the actual reader calls behind the handler.
    // Both projections run the same adjacency query; only Record needs the
    // additional batched endpoint read. Account for diagnostics' own fixed
    // read cost by measuring one pair of empty snapshots first.
    let diagnostics_before = pooled_reader_checkouts(&rt).await;
    let diagnostics_after = pooled_reader_checkouts(&rt).await;
    let diagnostics_cost = diagnostics_after - diagnostics_before;
    let before_query = pooled_reader_checkouts(&rt).await;
    let query_hits = rt
        .neighbors_with_query_page(
            &token,
            root_id.parse().expect("root UUID"),
            NeighborQuery {
                direction: parse_direction(Some("out")).expect("direction"),
                relations: None,
                limit: Some(4),
                min_weight: None,
            },
            None,
            None,
            true,
        )
        .await
        .expect("adjacency query");
    let query_cost = pooled_reader_checkouts(&rt).await - before_query;
    assert_eq!(query_hits.len(), 4, "three hits plus a pagination probe");

    let before_summary = pooled_reader_checkouts(&rt).await;
    let summary = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "out", "limit": 3, "projection": "summary"}),
            &registry,
            &token,
        )
        .await
        .expect("summary projection");
    let summary_cost = pooled_reader_checkouts(&rt).await - before_summary;
    assert_eq!(summary["neighbors"].as_array().unwrap().len(), 3);
    assert_eq!(
        summary_cost, query_cost,
        "Summary must perform zero extra edge reads"
    );

    let before_edge = pooled_reader_checkouts(&rt).await;
    rt.get_edge(&token, query_hits[0].edge_id)
        .await
        .expect("edge read")
        .expect("existing edge");
    let edge_cost = pooled_reader_checkouts(&rt).await - before_edge - diagnostics_cost;
    assert!(edge_cost > 0, "edge read must be visible in pool counters");

    let before_record = pooled_reader_checkouts(&rt).await;
    let record = pack
        .dispatch(
            "neighbors",
            json!({"id": root_id, "direction": "out", "limit": 3, "projection": "record"}),
            &registry,
            &token,
        )
        .await
        .expect("record projection");
    let record_cost = pooled_reader_checkouts(&rt).await - before_record;
    assert_eq!(record["neighbors"].as_array().unwrap().len(), 3);
    assert_eq!(
        record_cost,
        query_cost + edge_cost,
        "Record must batch the retained edge IDs into one namespace window"
    );

    let context_over_cap = pack
        .dispatch(
            "context",
            json!({"entity_ids": [root_id], "hops": 0, "limit": 21}),
            &registry,
            &token,
        )
        .await
        .expect("over-cap context must succeed");
    assert_eq!(context_over_cap["requested_limit"], 21);
    assert_eq!(context_over_cap["effective_limit"], 20);
    assert_eq!(context_over_cap["limit_clamped"], true);

    let context_under_cap = pack
        .dispatch(
            "context",
            json!({"entity_ids": [root_id], "hops": 0, "limit": 3}),
            &registry,
            &token,
        )
        .await
        .expect("under-cap context must succeed");
    assert_eq!(context_under_cap["requested_limit"], 3);
    assert_eq!(context_under_cap["effective_limit"], 3);
    assert_eq!(context_under_cap["limit_clamped"], false);
}
