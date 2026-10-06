use std::collections::BTreeSet;

use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{
    Direction, EventFilter, PageRequest, TraversalExecutionBudget, TraversalOptions,
    TraversalRequest,
};
use khive_types::{EdgeRelation, EventKind, Pack};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, VerbRegistry, NamespaceToken) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry, token)
}

fn id(value: &Value) -> Uuid {
    value["id"].as_str().expect("id").parse().expect("UUID")
}

async fn entity(registry: &VerbRegistry, kind: &str, name: &str) -> Uuid {
    id(&registry
        .dispatch("create", json!({"kind": kind, "name": name}))
        .await
        .expect("create entity"))
}

async fn holding(registry: &VerbRegistry, source: Uuid, target: Uuid, metadata: Value) -> Value {
    registry
        .dispatch(
            "link",
            json!({
                "source_id": source, "target_id": target, "relation": "owns",
                "weight": 1.0, "metadata": metadata,
            }),
        )
        .await
        .expect("create ownership edge")
}

fn assert_endpoint_refusal(result: Result<Value, RuntimeError>, fragment: &str) {
    match result {
        Err(RuntimeError::InvalidInput(message)) => {
            assert!(message.contains(fragment), "{message}");
            assert!(!message.contains("Unknown edge relation"), "{message}");
        }
        other => panic!("expected endpoint InvalidInput, got {other:?}"),
    }
}

#[tokio::test]
async fn owns_accepts_only_person_or_org_to_org_and_refuses_self_loops() {
    let (_runtime, registry, _token) = fixture();
    let mut pairs = Vec::new();
    for kind in KgPack::ENTITY_KINDS {
        pairs.push((
            *kind,
            entity(&registry, kind, &format!("{kind} source")).await,
            entity(&registry, kind, &format!("{kind} target")).await,
        ));
    }
    for &(source_kind, source, _) in &pairs {
        for &(target_kind, _, target) in &pairs {
            let result = registry
                .dispatch(
                    "link",
                    json!({
                        "source_id": source, "target_id": target, "relation": "owns",
                    }),
                )
                .await;
            if matches!(source_kind, "person" | "org") && target_kind == "org" {
                assert_eq!(result.expect("allowed ownership pair")["relation"], "owns");
            } else {
                assert_endpoint_refusal(result, "Invalid relation \"owns\"");
            }
        }
    }
    let org = pairs.iter().find(|(kind, _, _)| *kind == "org").unwrap().1;
    assert_endpoint_refusal(
        registry
            .dispatch(
                "link",
                json!({
                    "source_id": org, "target_id": org, "relation": "owns",
                }),
            )
            .await,
        "self-loop",
    );
    let note = registry
        .dispatch(
            "create",
            json!({
                "kind": "observation", "content": "an ownership report",
            }),
        )
        .await
        .expect("create note");
    for (source, target) in [(id(&note), org), (org, id(&note))] {
        assert_endpoint_refusal(
            registry
                .dispatch(
                    "link",
                    json!({
                        "source_id": source, "target_id": target, "relation": "owns",
                    }),
                )
                .await,
            "must be an entity",
        );
    }
}

#[tokio::test]
async fn reciprocal_owns_preserves_direction_and_independent_metadata() {
    let (runtime, registry, token) = fixture();
    let a = entity(&registry, "org", "A").await;
    let b = entity(&registry, "org", "B").await;
    let (high, low) = if a > b { (a, b) } else { (b, a) };
    let forward = holding(&registry, high, low, json!({"pct": 6})).await;
    let reverse = holding(&registry, low, high, json!({"pct": 7})).await;
    assert_ne!(id(&forward), id(&reverse));
    for (edge_id, source, target, pct) in
        [(id(&forward), high, low, 6), (id(&reverse), low, high, 7)]
    {
        let stored = runtime.get_edge(&token, edge_id).await.unwrap().unwrap();
        assert_eq!((stored.source_id, stored.target_id), (source, target));
        assert_eq!(stored.metadata.unwrap()["pct"], pct);
    }
    for (direction, edge_id) in [
        (Direction::Out, id(&forward)),
        (Direction::In, id(&reverse)),
    ] {
        let hits = runtime
            .neighbors(
                &token,
                high,
                direction,
                None,
                Some(vec![EdgeRelation::Owns]),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node_id, low);
        assert_eq!(hits[0].edge_id, edge_id);
    }
}

#[tokio::test]
async fn owns_metadata_updates_and_ended_holdings_follow_normal_edge_lifecycle() {
    let (runtime, registry, token) = fixture();
    let owner = entity(&registry, "person", "Jane").await;
    let target = entity(&registry, "org", "Target").await;
    let metadata = json!({"pct": 0.5, "as_of": "2026-10-05", "valid_from": "2026-01-01"});
    let created = holding(&registry, owner, target, metadata.clone()).await;
    let edge_id = id(&created);
    let fetched = registry
        .dispatch("get", json!({"id": edge_id}))
        .await
        .unwrap();
    assert_eq!(fetched["metadata"], metadata);
    assert_eq!(fetched["weight"], 1.0);
    let multi_class = json!({"by_class": [
        {"class": "A", "pct": 0.5, "as_of": "2026-10-06"},
        {"class": "B", "pct": 2.0, "as_of": "2026-10-06"},
    ]});
    let updated = holding(&registry, owner, target, multi_class.clone()).await;
    assert_eq!(id(&updated), edge_id);
    assert_eq!(updated["mutation"], "updated");
    assert_eq!(updated["metadata"], multi_class);
    assert!(updated["metadata"].get("pct").is_none());
    assert!(updated["metadata"].get("class").is_none());
    assert!(runtime.delete_edge(&token, edge_id, false).await.unwrap());
    assert!(runtime
        .neighbors(
            &token,
            owner,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Owns])
        )
        .await
        .unwrap()
        .is_empty());
    let restored = registry
        .dispatch(
            "link",
            json!({
                "source_id": owner, "target_id": target, "relation": "owns",
                "resurrect": true, "weight": 1.0, "metadata": {"pct": 1.0},
            }),
        )
        .await
        .expect("restore a current holding");
    assert_eq!(id(&restored), edge_id);
    assert_eq!(restored["mutation"], "resurrected");
    assert_eq!(restored["metadata"]["pct"], 1.0);
    let hits = runtime
        .neighbors(
            &token,
            owner,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Owns]),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].edge_id, edge_id);
}

#[tokio::test]
async fn owns_paths_expose_real_edges_without_materializing_transitive_holdings() {
    let (runtime, registry, token) = fixture();
    let a = entity(&registry, "org", "A").await;
    let b = entity(&registry, "org", "B").await;
    let c = entity(&registry, "org", "C").await;
    let ab = holding(&registry, a, b, json!({"pct": 50})).await;
    let bc = holding(&registry, b, c, json!({"pct": 40})).await;
    let paths = runtime
        .traverse(
            &token,
            TraversalRequest {
                roots: vec![a],
                options: TraversalOptions {
                    max_depth: 2,
                    direction: Direction::Out,
                    relations: Some(vec![EdgeRelation::Owns]),
                    ..Default::default()
                },
                include_roots: false,
                include_properties: false,
                execution_budget: TraversalExecutionBudget::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(paths.len(), 1);
    let nodes = &paths[0].nodes;
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.node_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([b, c])
    );
    for (node_id, edge_id, pct) in [(b, id(&ab), 50), (c, id(&bc), 40)] {
        assert_eq!(
            nodes
                .iter()
                .find(|node| node.node_id == node_id)
                .unwrap()
                .via_edge,
            Some(edge_id)
        );
        let fetched = registry
            .dispatch("get", json!({"id": edge_id}))
            .await
            .unwrap();
        assert_eq!(fetched["metadata"]["pct"], pct);
    }
    let direct = runtime
        .neighbors(
            &token,
            a,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Owns]),
        )
        .await
        .unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].node_id, b);
    assert_eq!(direct[0].edge_id, id(&ab));
}

async fn cascade_warnings(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    target: Uuid,
) -> Vec<Value> {
    runtime
        .events(token)
        .unwrap()
        .query_events(
            EventFilter {
                target_id: Some(target),
                kinds: vec![EventKind::Audit],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| {
            event.payload["severity"] == "warning" && event.payload.get("warning").is_some()
        })
        .map(|event| event.payload)
        .collect()
}

#[tokio::test]
async fn owns_cascades_without_loss_warnings_and_event_reader_observes_provenance_loss() {
    let (runtime, registry, token) = fixture();
    let document = entity(&registry, "document", "Source document").await;
    let artifact = entity(&registry, "artifact", "Derived artifact").await;
    registry
        .dispatch(
            "link",
            json!({
                "source_id": artifact, "target_id": document, "relation": "derived_from",
            }),
        )
        .await
        .unwrap();
    assert!(runtime.delete_entity(&token, document, true).await.unwrap());
    let warnings = cascade_warnings(&runtime, &token, document).await;
    assert_eq!(
        warnings.len(),
        1,
        "the same reader must first observe a real cascade warning"
    );
    assert_eq!(warnings[0]["warning"], "provenance_loss");
    assert_eq!(warnings[0]["relation"], "derived_from");
    for delete_source in [false, true] {
        let owner = entity(&registry, "org", "Owner").await;
        let owned = entity(&registry, "org", "Owned").await;
        let edge_id = id(&holding(&registry, owner, owned, json!({"pct": 80})).await);
        let doomed = if delete_source { owner } else { owned };
        assert!(runtime.delete_entity(&token, doomed, true).await.unwrap());
        assert!(runtime.get_edge(&token, edge_id).await.unwrap().is_none());
        assert!(cascade_warnings(&runtime, &token, doomed).await.is_empty());
    }
}

#[tokio::test]
async fn certificate_contains_and_membership_assumptions_match_loaded_kg_rules() {
    let (_runtime, registry, _token) = fixture();
    let parent = entity(&registry, "org", "Parent").await;
    let subsidiary = entity(&registry, "org", "Subsidiary").await;
    let target = entity(&registry, "org", "Target").await;
    let fund = entity(&registry, "org", "Index Fund").await;
    let jane = entity(&registry, "person", "Jane").await;
    let john = entity(&registry, "person", "John").await;
    for (source, destination) in [(subsidiary, parent), (jane, target), (john, target)] {
        registry
            .dispatch(
                "link",
                json!({
                    "source_id": source, "target_id": destination, "relation": "part_of",
                }),
            )
            .await
            .expect("ADR fixture membership is admitted by the loaded KG rules");
    }
    registry
        .dispatch(
            "link",
            json!({
                "source_id": target, "target_id": fund, "relation": "contains",
            }),
        )
        .await
        .expect("org-to-org converse encoding is admitted");
    assert_endpoint_refusal(
        registry
            .dispatch(
                "link",
                json!({
                    "source_id": target, "target_id": jane, "relation": "contains",
                }),
            )
            .await,
        "Invalid relation \"contains\"",
    );
    for (source, destination, pct) in [
        (parent, subsidiary, 80.0),
        (fund, subsidiary, 6.0),
        (fund, target, 7.0),
        (jane, target, 0.5),
    ] {
        holding(&registry, source, destination, json!({"pct": pct})).await;
    }
}
