//! Persisted edge-cycle decisions through the registered GTD hook, before write triggers.

mod common;

use chrono::Utc;
use common::{pack, rt, Fixture};
use khive_runtime::{KhiveRuntime, LinkSpec, Namespace, RuntimeError};
use khive_storage::types::Edge;
use khive_storage::{EdgeRelation, Entity, Note, SqlStatement, SqlValue};
use serde_json::json;
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn task(value: u128) -> Note {
    let mut note = Note::new("local", "task", "edge dependency fixture");
    note.id = id(value);
    note.properties = Some(json!({"status": "next"}));
    note
}

fn edge(source: u128, target: u128) -> Edge {
    let now = Utc::now();
    Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id: id(source),
        target_id: id(target),
        relation: EdgeRelation::DependsOn,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    }
}

fn link(source: u128, target: u128) -> LinkSpec {
    LinkSpec {
        namespace: None,
        source_id: id(source),
        target_id: id(target),
        relation: EdgeRelation::DependsOn,
        weight: 1.0,
        metadata: None,
        resurrect: false,
    }
}

async fn check(
    runtime: &KhiveRuntime,
    fixture: &Fixture,
    links: &[LinkSpec],
) -> Result<(), RuntimeError> {
    let token = runtime.authorize(Namespace::local()).expect("local token");
    fixture
        .registry
        .validate_link_hooks(runtime, &token, links)
        .await
}

#[tokio::test]
async fn edge_cycle_hook_combines_long_persisted_paths_and_the_whole_proposed_batch() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_notes((1..=16).map(task).collect())
        .await
        .expect("seed tasks");
    let graph = runtime.graph(&token).unwrap();
    graph
        .upsert_edges((1..14).map(|source| edge(source, source + 1)).collect())
        .await
        .expect("seed thirteen-hop persisted path");

    check(&runtime, &fixture, &[link(16, 1), link(15, 16)])
        .await
        .expect("an acyclic batch stays valid beyond the public traversal depth of ten");
    for links in [[link(14, 15), link(15, 1)], [link(15, 1), link(14, 15)]] {
        let error = check(&runtime, &fixture, &links)
            .await
            .expect_err("persisted path plus both proposed edges closes a cycle");
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error
            .to_string()
            .contains("depends_on link would create a dependency cycle"));
    }
    assert!(graph
        .get_edge_by_natural_key_including_deleted("local", id(14), id(15), EdgeRelation::DependsOn)
        .await
        .unwrap()
        .is_none());
    assert!(graph
        .get_edge_by_natural_key_including_deleted("local", id(15), id(1), EdgeRelation::DependsOn)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn edge_cycle_pages_equal_weights_past_excluded_rows_and_keeps_by_id_namespace_semantics() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let mut foreign = task(1_000);
    foreign.namespace = "foreign-task-attribution".into();
    runtime
        .notes(&token)
        .unwrap()
        .upsert_notes(vec![task(1), foreign, task(2_000)])
        .await
        .expect("seed differently attributed task endpoint");
    let mut edges: Vec<_> = (2..=300).map(|target| edge(1, target)).collect();
    edges.extend([edge(1, 1_000), edge(1_000, 2_000)]);
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edges(edges)
        .await
        .unwrap();

    let observation = runtime
        .backend()
        .pool()
        .observe_test_statement_starts(128)
        .expect("observe hook reads");
    let error = check(&runtime, &fixture, &[link(2_000, 1)])
        .await
        .expect_err("the live task path after page one is still a cycle");
    let starts = observation
        .started_statements()
        .expect("complete observation");
    drop(observation);
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(error
        .to_string()
        .contains("depends_on link would create a dependency cycle"));
    assert!(starts.iter().all(|start| start.readonly));
    assert!(
        starts
            .iter()
            .filter(|start| {
                start.sql.contains("FROM graph_edges")
                    && start
                        .sql
                        .contains("ORDER BY weight DESC, node_id ASC, edge_id ASC")
            })
            .count()
            >= 3,
        "at least two pages for the first source and one for the foreign task"
    );
}

#[tokio::test]
async fn edge_cycle_hook_excludes_non_task_endpoints_and_nonmatching_edges() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let mut wrong_kind = task(3);
    wrong_kind.kind = "observation".into();
    let mut deleted_task = task(4);
    deleted_task.deleted_at = Some(1);
    runtime
        .notes(&token)
        .unwrap()
        .upsert_notes(vec![
            task(1),
            task(2),
            wrong_kind,
            deleted_task,
            task(6),
            task(7),
            task(8),
            task(10),
        ])
        .await
        .unwrap();
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(Entity::minimal(
            id(9),
            "local",
            "concept",
            "not a task note",
            1,
            1,
        ))
        .await
        .unwrap();
    let mut edges = Vec::new();
    // 5 is absent; 9 is an entity. Neither can join the live task-note graph.
    for middle in [3, 4, 5, 9] {
        edges.extend([edge(1, middle), edge(middle, 2)]);
    }
    let mut foreign_edge = edge(1, 6);
    foreign_edge.namespace = "foreign-edge-attribution".into();
    let mut deleted_edge = edge(1, 7);
    deleted_edge.deleted_at = Some(Utc::now());
    let mut unrelated_edge = edge(1, 8);
    unrelated_edge.relation = EdgeRelation::Enables;
    edges.extend([
        foreign_edge,
        edge(6, 2),
        deleted_edge,
        edge(7, 2),
        unrelated_edge,
        edge(8, 2),
    ]);
    let graph = runtime.graph(&token).unwrap();
    graph
        .upsert_edges(edges)
        .await
        .expect("seed excluded paths");
    check(&runtime, &fixture, &[link(2, 1)])
        .await
        .expect("excluded task and edge paths do not form a live dependency cycle");

    graph
        .upsert_edges(vec![edge(1, 10), edge(10, 2)])
        .await
        .unwrap();
    let error = check(&runtime, &fixture, &[link(2, 1)])
        .await
        .expect_err("live control path closes the cycle");
    assert!(error
        .to_string()
        .contains("depends_on link would create a dependency cycle"));
}

#[tokio::test]
async fn edge_cycle_raw_scan_counts_excluded_neighbors_and_fails_only_after_the_cap() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let source = common::assign(&fixture, json!({"title": "raw scan source"})).await;
    let source_id = Uuid::parse_str(source["full_id"].as_str().unwrap()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(task(1))
        .await
        .unwrap();
    let graph = runtime.graph(&token).unwrap();
    graph
        .upsert_edges((10_000..30_000).map(|target| edge(1, target)).collect())
        .await
        .expect("seed twenty thousand excluded neighbors in one batch");
    check(&runtime, &fixture, &[link(source_id.as_u128(), 1)])
        .await
        .expect("exactly the scan cap is exhausted without a task path");
    graph.upsert_edge(edge(1, 30_000)).await.unwrap();
    let error = check(&runtime, &fixture, &[link(source_id.as_u128(), 1)])
        .await
        .expect_err("one more excluded row must not evade the raw scan budget");
    assert_eq!(
        error.to_string(),
        "invalid input: depends_on edge cycle validation exceeded the 20000-scanned-neighbor safety bound"
    );
}

#[tokio::test]
async fn edge_cycle_restores_uuid_bfs_order_and_propagates_reached_decode_errors() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_notes(vec![task(1), task(2), task(3)])
        .await
        .unwrap();
    let mut low_weight_goal = edge(1, 2);
    low_weight_goal.weight = 0.1;
    let malformed = edge(3, 4);
    let malformed_id = malformed.id.to_string();
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edges(vec![low_weight_goal, edge(1, 3), malformed])
        .await
        .unwrap();
    // A malformed legacy edge is only relevant if its source is actually expanded.
    runtime.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "UPDATE graph_edges SET id = 'invalid-edge-id' WHERE namespace = 'local' AND id = ?1".into(),
        params: vec![SqlValue::Text(malformed_id)],
        label: Some("gtd_cycle_malformed_edge_fixture".into()),
    }).await.unwrap();

    let error = check(&runtime, &fixture, &[link(2, 1)])
        .await
        .expect_err("lower UUID goal precedes the higher-weight branch");
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(error
        .to_string()
        .contains("depends_on link would create a dependency cycle"));
    let error = check(&runtime, &fixture, &[link(2, 3)])
        .await
        .expect_err("reached malformed neighbor must fail closed");
    assert!(matches!(error, RuntimeError::Storage(_)), "{error}");
}
