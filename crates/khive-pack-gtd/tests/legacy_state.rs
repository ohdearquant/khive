mod common;
use common::*;

use khive_pack_gtd::handlers::{prepare_complete, prepare_transition};
use khive_pack_gtd::schema::TASK_STATUSES;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError};
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

async fn seed(runtime: &KhiveRuntime, properties: Value, created_at: i64) -> Note {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut note = Note::new("local", "task", "private legacy fixture");
    note.name = Some("private legacy fixture".into());
    note.properties = Some(properties);
    note.created_at = created_at;
    note.updated_at = created_at;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    note
}

#[tokio::test]
async fn legacy_state_default_filter_excludes_unknown_strings_before_pagination() {
    let runtime = rt();
    let pack = pack(runtime.clone());
    let older = seed(&runtime, json!({"status": "waiting"}), 1).await;
    let newer = seed(&runtime, json!({"status": "next"}), 2).await;
    for (index, status) in ["archived", "unknown", "todo", "", "done", "cancelled"]
        .into_iter()
        .enumerate()
    {
        seed(&runtime, json!({"status": status}), 10 + index as i64).await;
    }
    for (offset, expected) in [(0, newer.id), (1, older.id)] {
        let page = pack
            .dispatch("gtd.tasks", json!({"limit": 1, "offset": offset}))
            .await
            .unwrap();
        assert_eq!(page.as_array().unwrap().len(), 1);
        assert_eq!(page[0]["full_id"], expected.to_string());
    }
    for status in ["done", "cancelled"] {
        let page = pack
            .dispatch("gtd.tasks", json!({"status": status}))
            .await
            .unwrap();
        assert_eq!(page.as_array().unwrap().len(), 1);
        assert_eq!(page[0]["status"], status);
    }
    let next = pack.dispatch("gtd.next", json!({})).await.unwrap();
    assert_eq!(next.as_array().unwrap().len(), 1);
    assert_eq!(next[0]["full_id"], newer.id.to_string());
    assert!(pack
        .dispatch("gtd.tasks", json!({"status": "archived"}))
        .await
        .is_err());
}

#[tokio::test]
async fn legacy_state_default_filter_preserves_non_text_inbox_fallback() {
    let runtime = rt();
    let pack = pack(runtime.clone());
    for (index, properties) in [
        json!({}),
        json!({"status": null}),
        json!({"status": false}),
        json!({"status": 4}),
        json!({"status": []}),
        json!({"status": {}}),
    ]
    .into_iter()
    .enumerate()
    {
        seed(&runtime, properties, index as i64 + 1).await;
    }
    let default = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    let inbox = pack
        .dispatch("gtd.tasks", json!({"status": "inbox"}))
        .await
        .unwrap();
    assert_eq!(default, inbox);
    assert_eq!(default.as_array().unwrap().len(), 6);
    assert!(default
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["status"] == "inbox"));
}

#[tokio::test]
async fn legacy_state_refusal_preserves_history_on_canonical_and_atomic_prepare_paths() {
    let runtime = rt();
    let pack = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let legacy = seed(
        &runtime,
        json!({
            "status": "archived", "archived_at": 1_772_323_200_000_i64,
            "transition_history": [{"from": "active", "to": "archived"}],
        }),
        1_772_323_200,
    )
    .await;
    let id = legacy.id.to_string();
    for verb in ["gtd.transition", "gtd.complete"] {
        let error = pack
            .dispatch(verb, json!({"id": id, "status": "cancelled"}))
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error
            .to_string()
            .contains("invalid stored status \"archived\""));
    }
    let transition_error = prepare_transition(
        &runtime,
        &token,
        &id,
        "cancelled",
        None,
        khive_pack_gtd::handlers::DependencyOptions::default(),
    )
    .await
    .err()
    .expect("atomic transition preparation rejects invalid stored state");
    let complete_error = prepare_complete(
        &runtime,
        &token,
        &id,
        Some("cancelled"),
        None,
        khive_pack_gtd::handlers::DependencyOptions::default(),
    )
    .await
    .err()
    .expect("atomic completion preparation rejects invalid stored state");
    for error in [transition_error, complete_error] {
        assert!(error
            .to_string()
            .contains("invalid stored status \"archived\""));
    }
    let persisted = runtime
        .notes(&token)
        .unwrap()
        .get_note(legacy.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted, legacy);
    let hidden = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_eq!(hidden["tasks"], json!([]));
    assert!(hidden["filter_excluded"]
        .as_array()
        .unwrap()
        .contains(&json!("unrecognized_status")));
    assert_eq!(
        pack.dispatch("get", json!({"id": id})).await.unwrap()["properties"]["status"],
        "archived"
    );
    assert_eq!(
        pack.dispatch("gtd.tasks", json!({"assignee": "unrelated"}))
            .await
            .unwrap(),
        json!([])
    );
}

#[tokio::test]
async fn legacy_state_blockers_are_broken_not_pending_work() {
    let runtime = rt();
    let pack = pack(runtime.clone());
    let legacy = seed(&runtime, json!({"status": "archived"}), 1).await;
    let dependent = assign(
        &pack,
        json!({
            "title": "dependent", "status": "next", "depends_on": [legacy.id.to_string()],
        }),
    )
    .await;
    assert_eq!(
        pack.dispatch("gtd.next", json!({})).await.unwrap(),
        json!([])
    );
    let page = pack
        .dispatch("gtd.next", json!({"include_blocked": true}))
        .await
        .unwrap();
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(page[0]["full_id"], dependent["full_id"]);
    assert_eq!(page[0]["dependency_state"], "broken");
    assert_eq!(page[0]["actionable"], false);
    assert_eq!(page[0]["blocked_by"][0]["state"], "invalid");
    assert_eq!(page[0]["blocked_by"][0]["status"], "archived");
}

#[tokio::test]
async fn legacy_state_census_public_writes_store_only_canonical_statuses() {
    let runtime = rt();
    let pack = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut ids = Vec::new();
    for verb in ["gtd.assign", "create"] {
        for status in [
            "inbox",
            "next",
            "waiting",
            "someday",
            "active",
            "todo",
            "in_progress",
            "blocked",
            "later",
        ] {
            let mut args = json!({"title": "canonical state census", "status": status});
            if verb == "create" {
                args["kind"] = json!("task");
            }
            let created = pack.dispatch(verb, args).await.unwrap();
            // `gtd.assign` answers with `full_id`; the generic `create` verb
            // answers with the record's own `id`. Both are full UUIDs.
            let created_id = created
                .get("full_id")
                .or_else(|| created.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    panic!("{verb} response carries neither full_id nor id: {created}")
                });
            ids.push(
                Uuid::parse_str(created_id).unwrap_or_else(|e| {
                    panic!("{verb} returned a non-UUID id {created_id:?}: {e}")
                }),
            );
        }
        for status in ["archived", "unknown"] {
            let mut args = json!({"title": "invalid state", "status": status});
            if verb == "create" {
                args["kind"] = json!("task");
            }
            assert!(pack.dispatch(verb, args).await.is_err());
            assert!(pack.dispatch("create", json!({
                "kind": "task", "title": "invalid nested state", "properties": {"status": status},
            })).await.is_err());
        }
    }
    pack.dispatch(
        "gtd.transition",
        json!({"id": ids[0], "status": "finished"}),
    )
    .await
    .unwrap();
    pack.dispatch("gtd.complete", json!({"id": ids[1], "status": "cancelled"}))
        .await
        .unwrap();
    for id in &ids {
        assert!(pack
            .dispatch(
                "update",
                json!({"id": id, "properties": {"status": "archived"}})
            )
            .await
            .is_err());
    }
    let notes = runtime
        .notes(&token)
        .unwrap()
        .query_notes_filtered_count_free(
            "local",
            &khive_storage::note::NoteFilter {
                kind: Some("task".into()),
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 200,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(
        notes.len(),
        ids.len(),
        "rejected creates must not leave rows"
    );
    let mut observed = std::collections::BTreeSet::new();
    for note in notes {
        let status = note.properties.as_ref().unwrap()["status"]
            .as_str()
            .unwrap();
        assert!(TASK_STATUSES.contains(&status), "{}: {status:?}", note.id);
        observed.insert(status.to_string());
    }
    assert_eq!(
        observed,
        TASK_STATUSES
            .iter()
            .map(|status| status.to_string())
            .collect()
    );
}
#[tokio::test]
async fn tasks_priority_filter_preserves_non_text_p2_fallback() {
    let config = khive_runtime::RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_runtime::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_config: None,
        volume_lock_dir: None,
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        visibility_receipts: None,
        credentials: Vec::new(),
        mounts: Vec::new(),
        events_split: None,
        blob: Default::default(),
        packs: vec!["kg".into(), "gtd".into()],
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).unwrap();
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let pack = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).unwrap();

    // Seed historical values below current write validation; reads must not
    // repair the rows or disagree with their existing projected priority.
    let mut fallback = Vec::new();
    for (index, priority) in [
        None,
        Some(Value::Null),
        Some(json!(false)),
        Some(json!(true)),
        Some(json!(4)),
        Some(json!(4.5)),
        Some(json!([])),
        Some(json!({})),
        Some(json!("p2")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut properties = json!({"status": "inbox"});
        if let Some(priority) = priority {
            properties["priority"] = priority;
        }
        fallback.push(seed(&runtime, properties, index as i64 + 1).await);
    }
    let mut controls = Vec::new();
    // Newer nonmatches expose filtering after LIMIT/OFFSET, while unknown
    // text must retain its original projection instead of joining `p2`.
    for (index, priority) in ["p0", "p1", "p3", "unexpected", "", "P2"]
        .into_iter()
        .enumerate()
    {
        controls.push(
            seed(
                &runtime,
                json!({"status": "inbox", "priority": priority}),
                index as i64 + 100,
            )
            .await,
        );
    }
    let all = pack
        .dispatch("gtd.tasks", json!({"limit": 200}))
        .await
        .unwrap();
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 15);
    for note in fallback.iter().chain(&controls) {
        let row = all
            .iter()
            .find(|row| row["full_id"] == note.id.to_string())
            .unwrap();
        assert_eq!(&row["properties"], note.properties.as_ref().unwrap());
    }
    let expected: Vec<Value> = all
        .iter()
        .filter(|row| row["priority"] == "p2")
        .cloned()
        .collect();
    let expected_ids: Vec<String> = fallback
        .iter()
        .rev()
        .map(|note| note.id.to_string())
        .collect();
    let actual_ids: Vec<String> = expected
        .iter()
        .map(|row| row["full_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(actual_ids, expected_ids);
    assert_eq!(expected.len(), 9);
    for priority in ["p2", "P2"] {
        let filtered = pack
            .dispatch("gtd.tasks", json!({"priority": priority, "limit": 200}))
            .await
            .unwrap();
        assert_eq!(filtered, json!(expected));
    }
    for (offset, row) in expected.iter().enumerate() {
        let page = pack
            .dispatch(
                "gtd.tasks",
                json!({"priority": "p2", "limit": 1, "offset": offset}),
            )
            .await
            .unwrap();
        assert_eq!(page, json!([row]));
    }
    assert_eq!(
        pack.dispatch(
            "gtd.tasks",
            json!({"priority": "p2", "limit": 1, "offset": expected.len()}),
        )
        .await
        .unwrap(),
        json!([])
    );
    for (priority, note) in ["p0", "p1", "p3"].into_iter().zip(&controls) {
        let page = pack
            .dispatch("gtd.tasks", json!({"priority": priority}))
            .await
            .unwrap();
        assert_eq!(page.as_array().unwrap().len(), 1);
        assert_eq!(page[0]["full_id"], note.id.to_string());
        assert_eq!(page[0]["priority"], priority);
    }
    assert!(matches!(
        pack.dispatch("gtd.tasks", json!({"priority": "unexpected"}))
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
    for note in fallback.iter().chain(&controls) {
        let persisted = runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&persisted, note);
    }
}
