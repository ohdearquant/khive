//! Tests for `depends_on` task dependencies, context entity, and blocker semantics.

mod common;

use common::{assign, pack, rt};
use serde_json::json;

#[tokio::test]
async fn assign_creates_depends_on_edge_between_tasks() {
    use khive_storage::types::{Direction, NeighborQuery};
    use khive_storage::EdgeRelation;

    let rt = rt();
    let pack = pack(rt.clone());

    let blocker = assign(&pack, json!({"title": "write spec"})).await;
    let blocker_full = blocker["full_id"].as_str().unwrap();
    let dependent = assign(
        &pack,
        json!({"title": "implement feature", "depends_on": [blocker_full]}),
    )
    .await;
    let dep_full = dependent["full_id"].as_str().unwrap();

    let dep_uuid = uuid::Uuid::parse_str(dep_full).unwrap();
    let blocker_uuid = uuid::Uuid::parse_str(blocker_full).unwrap();

    let graph = rt
        .graph(&rt.authorize(khive_runtime::Namespace::local()).unwrap())
        .expect("graph store");
    let neighbors = graph
        .neighbors(
            dep_uuid,
            NeighborQuery {
                direction: Direction::Out,
                relations: Some(vec![EdgeRelation::DependsOn]),
                limit: Some(16),
                min_weight: None,
            },
        )
        .await
        .expect("neighbors query");

    let targets: Vec<_> = neighbors.iter().map(|hit| hit.node_id).collect();
    assert!(
        targets.contains(&blocker_uuid),
        "task->task depends_on edge should exist; got targets {targets:?}"
    );
}

#[tokio::test]
async fn assign_rejects_depends_on_when_target_is_non_task_note() {
    use khive_storage::types::PageRequest;

    let rt = rt();
    let pack = pack(rt.clone());

    let other = rt
        .create_note(
            &rt.authorize(khive_runtime::Namespace::local()).unwrap(),
            "observation",
            None,
            "an observation",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("create observation");
    let other_full = other.id.as_hyphenated().to_string();

    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "depends on observation", "depends_on": [other_full]}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("must be a task note"),
        "expected pack edge-rule rejection (task->task only); got: {msg}"
    );

    let notes = rt
        .notes(&rt.authorize(khive_runtime::Namespace::local()).unwrap())
        .expect("note store");
    let task_page = notes
        .query_notes(
            "local",
            Some("task"),
            PageRequest {
                offset: 0,
                limit: 64,
            },
        )
        .await
        .expect("query task notes");
    assert!(
        task_page.items.is_empty(),
        "rejected assign must not persist a task; found {:?}",
        task_page
            .items
            .iter()
            .filter_map(|n| n.name.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn scenario_gtd_c2_next_excludes_tasks_with_incomplete_deps() {
    let pack = pack(rt());

    let blocker = assign(&pack, json!({"title": "blocker", "status": "inbox"})).await;
    let blocker_id = blocker["full_id"].as_str().unwrap().to_string();

    let dependent = assign(
        &pack,
        json!({
            "title": "dependent-task",
            "status": "next",
            "depends_on": [blocker_id]
        }),
    )
    .await;
    let dep_id = dependent["full_id"].as_str().unwrap().to_string();

    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !titles.contains(&"dependent-task"),
        "scenario-gtd C2: next() must not return tasks with incomplete deps; got: {titles:?}"
    );

    pack.dispatch(
        "gtd.transition",
        json!({"id": blocker_id, "status": "done"}),
    )
    .await
    .expect("blocker->done");

    let result2 = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles2: Vec<&str> = result2
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        titles2.contains(&"dependent-task"),
        "scenario-gtd C2: after blocker is done, next() must include dependent task; got: {titles2:?}"
    );

    let _ = dep_id;
}

#[tokio::test]
async fn scenario_gtd_c2_next_includes_tasks_with_no_deps() {
    let pack = pack(rt());
    assign(
        &pack,
        json!({"title": "no-deps-task", "status": "next", "priority": "p1"}),
    )
    .await;

    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        titles.contains(&"no-deps-task"),
        "scenario-gtd C2: task with no deps must appear in next(); got: {titles:?}"
    );
}

#[tokio::test]
async fn scenario_gtd_c2_next_includes_tasks_with_all_deps_done() {
    let pack = pack(rt());

    let b1 = assign(&pack, json!({"title": "dep-done-1", "status": "inbox"})).await;
    let b1_id = b1["full_id"].as_str().unwrap().to_string();
    let b2 = assign(&pack, json!({"title": "dep-done-2", "status": "inbox"})).await;
    let b2_id = b2["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": b1_id, "status": "done"}))
        .await
        .unwrap();
    pack.dispatch("gtd.transition", json!({"id": b2_id, "status": "done"}))
        .await
        .unwrap();

    let dep = assign(
        &pack,
        json!({
            "title": "all-deps-done",
            "status": "next",
            "depends_on": [b1_id, b2_id]
        }),
    )
    .await;
    let dep_id = dep["full_id"].as_str().unwrap().to_string();

    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        titles.contains(&"all-deps-done"),
        "scenario-gtd C2: task with all deps done must appear in next(); got: {titles:?}"
    );

    let _ = dep_id;
}

#[tokio::test]
async fn next_resolves_deps_older_than_500_task_window() {
    use khive_storage::note::Note;
    use khive_storage::types::PageRequest;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    let blocker_id = uuid::Uuid::new_v4();
    let old_ts = chrono::Utc::now().timestamp_micros() - 1_000_000_000_000;
    let blocker = Note {
        id: blocker_id,
        namespace: "local".to_string(),
        kind: "task".to_string(),
        status: "active".to_string(),
        name: Some("ancient-blocker".to_string()),
        content: "ancient blocker task".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({"status": "done"})),
        created_at: old_ts,
        updated_at: old_ts,
        deleted_at: None,
    };
    note_store
        .upsert_note(blocker)
        .await
        .expect("insert blocker");

    let now = chrono::Utc::now().timestamp_micros();
    let fillers: Vec<Note> = (0..500_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("filler-{i}")),
            content: format!("filler task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "inbox"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(fillers)
        .await
        .expect("insert fillers");

    let fixture = pack(runtime);

    let blocker_full = blocker_id.as_hyphenated().to_string();
    let dep = assign(
        &fixture,
        json!({
            "title": "unblocked-by-ancient",
            "status": "next",
            "depends_on": [blocker_full]
        }),
    )
    .await;
    let dep_id = dep["full_id"].as_str().unwrap().to_string();

    let result = fixture.dispatch("gtd.next", json!({})).await.unwrap();
    let found = result
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["title"].as_str() == Some("unblocked-by-ancient"));
    assert!(
        found,
        "next() must surface task whose done blocker is older than 500-task window; \
         result: {result:?}"
    );

    let _ = dep_id;
    let _ = PageRequest {
        offset: 0,
        limit: 1,
    };
}

#[tokio::test]
async fn assign_context_entity_id_round_trips_through_tasks_and_get() {
    let rt = rt();
    let pack = pack(rt);

    let entity = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Context Entity"}),
        )
        .await
        .expect("context entity create must succeed");
    let context_id = entity["id"].as_str().unwrap().to_string();

    let assigned = assign(
        &pack,
        json!({"title": "task with context", "context_entity_id": context_id}),
    )
    .await;
    let task_id = assigned["full_id"].as_str().unwrap().to_string();

    assert_eq!(
        assigned["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );
    assert_eq!(
        assigned["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let tasks = pack
        .dispatch("gtd.tasks", json!({"status": "inbox"}))
        .await
        .expect("tasks listing must succeed");
    let task = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["full_id"].as_str() == Some(task_id.as_str()))
        .expect("created task must be in tasks(status=inbox)");
    assert_eq!(
        task["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );
    assert_eq!(
        task["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let got = pack
        .dispatch("get", json!({"id": task_id}))
        .await
        .expect("get task must succeed");
    assert_eq!(
        got["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let neighbors = pack
        .dispatch("neighbors", json!({"id": task_id, "direction": "out"}))
        .await
        .expect("neighbors must succeed");
    let has_annotates_edge = neighbors.as_array().unwrap().iter().any(|n| {
        n.to_string().contains("annotates") && n.to_string().contains(context_id.as_str())
    });
    assert!(
        has_annotates_edge,
        "task should have an annotates edge to the context entity; neighbors: {neighbors}"
    );
}

#[tokio::test]
async fn assign_rejects_malformed_context_entity_id() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "bad context", "context_entity_id": "not-a-uuid"}),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("context_entity_id"),
        "error must name the bad field; got: {msg}"
    );
    assert!(
        msg.contains("full UUID"),
        "error must explain expected shape; got: {msg}"
    );
    assert!(
        msg.contains("not-a-uuid"),
        "error must echo the malformed value; got: {msg}"
    );
}
