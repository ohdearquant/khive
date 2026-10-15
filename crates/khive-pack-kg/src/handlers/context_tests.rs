use super::*;

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn context_anchor_edges_share_the_record_budget_and_keep_admitted_endpoints() {
    use khive_runtime::{RuntimeConfig, VerbRegistryBuilder};

    let namespace = format!("context-anchor-budget-{}", Uuid::new_v4().simple());
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace(namespace.clone());
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.call_register_entity_type_validators(&runtime);
    let mut ids = Vec::new();
    for label in ["甲", "乙", "丙", "丁"] {
        let mut create_args = json!({
            "kind": "concept", "name": label.repeat(40),
            "namespace": namespace, "skip_dedup_check": true,
        });
        // The second anchor's record costs more than one anchor edge, so a budget can cut
        // that anchor while an edge naming it would still fit.
        if label == "乙" {
            create_args["description"] = json!(label.repeat(400));
        }
        let entity = registry.dispatch("create", create_args).await.unwrap();
        ids.push(entity["id"].as_str().unwrap().to_owned());
    }
    for (source, target) in [(0, 1), (1, 2), (0, 3)] {
        registry
            .dispatch(
                "link",
                json!({
                    "source_id": ids[source], "target_id": ids[target],
                    "relation": "contains", "weight": 0.8, "namespace": namespace,
                }),
            )
            .await
            .unwrap();
    }
    let args = json!({
        "entity_ids": &ids[..3], "hops": 1, "direction": "outgoing",
        "budget": 65536, "namespace": namespace,
    });
    let full = registry.dispatch("context", args.clone()).await.unwrap();
    let full_anchor_edges = full["anchor_edges"].as_array().unwrap();
    assert_eq!(full_anchor_edges.len(), 2, "{full}");
    assert_eq!(full["edges"].as_array().unwrap().len(), 1, "{full}");
    assert_eq!(full["dropped"]["anchor_edges"], 0);

    let cost = |record: &Value| record.to_string().chars().count();
    let blocks: Vec<AnchorBlock> = full["anchors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|anchor| AnchorBlock {
            entity_json: anchor["entity"].clone(),
            entity_size: cost(&anchor["entity"]),
            pairs: anchor["neighbors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|neighbor| {
                    let edge = full["edges"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|edge| {
                            edge["source_id"] == anchor["entity"]["id"]
                                && edge["target_id"] == neighbor["id"]
                        })
                        .unwrap();
                    ContextPair::new(neighbor.clone(), edge.clone()).unwrap()
                })
                .collect(),
        })
        .collect();
    let earlier_cost: usize = blocks
        .iter()
        .map(|block| block.entity_size + block.pairs.iter().map(|pair| pair.size).sum::<usize>())
        .sum();
    let first_edge_cost = cost(&full_anchor_edges[0]);
    let complete_cost = earlier_cost + full_anchor_edges.iter().map(cost).sum::<usize>();
    assert!(earlier_cost > 256);
    assert!(full_anchor_edges[0].to_string().len() > first_edge_cost);
    // Admit the first anchor and exactly one anchor edge's cost: the second anchor is cut, the
    // first anchor's neighbor pair does not fit, and the edge from the first anchor to the cut
    // second anchor fits on cost alone, so only endpoint admission keeps it out.
    assert_eq!(blocks[0].entity_json["id"], ids[0].as_str());
    assert_eq!(blocks[1].entity_json["id"], ids[1].as_str());
    let cut_endpoint_edge = full_anchor_edges
        .iter()
        .find(|edge| edge["source_id"] == ids[0].as_str() && edge["target_id"] == ids[1].as_str())
        .unwrap();
    let cut_endpoint_budget = blocks[0].entity_size + cost(cut_endpoint_edge);
    assert!(cost(cut_endpoint_edge) < blocks[1].entity_size);
    assert!(blocks[0]
        .pairs
        .iter()
        .all(|pair| pair.size > cost(cut_endpoint_edge)));
    for budget in [
        256,
        cut_endpoint_budget,
        earlier_cost,
        earlier_cost + first_edge_cost - 1,
        earlier_cost + first_edge_cost,
        complete_cost,
    ] {
        let mut bounded_args = args.clone();
        bounded_args["budget"] = json!(budget);
        let bounded = registry.dispatch("context", bounded_args).await.unwrap();
        let (anchors, edges, _, dropped_anchors, dropped_neighbors, used) =
            assemble_within_budget(&blocks, budget);
        assert_eq!(bounded["anchors"], json!(anchors), "budget {budget}");
        assert_eq!(bounded["edges"], json!(edges), "budget {budget}");
        assert_eq!(bounded["dropped"]["anchors"], dropped_anchors);
        assert_eq!(bounded["dropped"]["neighbors"], dropped_neighbors);
        assert_eq!(bounded["dropped"]["edges"], dropped_neighbors);
        let admitted: HashSet<&str> = bounded["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|anchor| anchor["entity"]["id"].as_str().unwrap())
            .collect();
        let anchor_edges = bounded["anchor_edges"].as_array().unwrap();
        assert!(anchor_edges.iter().all(|edge| {
            admitted.contains(edge["source_id"].as_str().unwrap())
                && admitted.contains(edge["target_id"].as_str().unwrap())
        }));
        assert!(used + anchor_edges.iter().map(cost).sum::<usize>() <= budget);
        assert_eq!(
            bounded["dropped"]["anchor_edges"],
            full_anchor_edges.len() - anchor_edges.len()
        );
        if anchor_edges.len() < full_anchor_edges.len() {
            assert_eq!(bounded["truncated"], true);
        }
        if budget == cut_endpoint_budget {
            assert_eq!(
                bounded["anchors"].as_array().unwrap().len(),
                1,
                "budget {budget}"
            );
            assert!(anchor_edges.is_empty(), "budget {budget}");
            assert_eq!(bounded["truncated"], true);
        }
        if budget == earlier_cost + first_edge_cost {
            assert_eq!(anchor_edges, &full_anchor_edges[..1]);
        }
        if budget == complete_cost {
            assert_eq!(bounded["anchor_edges"], full["anchor_edges"]);
            assert_eq!(bounded["truncated"], false);
        }
    }
}

fn anchor_block(name: &str, entity_size_filler: usize, neighbor_sizes: &[usize]) -> AnchorBlock {
    let entity_json = json!({ "id": name, "filler": "x".repeat(entity_size_filler) });
    let entity_size = compact_len(&entity_json).unwrap();
    let pairs = neighbor_sizes
        .iter()
        .enumerate()
        .map(|(idx, &sz)| {
            let nj = json!({ "id": format!("{name}-n{idx}"), "filler": "x".repeat(sz) });
            let edge = json!({ "source_id": name, "target_id": nj["id"] });
            ContextPair::new(nj, edge).unwrap()
        })
        .collect();
    AnchorBlock {
        entity_json,
        entity_size,
        pairs,
    }
}

#[test]
fn relations_all_symmetric_true_for_only_symmetric_relations() {
    assert!(relations_all_symmetric(Some(&[EdgeRelation::CompetesWith])));
    assert!(relations_all_symmetric(Some(&[
        EdgeRelation::CompetesWith,
        EdgeRelation::ComposedWith
    ])));
}

#[test]
fn relations_all_symmetric_false_for_mixed_or_absent() {
    assert!(!relations_all_symmetric(None));
    assert!(!relations_all_symmetric(Some(&[])));
    assert!(!relations_all_symmetric(Some(&[EdgeRelation::Extends])));
    assert!(!relations_all_symmetric(Some(&[
        EdgeRelation::CompetesWith,
        EdgeRelation::Extends
    ])));
}

#[test]
fn assemble_within_budget_no_truncation_when_everything_fits() {
    let blocks = vec![anchor_block("a1", 4, &[4, 4]), anchor_block("a2", 4, &[4])];
    let total: usize = blocks
        .iter()
        .map(|b| b.entity_size + b.pairs.iter().map(|pair| pair.size).sum::<usize>())
        .sum();
    let (out, edges, truncated, d_anchors, d_neighbors, _) = assemble_within_budget(&blocks, total);
    assert!(!truncated);
    assert_eq!(d_anchors, 0);
    assert_eq!(d_neighbors, 0);
    assert_eq!(
        edges.len(),
        out.iter()
            .map(|anchor| anchor["neighbors"].as_array().unwrap().len())
            .sum::<usize>()
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["neighbors"].as_array().unwrap().len(), 2);
    assert_eq!(out[1]["neighbors"].as_array().unwrap().len(), 1);
}

#[test]
fn assemble_within_budget_exact_boundary_is_not_truncated() {
    // A budget exactly equal to the cumulative size must NOT truncate —
    // the spec's stop condition is "would push the running total PAST
    // budget", i.e. a record landing exactly on the boundary still fits.
    let blocks = vec![anchor_block("a1", 4, &[4])];
    let exact_total = blocks[0].entity_size + blocks[0].pairs[0].size;
    let (out, edges, truncated, d_anchors, d_neighbors, _) =
        assemble_within_budget(&blocks, exact_total);
    assert!(
        !truncated,
        "exact-fit budget must not be reported as truncated"
    );
    assert_eq!(d_anchors, 0);
    assert_eq!(d_neighbors, 0);
    assert_eq!(
        edges.len(),
        out.iter()
            .map(|anchor| anchor["neighbors"].as_array().unwrap().len())
            .sum::<usize>()
    );
    assert_eq!(out[0]["neighbors"].as_array().unwrap().len(), 1);
}

#[test]
fn assemble_within_budget_one_char_over_boundary_truncates_the_overflowing_record() {
    let blocks = vec![anchor_block("a1", 4, &[4, 4])];
    let entity_size = blocks[0].entity_size;
    let first_neighbor_size = blocks[0].pairs[0].size;
    // Budget fits the anchor entity plus the first neighbor exactly, but not
    // a single Unicode scalar more — the second neighbor must be dropped.
    let budget = entity_size + first_neighbor_size;
    let (out, edges, truncated, d_anchors, d_neighbors, _) =
        assemble_within_budget(&blocks, budget);
    assert!(truncated);
    assert_eq!(d_anchors, 0, "the anchor's entity record itself fit");
    assert_eq!(
        d_neighbors, 1,
        "exactly the overflowing neighbor is dropped"
    );
    assert_eq!(out[0]["neighbors"].as_array().unwrap().len(), 1);
    assert_eq!(edges.len(), 1);
}

#[test]
fn assemble_within_budget_anchor_entity_overflow_drops_whole_anchor_and_all_after_it() {
    let blocks = vec![anchor_block("a1", 4, &[4]), anchor_block("a2", 4, &[4])];
    // Budget too small even for the first anchor's entity record.
    let budget = blocks[0].entity_size - 1;
    let (out, edges, truncated, d_anchors, d_neighbors, _) =
        assemble_within_budget(&blocks, budget);
    assert!(truncated);
    assert!(out.is_empty(), "no anchor entity fit at all");
    assert!(edges.is_empty());
    assert_eq!(d_anchors, 2, "both anchors dropped");
    assert_eq!(d_neighbors, 2, "both anchors' single neighbor each dropped");
}

#[test]
fn assemble_within_budget_first_anchor_bloated_neighbor_no_longer_starves_second_anchor() {
    // Regression test for the context-verb anchor-starvation defect: a bloated
    // neighbor list on a higher-ranked anchor must never push a lower-ranked
    // (but still selected) anchor's own entity record out of the result. Only
    // that first anchor's oversized neighbor should be dropped.
    let blocks = vec![anchor_block("a1", 4, &[2000]), anchor_block("a2", 4, &[4])];
    let budget = blocks[0].entity_size + blocks[1].entity_size + blocks[1].pairs[0].size;
    let (out, edges, truncated, d_anchors, d_neighbors, _) =
        assemble_within_budget(&blocks, budget);
    assert!(truncated);
    assert_eq!(
        out.len(),
        2,
        "both anchors' entity records must survive a bloated sibling neighbor list"
    );
    assert_eq!(
        out[1]["entity"]["id"], "a2",
        "second anchor's entity must be present"
    );
    assert_eq!(d_anchors, 0, "no anchor entity is dropped");
    assert_eq!(
        d_neighbors, 1,
        "only the oversized first-anchor neighbor is dropped"
    );
    assert_eq!(
        out[0]["neighbors"].as_array().unwrap().len(),
        0,
        "first anchor's oversized neighbor did not fit"
    );
    assert_eq!(
        out[1]["neighbors"].as_array().unwrap().len(),
        1,
        "second anchor's small neighbor still fits"
    );
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["source_id"], "a2");
}

#[test]
fn assemble_within_budget_hop_and_via_pass_through_untouched() {
    let hop1 = json!({ "id": "n1", "hop": 1, "via": Value::Null });
    let hop2 = json!({ "id": "n2", "hop": 2, "via": "n1" });
    let pair1 = ContextPair::new(hop1, json!({ "hop": 1, "via": null })).unwrap();
    let pair2 = ContextPair::new(hop2, json!({ "hop": 2, "via": "n1" })).unwrap();
    let size = pair1.size + pair2.size;
    let block = AnchorBlock {
        entity_json: json!({ "id": "a1" }),
        entity_size: compact_len(&json!({ "id": "a1" })).unwrap(),
        pairs: vec![pair1, pair2],
    };
    let budget = block.entity_size + size;
    let (out, edges, truncated, ..) = assemble_within_budget(&[block], budget);
    assert!(!truncated);
    let neighbors = out[0]["neighbors"].as_array().unwrap();
    assert_eq!(neighbors[0]["hop"], 1);
    assert_eq!(neighbors[0]["via"], Value::Null);
    assert_eq!(neighbors[1]["hop"], 2);
    assert_eq!(neighbors[1]["via"], "n1");
    assert_eq!(edges[0]["hop"], 1);
    assert_eq!(edges[0]["via"], Value::Null);
    assert_eq!(edges[1]["hop"], 2);
    assert_eq!(edges[1]["via"], "n1");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn context_pairs_use_scoped_metadata_after_controlled_real_deletion() {
    use khive_runtime::{RuntimeConfig, VerbRegistryBuilder};
    use std::time::Duration;

    async fn entity(registry: &VerbRegistry, namespace: &str, name: &str) -> String {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept", "name": name, "skip_dedup_check": true,
                    "namespace": namespace,
                }),
            )
            .await
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    // None is the barrier's positive control. The other cases delete an
    // actual selected neighbor or via parent after expansion, before hydration.
    for removed in [None, Some("child"), Some("parent")] {
        let namespace = format!("context-race-{}", Uuid::new_v4().simple());
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace(namespace.clone());
        builder.register(KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        registry.call_register_entity_type_validators(&runtime);
        let anchor = entity(&registry, &namespace, "Race anchor").await;
        let parent = entity(&registry, &namespace, "Race parent").await;
        let child = entity(&registry, &namespace, "Race child").await;
        for (source, target) in [(&anchor, &parent), (&parent, &child)] {
            registry
                .dispatch(
                    "link",
                    json!({
                        "source_id": source, "target_id": target,
                        "relation": "contains", "weight": 0.8,
                        "namespace": namespace,
                    }),
                )
                .await
                .unwrap();
        }
        let args = json!({
            "entity_ids": [anchor], "hops": 2, "direction": "outgoing",
            "budget": 65536, "namespace": namespace,
        });
        let before = registry.dispatch("context", args.clone()).await.unwrap();
        assert_eq!(before["edges"].as_array().unwrap().len(), 2, "{before}");
        assert_eq!(before["edges"][1]["via"], parent);
        let mut pause = hydration_pause::arm(&namespace);
        let reader = registry.clone();
        let task = tokio::spawn(async move { reader.dispatch("context", args).await });
        pause.reached().await;
        if let Some(which) = removed {
            let id = if which == "parent" { &parent } else { &child };
            tokio::time::timeout(
                Duration::from_secs(10),
                registry.dispatch("delete", json!({"id": id})),
            )
            .await
            .expect("delete watchdog")
            .expect("actual selected record deletion");
        }
        pause.release();
        let after = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("context completion watchdog")
            .unwrap()
            .unwrap();
        assert_eq!(after["truncated"], false, "{after}");
        assert_eq!(after["dropped"]["anchors"], 0);
        assert_eq!(after["dropped"]["neighbors"], 0);
        assert_eq!(after["dropped"]["edges"], 0);
        assert_eq!(after["anchors"].as_array().unwrap().len(), 1);
        match removed {
            None => assert_eq!(after, before),
            Some("child") => {
                assert_eq!(after["edges"].as_array().unwrap().len(), 1);
                assert_eq!(after["edges"][0]["target_id"], parent);
                assert_eq!(
                    after["anchors"][0]["neighbors"].as_array().unwrap().len(),
                    1
                );
                assert!(!after.to_string().contains(&child));
            }
            Some("parent") => {
                // The child still exists, but cannot acquire an invented
                // parent name or a new visited owner after its parent vanished.
                registry
                    .dispatch("get", json!({"id": child}))
                    .await
                    .unwrap();
                assert!(after["edges"].as_array().unwrap().is_empty());
                assert!(after["anchors"][0]["neighbors"]
                    .as_array()
                    .unwrap()
                    .is_empty());
                assert!(!after.to_string().contains(&parent));
                assert!(!after.to_string().contains(&child));
            }
            _ => unreachable!(),
        }
    }
}
