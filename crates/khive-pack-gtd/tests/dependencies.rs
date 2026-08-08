//! Tests for `depends_on` task dependencies, context entity, and blocker semantics.

mod common;

use common::{assign, pack, rt};
use serde_json::{json, Value};

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

    let diagnostic = pack
        .dispatch("gtd.tasks", json!({"status": "next"}))
        .await
        .expect("diagnostic task listing");
    let dependent_diagnostic = diagnostic
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["full_id"].as_str() == Some(dep_id.as_str()))
        .expect("dependent task diagnostic");
    assert_eq!(dependent_diagnostic["dependency_state"], "blocked");
    assert_eq!(dependent_diagnostic["actionable"], false);
    assert_eq!(dependent_diagnostic["blocked_by"][0]["state"], "pending");
    assert_eq!(dependent_diagnostic["blocked_by"][0]["status"], "inbox");

    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !titles.contains(&"dependent-task"),
        "next() must not return tasks with incomplete deps; got: {titles:?}"
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
        "after blocker is done, next() must include dependent task; got: {titles2:?}"
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
        "task with no deps must appear in next(); got: {titles:?}"
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
        "task with all deps done must appear in next(); got: {titles:?}"
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

#[tokio::test]
async fn assign_rejects_context_prefix_with_resolution_consequence() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "ambiguous context", "context_entity_id": "deadbeef"}),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("primary-namespace resolution"),
        "error must explain what a short prefix means; got: {msg}"
    );
    assert!(
        msg.contains("explicit stable entity reference"),
        "error must explain why this field requires direct identity; got: {msg}"
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
async fn create_task_treats_null_annotates_as_absent_with_context_entity_id() {
    let pack = pack(rt());

    let context = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Null annotates context"}),
        )
        .await
        .unwrap();
    let context_id = context["id"].as_str().unwrap().to_string();

    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "null annotates with context",
                "annotates": null,
                "context_entity_id": context_id.clone(),
            }),
        )
        .await
        .expect("null annotates must be absent when the context adds an annotation");
    assert_eq!(
        created["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let task_id = created["id"].as_str().unwrap();
    let annotations = pack
        .dispatch(
            "neighbors",
            json!({"id": task_id, "direction": "out", "relations": ["annotates"]}),
        )
        .await
        .expect("context-derived annotates edge must be persisted");
    assert!(
        annotations
            .as_array()
            .unwrap()
            .iter()
            .any(|neighbor| neighbor.to_string().contains(context_id.as_str())),
        "task must annotate its context entity; got {annotations:?}"
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

#[tokio::test]
async fn generic_create_rejects_malformed_raw_kind_discriminants_before_write() {
    let pack = pack(rt());
    let cases = vec![
        (
            "substrate note_kind type",
            "note_kind",
            json!({"kind": "note", "note_kind": 17, "content": "must not become an observation"}),
        ),
        (
            "substrate entity_kind type",
            "entity_kind",
            json!({"kind": "entity", "entity_kind": 17, "name": "must not persist"}),
        ),
        (
            "granular note_kind type",
            "note_kind",
            json!({"kind": "task", "note_kind": 17, "title": "must not persist"}),
        ),
        (
            "granular entity_kind type",
            "entity_kind",
            json!({"kind": "concept", "entity_kind": 17, "name": "must not persist"}),
        ),
        (
            "irrelevant entity_kind type",
            "entity_kind",
            json!({"kind": "task", "entity_kind": false, "title": "must not persist"}),
        ),
        (
            "irrelevant note_kind type",
            "note_kind",
            json!({"kind": "concept", "note_kind": [], "name": "must not persist"}),
        ),
        (
            "kind type",
            "kind",
            json!({"kind": 17, "content": "must not persist"}),
        ),
        (
            "kind null",
            "kind",
            json!({"kind": null, "content": "must not persist"}),
        ),
        (
            "substrate empty note_kind",
            "note_kind",
            json!({"kind": "note", "note_kind": "", "content": "must not persist"}),
        ),
        (
            "substrate empty entity_kind",
            "entity_kind",
            json!({"kind": "entity", "entity_kind": "  ", "name": "must not persist"}),
        ),
        (
            "granular empty note_kind",
            "note_kind",
            json!({"kind": "task", "note_kind": "", "title": "must not persist"}),
        ),
        (
            "granular empty entity_kind",
            "entity_kind",
            json!({"kind": "concept", "entity_kind": "\t", "name": "must not persist"}),
        ),
    ];

    for (case, field, args) in cases {
        let err = match pack.dispatch("create", args).await {
            Err(err) => err,
            Ok(value) => panic!("{case} must fail, but created {value:?}"),
        };
        assert!(
            err.to_string().contains(field),
            "{case} error must name `{field}`; got {err}"
        );
    }

    let stats = pack
        .dispatch("stats", json!({}))
        .await
        .expect("stats after rejected creates");
    assert_eq!(stats["entities"].as_u64(), Some(0));
    assert_eq!(stats["notes"].as_u64(), Some(0));
}

#[tokio::test]
async fn generic_task_create_rejects_wrong_typed_optional_fields_before_write() {
    use khive_storage::types::PageRequest;

    let rt = rt();
    let pack = pack(rt.clone());
    let top_level_cases: Vec<(&str, Value)> = vec![
        ("title", json!(17)),
        ("name", json!(17)),
        ("content", json!(["not", "text"])),
        ("description", json!(1)),
        ("assignee", json!(["agent"])),
        ("priority", json!(1)),
        ("status", json!(true)),
        ("due", json!({"date": "2026-08-06"})),
        ("start", json!(1)),
        ("end", json!([])),
        ("depends_on", json!("not-an-array")),
        ("context_entity_id", json!(17)),
        ("tags", json!("not-an-array")),
        ("salience", json!("high")),
        ("annotates", json!("not-an-array")),
        ("entity_type", json!(17)),
        ("skip_dedup_check", json!("yes")),
        ("edges", json!("not-an-array")),
        ("embedding_content", json!(17)),
        ("properties", json!("not-an-object")),
    ];

    for (field, bad_value) in top_level_cases {
        let mut args = json!({
            "kind": "note",
            "note_kind": "task",
            "title": format!("bad top-level {field}"),
        });
        args.as_object_mut()
            .expect("create args object")
            .insert(field.to_string(), bad_value);
        let err = pack
            .dispatch("create", args)
            .await
            .expect_err("wrong-typed optional task field must be rejected");
        let message = err.to_string();
        if [
            "name",
            "content",
            "description",
            "tags",
            "salience",
            "annotates",
            "entity_type",
            "skip_dedup_check",
            "edges",
            "embedding_content",
        ]
        .contains(&field)
        {
            assert!(
                message.contains("bad params"),
                "shared CreateParams must reject {field}; got: {message}"
            );
        } else {
            assert!(
                message.contains(field),
                "error must name {field}; got: {message}"
            );
        }
    }

    let err = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": 17,
                "name": "must not hide malformed title",
            }),
        )
        .await
        .expect_err("a valid fallback must not hide a malformed present title");
    assert!(
        err.to_string().contains("title"),
        "strict parser must report malformed title before fallback; got: {err}"
    );

    let nested_cases: Vec<(&str, Value)> = vec![
        ("description", json!(1)),
        ("assignee", json!(["agent"])),
        ("priority", json!(1)),
        ("status", json!(true)),
        ("due", json!({"date": "2026-08-06"})),
        ("start", json!(1)),
        ("end", json!([])),
        ("depends_on", json!("not-an-array")),
        ("context_entity_id", json!(17)),
        ("tags", json!("not-an-array")),
    ];
    for (field, bad_value) in nested_cases {
        let mut properties = serde_json::Map::new();
        properties.insert(field.to_string(), bad_value);
        let err = pack
            .dispatch(
                "create",
                json!({
                    "kind": "note",
                    "note_kind": "task",
                    "title": format!("bad nested {field}"),
                    "properties": properties,
                }),
            )
            .await
            .expect_err("wrong-typed nested task field must be rejected");
        assert!(
            err.to_string().contains(field),
            "error must name properties.{field}; got: {err}"
        );
    }

    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let tasks = rt
        .notes(&token)
        .expect("note store")
        .query_notes(
            "local",
            Some("task"),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .expect("task query");
    assert!(
        tasks.items.is_empty(),
        "no malformed generic task create may persist a note"
    );
}

#[tokio::test]
async fn generic_task_create_treats_null_as_absence_and_falls_back_to_name() {
    let pack = pack(rt());
    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "task",
                "entity_kind": null,
                "note_kind": null,
                "title": null,
                "name": "name fallback",
                "content": null,
                "description": null,
                "assignee": null,
                "priority": null,
                "status": null,
                "due": null,
                "start": null,
                "end": null,
                "depends_on": null,
                "context_entity_id": null,
                "tags": null,
                "salience": null,
                "annotates": null,
                "properties": null,
            }),
        )
        .await
        .expect("JSON null must retain optional-field semantics");
    assert_eq!(created["name"], "name fallback");
    assert_eq!(created["content"], "name fallback");
    assert_eq!(created["properties"]["status"], "inbox");
    assert_eq!(created["properties"]["priority"], "p2");
    assert!(created["properties"].get("description").is_none());

    let nested = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "nested nulls",
                "properties": {
                    "description": null,
                    "assignee": null,
                    "priority": null,
                    "status": null,
                    "due": null,
                    "start": null,
                    "end": null,
                    "depends_on": null,
                    "context_entity_id": null,
                    "tags": null,
                },
            }),
        )
        .await
        .expect("nested optional nulls must also be accepted");
    assert_eq!(nested["content"], "nested nulls");
    assert_eq!(nested["properties"]["status"], "inbox");
    assert_eq!(nested["properties"]["priority"], "p2");
    assert!(nested["properties"].get("description").is_none());

    let err = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": null,
                "name": null,
            }),
        )
        .await
        .expect_err("both task title spellings absent must be rejected");
    assert!(
        err.to_string().contains("title") && err.to_string().contains("name"),
        "missing-title error must explain both accepted spellings; got: {err}"
    );
}

#[tokio::test]
async fn generic_task_update_keeps_content_and_description_synchronized() {
    let pack = pack(rt());
    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "sync task",
                "description": "original body",
            }),
        )
        .await
        .expect("task create");
    let id = created["id"].as_str().expect("task id").to_string();

    let content_update = pack
        .dispatch("update", json!({"id": id, "content": "body from content"}))
        .await
        .expect("content update");
    assert_eq!(content_update["content"], "body from content");
    assert_eq!(
        content_update["properties"]["description"],
        "body from content"
    );

    let description_update = pack
        .dispatch(
            "update",
            json!({"id": id, "properties": {"description": "body from property"}}),
        )
        .await
        .expect("description update");
    assert_eq!(description_update["content"], "body from property");
    assert_eq!(
        description_update["properties"]["description"],
        "body from property"
    );

    let null_noop = pack
        .dispatch(
            "update",
            json!({"id": id, "content": null, "properties": null}),
        )
        .await
        .expect("null generic note patches mean leave unchanged");
    assert_eq!(null_noop["content"], "body from property");
    assert_eq!(null_noop["properties"]["description"], "body from property");

    let cleared = pack
        .dispatch(
            "update",
            json!({"id": id, "properties": {"description": null}}),
        )
        .await
        .expect("explicit description clear");
    assert_eq!(cleared["content"], "sync task");
    assert!(cleared["properties"]["description"].is_null());

    let err = pack
        .dispatch(
            "update",
            json!({
                "id": id,
                "content": "one body",
                "properties": {"description": "another body"},
            }),
        )
        .await
        .expect_err("conflicting mirrors must be rejected");
    assert!(
        err.to_string().contains("must match"),
        "conflict error must explain the mirror contract; got: {err}"
    );
}

#[tokio::test]
async fn generic_task_update_rejects_title_clear_before_description_clear_can_write() {
    let runtime = rt();
    let pack = pack(runtime.clone());
    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "task",
                "title": "preserved title",
                "description": "preserved body",
            }),
        )
        .await
        .expect("task create");
    let id = uuid::Uuid::parse_str(created["id"].as_str().expect("task id")).expect("task UUID");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(id)
        .await
        .expect("read task before rejected update")
        .expect("task exists");

    let err = pack
        .dispatch(
            "update",
            json!({
                "id": id.to_string(),
                "name": null,
                "properties": {"description": null},
            }),
        )
        .await
        .expect_err("a task title cannot be cleared");
    assert!(
        err.to_string().contains("task title cannot be cleared"),
        "title-clear error must identify the task invariant; got: {err}"
    );

    let after = runtime
        .notes(&token)
        .expect("note store")
        .get_note(id)
        .await
        .expect("read task after rejected update")
        .expect("task exists");
    assert_eq!(after, before, "rejected normalization must not write");
}

#[tokio::test]
async fn renaming_task_without_description_updates_fallback_content() {
    let pack = pack(rt());
    let created = pack
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "task", "title": "old title"}),
        )
        .await
        .expect("task create");
    let id = created["id"].as_str().expect("task id");

    let updated = pack
        .dispatch("update", json!({"id": id, "name": "new title"}))
        .await
        .expect("task rename");
    assert_eq!(updated["name"], "new title");
    assert_eq!(updated["content"], "new title");
    assert!(updated["properties"].get("description").is_none());
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

#[tokio::test]
async fn dependency_diagnostics_surface_cancelled_soft_deleted_and_missing_blockers() {
    let fixture = pack(rt());

    let cancelled = assign(
        &fixture,
        json!({"title": "cancelled blocker", "status": "inbox"}),
    )
    .await;
    let soft_deleted = assign(
        &fixture,
        json!({"title": "soft-deleted blocker", "status": "inbox"}),
    )
    .await;
    let hard_deleted = assign(
        &fixture,
        json!({"title": "hard-deleted blocker", "status": "inbox"}),
    )
    .await;

    for (title, blocker) in [
        ("blocked by cancelled", &cancelled),
        ("blocked by soft delete", &soft_deleted),
        ("blocked by hard delete", &hard_deleted),
    ] {
        assign(
            &fixture,
            json!({
                "title": title,
                "status": "next",
                "depends_on": [blocker["full_id"].as_str().unwrap()]
            }),
        )
        .await;
    }

    fixture
        .dispatch(
            "gtd.transition",
            json!({"id": cancelled["full_id"], "status": "cancelled"}),
        )
        .await
        .expect("cancel blocker");
    fixture
        .dispatch("delete", json!({"id": soft_deleted["full_id"]}))
        .await
        .expect("soft-delete blocker");
    fixture
        .dispatch(
            "delete",
            json!({"id": hard_deleted["full_id"], "hard": true}),
        )
        .await
        .expect("hard-delete blocker");

    let tasks = fixture
        .dispatch("gtd.tasks", json!({"status": "next"}))
        .await
        .expect("diagnostic task listing");
    let tasks = tasks.as_array().expect("tasks array");
    for (title, blocker_state) in [
        ("blocked by cancelled", "cancelled"),
        ("blocked by soft delete", "soft_deleted"),
        ("blocked by hard delete", "missing"),
    ] {
        let task = tasks
            .iter()
            .find(|task| task["title"].as_str() == Some(title))
            .unwrap_or_else(|| panic!("missing diagnostic task {title:?}: {tasks:?}"));
        assert_eq!(task["dependency_state"], "broken");
        assert_eq!(task["actionable"], false);
        assert_eq!(task["blocked_by"][0]["state"], blocker_state);
    }

    let default_next = fixture
        .dispatch("gtd.next", json!({}))
        .await
        .expect("default next");
    assert!(default_next.as_array().unwrap().is_empty());

    let diagnostic_next = fixture
        .dispatch("gtd.next", json!({"include_blocked": true}))
        .await
        .expect("diagnostic next");
    assert_eq!(diagnostic_next.as_array().unwrap().len(), 3);
    assert!(diagnostic_next
        .as_array()
        .unwrap()
        .iter()
        .all(|task| { task["dependency_state"] == "broken" && task["actionable"] == false }));
}

#[tokio::test]
async fn dependency_diagnostics_surface_invalid_different_namespace_and_wrong_kind_blockers() {
    let rt = rt();
    let fixture = pack(rt.clone());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let store = rt.notes(&token).expect("note store");

    let wrong_kind_blocker = assign(
        &fixture,
        json!({"title": "wrong-kind blocker", "status": "inbox"}),
    )
    .await;
    let namespace_blocker = assign(
        &fixture,
        json!({"title": "namespace blocker", "status": "inbox"}),
    )
    .await;

    for (title, blocker) in [
        ("blocked by wrong kind", &wrong_kind_blocker),
        ("blocked by different namespace", &namespace_blocker),
    ] {
        assign(
            &fixture,
            json!({
                "title": title,
                "status": "next",
                "depends_on": [blocker["full_id"].as_str().unwrap()]
            }),
        )
        .await;
    }

    let invalid_dependent = assign(
        &fixture,
        json!({"title": "blocked by invalid entry", "status": "next"}),
    )
    .await;

    // Corrupt each blocker's stored row directly through the note store,
    // bypassing the pack-level write-time validation that would otherwise
    // reject these shapes — read-time diagnostics must still catch them.
    let wrong_kind_id =
        uuid::Uuid::parse_str(wrong_kind_blocker["full_id"].as_str().unwrap()).unwrap();
    let mut corrupted_kind = store
        .get_note(wrong_kind_id)
        .await
        .expect("fetch blocker")
        .expect("blocker exists");
    corrupted_kind.kind = "observation".to_string();
    store
        .upsert_note(corrupted_kind)
        .await
        .expect("corrupt blocker kind");

    let namespace_id =
        uuid::Uuid::parse_str(namespace_blocker["full_id"].as_str().unwrap()).unwrap();
    let mut corrupted_namespace = store
        .get_note(namespace_id)
        .await
        .expect("fetch blocker")
        .expect("blocker exists");
    corrupted_namespace.namespace = "other".to_string();
    store
        .upsert_note(corrupted_namespace)
        .await
        .expect("corrupt blocker namespace");

    let invalid_id = uuid::Uuid::parse_str(invalid_dependent["full_id"].as_str().unwrap()).unwrap();
    store
        .set_note_property(
            invalid_id,
            "depends_on",
            json!(["not-a-uuid"]),
            chrono::Utc::now().timestamp_micros(),
        )
        .await
        .expect("corrupt dependent depends_on");

    let tasks = fixture
        .dispatch("gtd.tasks", json!({"status": "next"}))
        .await
        .expect("diagnostic task listing");
    let tasks = tasks.as_array().expect("tasks array");
    for (title, blocker_state) in [
        ("blocked by wrong kind", "wrong_kind"),
        ("blocked by different namespace", "different_namespace"),
        ("blocked by invalid entry", "invalid"),
    ] {
        let task = tasks
            .iter()
            .find(|task| task["title"].as_str() == Some(title))
            .unwrap_or_else(|| panic!("missing diagnostic task {title:?}: {tasks:?}"));
        assert_eq!(task["dependency_state"], "broken");
        assert_eq!(task["actionable"], false);
        assert_eq!(task["blocked_by"][0]["state"], blocker_state);
    }
}

#[tokio::test]
async fn generic_update_rejects_direct_and_multihop_property_cycles() {
    let fixture = pack(rt());
    let a = assign(&fixture, json!({"title": "cycle A"})).await;
    let b = assign(&fixture, json!({"title": "cycle B"})).await;
    let c = assign(&fixture, json!({"title": "cycle C"})).await;
    let a_id = a["full_id"].as_str().unwrap();
    let b_id = b["full_id"].as_str().unwrap();
    let c_id = c["full_id"].as_str().unwrap();

    let direct = fixture
        .dispatch(
            "update",
            json!({"id": a_id, "properties": {"depends_on": [a_id]}}),
        )
        .await
        .expect_err("direct property cycle must fail");
    assert!(direct.to_string().contains("dependency cycle"));

    fixture
        .dispatch(
            "update",
            json!({"id": a_id, "properties": {"depends_on": [b_id]}}),
        )
        .await
        .expect("A depends on B");
    fixture
        .dispatch(
            "update",
            json!({"id": b_id, "properties": {"depends_on": [c_id]}}),
        )
        .await
        .expect("B depends on C");
    let multihop = fixture
        .dispatch(
            "update",
            json!({"id": c_id, "properties": {"depends_on": [a_id]}}),
        )
        .await
        .expect_err("C -> A must not close A -> B -> C");
    assert!(multihop.to_string().contains("dependency cycle"));

    let persisted_c = fixture
        .dispatch("get", json!({"id": c_id}))
        .await
        .expect("load C after rejected update");
    assert!(persisted_c["properties"].get("depends_on").is_none());
}

#[tokio::test]
async fn generic_update_rejects_noncanonical_dependency_uuid_without_persisting() {
    let fixture = pack(rt());
    let task = assign(&fixture, json!({"title": "canonical dependency source"})).await;
    let blocker = assign(&fixture, json!({"title": "canonical dependency target"})).await;
    let task_id = task["full_id"].as_str().unwrap();
    let blocker_id = blocker["full_id"].as_str().unwrap();
    let compact_blocker_id = blocker_id.replace('-', "");

    let error = fixture
        .dispatch(
            "update",
            json!({
                "id": task_id,
                "properties": {"depends_on": [compact_blocker_id]}
            }),
        )
        .await
        .expect_err("ordinary update must reject an alternate UUID spelling");
    assert!(
        error
            .to_string()
            .contains("canonical lowercase hyphenated UUID"),
        "unexpected canonical UUID validation error: {error}"
    );

    let persisted = fixture
        .dispatch("get", json!({"id": task_id}))
        .await
        .expect("load task after rejected alternate UUID spelling");
    assert!(persisted["properties"].get("depends_on").is_none());
}

#[tokio::test]
async fn generic_update_rejects_duplicate_dependency_walk_over_edge_budget() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local namespace");
    let leaf = assign(&fixture, json!({"title": "fanout leaf"})).await;
    let leaf_id = leaf["full_id"].as_str().unwrap();
    let repeated_edges = vec![leaf_id.to_string(); 20_001];
    let fanout = runtime
        .create_note(
            &token,
            "task",
            Some("duplicate dependency fanout"),
            "duplicate dependency fanout",
            Some(0.5),
            Some(json!({"status": "next", "depends_on": repeated_edges})),
            vec![],
        )
        .await
        .expect("create legacy task above the typed traversal edge budget");
    let source = assign(&fixture, json!({"title": "bounded traversal source"})).await;

    let error = fixture
        .dispatch(
            "update",
            json!({
                "id": source["full_id"],
                "properties": {"depends_on": [fanout.id.as_hyphenated().to_string()]}
            }),
        )
        .await
        .expect_err("duplicate dependency entries must consume the edge budget");
    assert!(
        error.to_string().contains("20000-edge safety bound"),
        "unexpected bounded-walk error: {error}"
    );
}

#[tokio::test]
async fn link_rejects_multihop_and_same_batch_dependency_cycles() {
    let fixture = pack(rt());
    let a = assign(&fixture, json!({"title": "edge cycle A"})).await;
    let b = assign(&fixture, json!({"title": "edge cycle B"})).await;
    let c = assign(&fixture, json!({"title": "edge cycle C"})).await;
    let a_id = a["full_id"].as_str().unwrap();
    let b_id = b["full_id"].as_str().unwrap();
    let c_id = c["full_id"].as_str().unwrap();

    for (source_id, target_id) in [(a_id, b_id), (b_id, c_id)] {
        fixture
            .dispatch(
                "link",
                json!({
                    "source_id": source_id,
                    "target_id": target_id,
                    "relation": "depends_on"
                }),
            )
            .await
            .expect("acyclic dependency link");
    }
    let multihop = fixture
        .dispatch(
            "link",
            json!({
                "source_id": c_id,
                "target_id": a_id,
                "relation": "depends_on"
            }),
        )
        .await
        .expect_err("C -> A must not close A -> B -> C");
    assert!(multihop.to_string().contains("dependency cycle"));

    let x = assign(&fixture, json!({"title": "batch cycle X"})).await;
    let y = assign(&fixture, json!({"title": "batch cycle Y"})).await;
    let same_batch = fixture
        .dispatch(
            "link",
            json!({
                "links": [
                    {"source_id": x["full_id"], "target_id": y["full_id"], "relation": "depends_on"},
                    {"source_id": y["full_id"], "target_id": x["full_id"], "relation": "depends_on"}
                ]
            }),
        )
        .await
        .expect_err("atomic batch cycle must fail before either edge is written");
    assert!(same_batch.to_string().contains("dependency cycle"));
}

#[tokio::test]
async fn link_cycle_walk_ignores_paths_through_soft_deleted_tasks() {
    let fixture = pack(rt());
    let a = assign(&fixture, json!({"title": "live edge path A"})).await;
    let tombstone = assign(&fixture, json!({"title": "deleted edge path B"})).await;
    let c = assign(&fixture, json!({"title": "live edge path C"})).await;

    for (source, target) in [(&a, &tombstone), (&tombstone, &c)] {
        fixture
            .dispatch(
                "link",
                json!({
                    "source_id": source["full_id"],
                    "target_id": target["full_id"],
                    "relation": "depends_on"
                }),
            )
            .await
            .expect("seed acyclic dependency path");
    }

    fixture
        .dispatch("delete", json!({"id": tombstone["full_id"]}))
        .await
        .expect("soft-delete intermediate task");

    fixture
        .dispatch(
            "link",
            json!({
                "source_id": c["full_id"],
                "target_id": a["full_id"],
                "relation": "depends_on"
            }),
        )
        .await
        .expect("a path through a soft-deleted task is not a live dependency cycle");
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
