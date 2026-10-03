use std::sync::Arc;

use chrono::Utc;
use khive_gate::{Gate, GateDecision, GateError, GateRequest};
use khive_pack_kg::KgPack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::{Direction, LinkId, NeighborQuery};
use khive_storage::{Edge, EdgeRelation, Entity};
use serde_json::json;
use uuid::Uuid;

async fn fixture() -> (
    tempfile::TempDir,
    KhiveRuntime,
    NamespaceToken,
    KgPack,
    VerbRegistry,
    Uuid,
) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(dir.path().join("neighbors-batch.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    let pack = KgPack::new(runtime.clone());
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    let root = Entity::new("local", "concept", "batch root");
    let targets: Vec<_> = (0..1001)
        .map(|index| Entity::new("local", "concept", format!("target {index}")))
        .collect();
    let now = Utc::now();
    let edges: Vec<_> = targets
        .iter()
        .enumerate()
        .map(|(index, target)| Edge {
            id: LinkId::from(Uuid::from_u128(10_000 + index as u128)),
            namespace: "local".into(),
            source_id: root.id,
            target_id: target.id,
            relation: EdgeRelation::Supports,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .collect();
    let mut entities = targets;
    entities.push(root.clone());
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entities(entities)
        .await
        .unwrap();
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edges(edges)
        .await
        .unwrap();
    (dir, runtime, token, pack, registry, root.id)
}

fn checkouts(runtime: &KhiveRuntime) -> u64 {
    runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts
}

fn query(limit: u32) -> NeighborQuery {
    NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: Some(limit),
        min_weight: None,
    }
}

#[tokio::test]
async fn original_endpoint_point_read_baseline_1000_is_2000_real_checkouts() {
    let (_dir, runtime, token, _pack, _registry, root) = fixture().await;
    let hits = runtime
        .neighbors_with_query_page(&token, root, query(1000), None, None, false)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1000);
    let before = checkouts(&runtime);
    for hit in &hits {
        assert!(runtime
            .get_edge(&token, hit.edge_id)
            .await
            .unwrap()
            .is_some());
    }
    assert_eq!(
        checkouts(&runtime) - before,
        2000,
        "actual original primitive baseline"
    );
}

#[tokio::test]
async fn neighbors_record_and_edge_1000_use_bounded_actual_reader_work() {
    let (_dir, runtime, token, pack, registry, root) = fixture().await;
    for limit in [1_u32, 900, 901, 1000] {
        for projection in ["record", "edge", "summary"] {
            let enrich = projection != "edge";
            let before = checkouts(&runtime);
            let adjacent = runtime
                .neighbors_with_query_page(&token, root, query(limit + 1), None, None, enrich)
                .await
                .unwrap();
            let baseline = checkouts(&runtime) - before;
            assert_eq!(adjacent.len(), limit as usize + 1);
            let before = checkouts(&runtime);
            let response = pack
                .dispatch(
                    "neighbors",
                    json!({
                        "id": root, "direction": "out", "limit": limit, "projection": projection
                    }),
                    &registry,
                    &token,
                )
                .await
                .unwrap();
            let actual = checkouts(&runtime) - before;
            let rows = response["neighbors"].as_array().unwrap();
            assert_eq!(rows.len(), limit as usize);
            assert!(!response["next_after"].is_null());
            assert_eq!(response["effective_limit"], limit);
            let hydration = if projection == "summary" {
                0
            } else {
                2 * u64::from(limit.div_ceil(900))
            };
            assert_eq!(
                actual,
                baseline + hydration,
                "limit={limit}, projection={projection}"
            );
            for (row, hit) in rows.iter().zip(adjacent.iter()) {
                assert_eq!(row["edge_id"], hit.edge_id.to_string());
                if projection != "summary" {
                    assert_eq!(row["source_id"], root.to_string());
                    assert_eq!(row["target_id"], hit.node_id.to_string());
                } else {
                    assert!(row.get("source_id").is_none());
                }
            }
        }
    }
}

#[derive(Debug)]
struct DenyNeighbors;
impl Gate for DenyNeighbors {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(if request.verb == "neighbors" {
            GateDecision::deny("batch fixture denies neighbors")
        } else {
            GateDecision::allow()
        })
    }
}

#[tokio::test]
async fn neighbors_batch_respects_gate_before_anchor_or_edge_reads() {
    let (_dir, runtime, token, pack, _registry, root) = fixture().await;
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    builder.with_gate(Arc::new(DenyNeighbors));
    builder.with_runtime_event_store(&runtime).unwrap();
    let registry = builder.build().unwrap();
    let error = registry
        .dispatch(
            "neighbors",
            json!({"id": root, "limit": 1000, "projection": "record"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::PermissionDenied { .. }),
        "{error:?}"
    );
    // The direct authorized runtime remains usable; denial is not a forged
    // empty result and does not alter any edge row.
    let rows = runtime
        .get_edge(&token, Uuid::from_u128(10_000))
        .await
        .unwrap();
    assert!(rows.is_some());
}
