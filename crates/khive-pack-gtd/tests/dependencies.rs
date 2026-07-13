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

// ---- #625/#626: gtd.assign / create(kind="note", note_kind="task") parity ----
//
// Both verbs now route through `task_create::prepare_task_create` and
// `task_create::link_depends_on_edges` (see hook.rs and handlers.rs). These
// tests prove the unification didn't just compile — the two paths must
// persist the same normalized properties and produce the same depends_on /
// annotates graph edges for the same logical input.

#[tokio::test]
async fn assign_and_create_task_are_equivalent_for_dependencies_and_context() {
    let rt = rt();
    let pack = pack(rt);

    let blocker = assign(&pack, json!({"title": "blocker"})).await;
    let blocker_id = blocker["full_id"].as_str().unwrap().to_string();

    let context = pack
        .dispatch("create", json!({"kind": "concept", "name": "Context"}))
        .await
        .unwrap();
    let context_id = context["id"].as_str().unwrap().to_string();

    let assigned = pack
        .dispatch(
            "gtd.assign",
            json!({
                "title": "via assign",
                "description": "body",
                "status": "next",
                "priority": "p1",
                "depends_on": [blocker_id.clone()],
                "context_entity_id": context_id.clone(),
                "tags": ["shared"]
            }),
        )
        .await
        .unwrap();

    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "via create",
                "description": "body",
                "status": "next",
                "priority": "p1",
                "depends_on": [blocker_id.clone()],
                "context_entity_id": context_id.clone(),
                "tags": ["shared"]
            }),
        )
        .await
        .unwrap();

    let assigned_id = assigned["full_id"].as_str().unwrap().to_string();
    let created_id = created["id"].as_str().unwrap().to_string();

    for task in [&assigned, &created] {
        assert_eq!(task["properties"]["status"].as_str(), Some("next"));
        assert_eq!(task["properties"]["priority"].as_str(), Some("p1"));
        assert_eq!(
            task["properties"]["depends_on"][0].as_str(),
            Some(blocker_id.as_str())
        );
        assert_eq!(
            task["properties"]["context_entity_id"].as_str(),
            Some(context_id.as_str())
        );
        assert_eq!(task["properties"]["tags"], serde_json::json!(["shared"]));
    }

    for task_id in [assigned_id, created_id] {
        let deps = pack
            .dispatch(
                "neighbors",
                json!({"id": task_id, "direction": "out", "relations": ["depends_on"]}),
            )
            .await
            .expect("neighbors(depends_on) must succeed");
        assert!(
            deps.as_array()
                .unwrap()
                .iter()
                .any(|n| n.to_string().contains(blocker_id.as_str())),
            "task {task_id} must have a depends_on edge to the blocker; got {deps:?}"
        );

        let annotations = pack
            .dispatch(
                "neighbors",
                json!({"id": task_id, "direction": "out", "relations": ["annotates"]}),
            )
            .await
            .expect("neighbors(annotates) must succeed");
        assert!(
            annotations
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.to_string().contains(context_id.as_str())),
            "task {task_id} must have an annotates edge to the context entity; got {annotations:?}"
        );
    }
}

#[tokio::test]
async fn create_task_merges_explicit_annotates_with_context_entity_id() {
    let rt = rt();
    let pack = pack(rt);

    let explicit = pack
        .dispatch("create", json!({"kind": "concept", "name": "Explicit"}))
        .await
        .unwrap();
    let explicit_id = explicit["id"].as_str().unwrap().to_string();

    let context = pack
        .dispatch("create", json!({"kind": "concept", "name": "Context"}))
        .await
        .unwrap();
    let context_id = context["id"].as_str().unwrap().to_string();

    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "both annotates",
                "annotates": [explicit_id.clone()],
                "context_entity_id": context_id.clone(),
            }),
        )
        .await
        .unwrap();
    let task_id = created["id"].as_str().unwrap().to_string();

    let annotations = pack
        .dispatch(
            "neighbors",
            json!({"id": task_id, "direction": "out", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors(annotates) must succeed");
    let annotations = annotations.as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|n| n.to_string().contains(explicit_id.as_str())),
        "task must keep the explicit annotates edge; got {annotations:?}"
    );
    assert!(
        annotations
            .iter()
            .any(|n| n.to_string().contains(context_id.as_str())),
        "task must also have the context_entity_id annotates edge; got {annotations:?}"
    );
}

#[tokio::test]
async fn assign_and_create_task_reject_malformed_context_entity_id() {
    let pack = pack(rt());

    let assign_err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "bad", "context_entity_id": "not-a-uuid"}),
        )
        .await
        .unwrap_err();
    assert!(assign_err.to_string().contains("context_entity_id"));

    let create_err = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "bad",
                "context_entity_id": "not-a-uuid"
            }),
        )
        .await
        .unwrap_err();
    assert!(create_err.to_string().contains("context_entity_id"));
}

// ---- generic create must normalize nested properties -------------------------

#[tokio::test]
async fn create_task_normalizes_nested_priority_and_depends_on_before_write() {
    use khive_storage::types::{Direction, NeighborQuery};
    use khive_storage::EdgeRelation;

    let rt = rt();
    let pack = pack(rt.clone());

    let blocker = assign(&pack, json!({"title": "write spec"})).await;
    let blocker_full = blocker["full_id"].as_str().unwrap().to_string();

    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "generic dependent",
                "properties": {"priority": "p1", "depends_on": [blocker_full.clone()]}
            }),
        )
        .await
        .expect("generic create with nested properties must succeed");

    assert_eq!(
        created["properties"]["priority"].as_str(),
        Some("p1"),
        "nested properties.priority must be preserved, not overwritten with default p2; got {created:?}"
    );
    let deps = created["properties"]["depends_on"]
        .as_array()
        .expect("depends_on must be an array");
    assert_eq!(
        deps.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>(),
        vec![blocker_full.as_str()],
        "nested depends_on must be canonicalized to hyphenated UUID form; got {created:?}"
    );

    let dep_uuid = uuid::Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let blocker_uuid = uuid::Uuid::parse_str(&blocker_full).unwrap();
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
        "generic create with nested depends_on must also create the graph edge; got targets {targets:?}"
    );
}

#[tokio::test]
async fn create_task_top_level_priority_wins_over_nested_priority() {
    let pack = pack(rt());

    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "conflicting priority",
                "priority": "p0",
                "properties": {"priority": "p3"}
            }),
        )
        .await
        .expect("generic create must succeed");

    assert_eq!(
        created["properties"]["priority"].as_str(),
        Some("p0"),
        "top-level priority must win over nested properties.priority when both are present; got {created:?}"
    );
}

#[tokio::test]
async fn create_task_rejects_nested_depends_on_non_task_without_persisting() {
    use khive_storage::types::PageRequest;

    let rt = rt();
    let pack = pack(rt.clone());

    let entity = pack
        .dispatch("create", json!({"kind": "concept", "name": "Not A Task"}))
        .await
        .expect("entity create must succeed");
    let entity_id = entity["id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "bad nested dep",
                "properties": {"depends_on": [entity_id]}
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("must be a task note"),
        "expected rejection of non-task nested depends_on target; got: {msg}"
    );

    let local_token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let notes = rt.notes(&local_token).expect("note store");
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
        "rejected generic create must not persist a task; found {:?}",
        task_page
            .items
            .iter()
            .filter_map(|n| n.name.clone())
            .collect::<Vec<_>>()
    );
}

// ---- depends_on and context_entity_id must be primary-only -------------------

/// A task in a visible (non-primary) namespace must be treated as NotFound
/// when referenced as a `depends_on` target.
///
/// This is a direct runtime-layer test: `resolve_primary` must return None
/// for a foreign-visible note, while `resolve` (visible-aware) returns Some.
/// The distinction is what the fixed code path relies on.
#[tokio::test]
async fn resolve_primary_rejects_visible_only_task() {
    use khive_runtime::{KhiveRuntime, Namespace};

    let rt = KhiveRuntime::memory().unwrap();

    let ns_primary = Namespace::parse("dep-primary-ns").unwrap();
    let ns_foreign = Namespace::parse("dep-foreign-ns").unwrap();

    // Create a task in the foreign namespace.
    let tok_foreign = rt.authorize(ns_foreign.clone()).unwrap();
    let foreign_task = rt
        .create_note(
            &tok_foreign,
            "task",
            Some("foreign blocker"),
            "foreign blocker",
            Some(0.5),
            Some(serde_json::json!({"status": "inbox", "priority": "p2"})),
            vec![],
        )
        .await
        .unwrap();

    // Build a visible-set token: primary-ns can see foreign-ns.
    let tok_vis = rt
        .authorize_with_visibility(ns_primary.clone(), vec![ns_foreign.clone()])
        .unwrap();

    // resolve (visible-aware) finds the foreign task.
    let resolved_visible = rt.resolve(&tok_vis, foreign_task.id).await.unwrap();
    assert!(
        resolved_visible.is_some(),
        "visible-aware resolve must find the foreign task"
    );

    // resolve_primary must NOT find it (foreign namespace).
    let resolved_primary = rt.resolve_primary(&tok_vis, foreign_task.id).await.unwrap();
    assert!(
        resolved_primary.is_none(),
        "resolve_primary must return None for a visible-only task; \
         the depends_on validator uses resolve_primary and must reject it as NotFound"
    );
}

/// A KG entity in a visible (non-primary) namespace must be treated as NotFound
/// by `resolve_primary`, which is what `context_entity_id` validation now uses.
#[tokio::test]
async fn resolve_primary_rejects_visible_only_entity() {
    use khive_runtime::{KhiveRuntime, Namespace};

    let rt = KhiveRuntime::memory().unwrap();

    let ns_primary = Namespace::parse("ctx-dep-primary-ns").unwrap();
    let ns_foreign = Namespace::parse("ctx-dep-foreign-ns").unwrap();

    let tok_foreign = rt.authorize(ns_foreign.clone()).unwrap();
    let foreign_entity = rt
        .create_entity(
            &tok_foreign,
            "concept",
            None,
            "Foreign Concept",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();

    let tok_vis = rt
        .authorize_with_visibility(ns_primary.clone(), vec![ns_foreign.clone()])
        .unwrap();

    // resolve (visible-aware) finds the foreign entity.
    let resolved_visible = rt.resolve(&tok_vis, foreign_entity.id).await.unwrap();
    assert!(
        resolved_visible.is_some(),
        "visible-aware resolve must find the foreign entity"
    );

    // resolve_primary must NOT find it — context_entity_id validation uses this.
    let resolved_primary = rt
        .resolve_primary(&tok_vis, foreign_entity.id)
        .await
        .unwrap();
    assert!(
        resolved_primary.is_none(),
        "resolve_primary must return None for a visible-only entity; \
         context_entity_id validation uses resolve_primary and must reject it as NotFound"
    );
}

/// Documents current KG-create-path behavior for a visible-only `depends_on`
/// target — NOT a discriminating regression test for the F2 `resolve` vs
/// `resolve_primary` fix. The KG create dispatch only forwards the token's
/// primary namespace string to `TaskHook` (`khive-pack-kg/src/handlers/create.rs`
/// builds `args["namespace"]` from `token.namespace()` alone, discarding any
/// wider visible set), and `TaskHook::prepare_create` always re-derives its
/// own token via `runtime.authorize(ns)`, which mints a primary-namespace-only
/// token (`KhiveRuntime::authorize` -> `mint_with_visibility(ns, vec![], ..)`).
/// So `TaskHook` can never hold a token that sees a foreign namespace on this
/// path today, and `resolve`/`resolve_primary` are indistinguishable here —
/// this test would pass identically with the F2 fix reverted. It is kept to
/// pin the current (safe) end-to-end behavior — reject + persist nothing —
/// as defensive parity with `gtd.assign`. The test that actually proves the
/// `resolve_primary` fix matters is `resolve_primary_rejects_visible_only_task`
/// above, which hand-builds a widened (`authorize_with_visibility`) token and
/// shows `resolve` finds the foreign task while `resolve_primary` does not —
/// the exact distinction `TaskHook`'s dependency validator now relies on.
#[tokio::test]
async fn create_task_with_visible_only_dependency_is_rejected_and_persists_no_local_task() {
    use khive_pack_gtd::GtdPack;
    use khive_pack_kg::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use khive_storage::types::PageRequest;

    let rt = KhiveRuntime::memory().unwrap();

    let ns_foreign = Namespace::parse("dep-foreign-ns-create").unwrap();
    let tok_foreign = rt.authorize(ns_foreign.clone()).unwrap();
    let foreign_task = rt
        .create_note(
            &tok_foreign,
            "task",
            Some("foreign blocker"),
            "foreign blocker",
            Some(0.5),
            Some(json!({"status": "inbox", "priority": "p2"})),
            vec![],
        )
        .await
        .expect("create foreign blocker task");

    let tok_visible = rt
        .authorize_with_visibility(Namespace::local(), vec![ns_foreign.clone()])
        .unwrap();
    assert!(
        rt.resolve(&tok_visible, foreign_task.id)
            .await
            .unwrap()
            .is_some(),
        "visible-aware resolve must find the foreign task"
    );
    assert!(
        rt.resolve_primary(&tok_visible, foreign_task.id)
            .await
            .unwrap()
            .is_none(),
        "resolve_primary must not find the foreign task"
    );

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    builder.with_visible_namespaces(vec![ns_foreign.clone()]);
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());

    let foreign_full = foreign_task.id.as_hyphenated().to_string();
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "local dependent",
                "depends_on": [foreign_full]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, khive_runtime::RuntimeError::NotFound(_))
            || msg.contains("not found in namespace"),
        "visible-only depends_on target must be rejected as NotFound; got: {msg}"
    );

    let local_token = rt.authorize(Namespace::local()).unwrap();
    let notes = rt.notes(&local_token).expect("note store");
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
        .expect("query local task notes");
    assert!(
        task_page.items.is_empty(),
        "rejected create must not persist a local task; found {:?}",
        task_page
            .items
            .iter()
            .filter_map(|n| n.name.clone())
            .collect::<Vec<_>>()
    );
}
